//! Per-run PPTX geometry: `p:txBody` → measured lines → [`TextItem`]s.
//!
//! [`super::pptx`] needs only shape rectangles, and PPTX hands those over
//! directly — `<a:off>`/`<a:ext>` are absolute EMU and `pptx::geometry` has
//! already composed group coordinate spaces into `Shape::slide_rect`. **Below
//! the shape there are no coordinates at all.** A paragraph does not say where
//! its lines fall, and a run does not say how wide it is; both are derived by
//! measuring the text and wrapping it inside the rectangle the shape gives it.
//!
//! That derivation is not written here. `render::layout` already does it for
//! DOCX text boxes, and the layout census established that the reusable part
//! reaches further than expected: `stack_blocks` names no `w:` type, and
//! `shape_body::layout_shape_body` — carved out of the DOCX text-box path for
//! this — does insets, anchor and `@vertOverflow` off a plain `a:bodyPr`, which
//! is DrawingML and identical in both formats. So this module is an **adapter**:
//! it turns a `TextParagraph` plus its resolved text style into the
//! `LayoutBlock`s that stack expects, and hands the resulting draw commands to
//! the same `DrawCommand` → `TextItem` converter the DOCX path uses.
//!
//! What is genuinely new here, and what the census said each would cost:
//!
//! | input | why it cannot be assumed |
//! |---|---|
//! | theme font default | **26.3% of runs name no font.** DrawingML's answer is `+mn-lt`/`+mj-lt`; guessing the host default puts a quarter of the corpus in the wrong face *and* the wrong width |
//! | `a:bodyPr` insets | declared on 100% of text shapes, and **47.2% differ from the spec default** |
//! | `a:bodyPr` anchor | **27.7% are not `top`** |
//! | `a:normAutofit` | only **3.4%** actually shrink, but the most aggressive is to **25%** — a body laid out at 4x its intended size is not a subtle error |
//! | shape rotation | **5.3% of text shapes, 148 at a right angle.** The DOCX path hardcodes `rotation = 0.0`, which is honest for a page and is not honest here |
//!
//! Rotation is applied here rather than inside the shared converter because
//! `DrawCommand::Text` carries no rotation field — it is emitted pre-shifted but
//! un-rotated, and the DOCX painter rotates the whole shape instead. Adding one
//! would touch an enum that is matched exhaustively across the vendored crate by
//! design. Each shape is therefore converted on its own and its items are
//! rotated about the shape centre on the way onto the slide.
//!
//! Reading order and the text cascade both come from [`super::pptx`], through
//! `Deck::prepare`, so a `TextItem` is a box for exactly the text the markdown
//! emitter saw, resolved the same way.
//!
//! # Two walks, one prepare
//!
//! Text is emitted in reading order; **fills and outlines are emitted in
//! document order**, by [`paint_shapes`], because §19.3.1.45 makes document
//! order z-order and the paint census measured 29.1% of the corpus pairs where
//! that difference is observable coming out the wrong way round under reading
//! order. The two walks share one `Deck::prepare`, so they cannot disagree
//! about a rectangle or a cascade; what they deliberately do not share is the
//! chrome filter, the traversal order, and the placement mechanism — a text
//! body is bracketed, a path places itself.

use liteparse_ooxml::model::{Alignment, Theme};
use liteparse_ooxml::pptx::{
    self, PresentationPackage, ResolvedTextStyle, Shape, ShapeKind, Spacing, TextBody, TextCascade,
    TextParagraph,
};
use liteparse_ooxml::render::dimension::Pt;
use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::geometry::{PtOffset, PtSize};
use liteparse_ooxml::render::layout::ShapeAutoFit;
use liteparse_ooxml::render::layout::draw_command::{
    DrawCommand, LayoutedPage, ResolvedFill, ShapeTransform, TransformMark,
};
use liteparse_ooxml::render::layout::fragment::{
    Fragment, emit_run_fragments, font_props_from_run,
};
use liteparse_ooxml::render::layout::measurer::TextMeasurer;
use liteparse_ooxml::render::layout::paragraph::{LineSpacingRule, ParagraphStyle};
use liteparse_ooxml::render::layout::section::LayoutBlock;
use liteparse_ooxml::render::layout::shape_body::{layout_shape_body, measure_shape_body};
use liteparse_ooxml::render::resolve::color::RgbColor;
use liteparse_ooxml::render::resolve::fonts::resolve_font_set_themes;
use liteparse_ooxml::render::resolve::shape_geometry::build_geometry;
use liteparse_ooxml::render::resolve::shape_visuals::resolve_shape_visuals;

use std::collections::HashMap;

use crate::error::LiteParseError;
use crate::office::docx_layout;
use crate::office::pptx::{Deck, is_title, reading_order};
use crate::types::{Page, TextItem};

/// EMU per point (§20.1.2.1: 914400 EMU/inch ÷ 72 pt/inch).
const EMU_PER_POINT: f32 = 12700.0;

/// §20.1.10.60 `@rot` is in 60000ths of a degree.
const ANGLE_UNITS_PER_DEGREE: f32 = 60000.0;

/// What a deck's geometry pass produced, plus what it could not place.
///
/// The counters are not diagnostics — they are the honest half of the result.
/// Two content classes carry text that this pass does not yet position, and a
/// consumer that saw only `pages` would read their absence as "the slide had no
/// such text" rather than "this pass cannot place it yet".
pub struct SlideGeometry {
    /// One page per slide, in presentation order. Page count always equals
    /// slide count, so a page index is a slide index.
    pub pages: Vec<Page>,
    /// The same slides as draw commands rather than boxes — one page per
    /// slide, parallel to `pages`, ready for `render::raster::rasterize_page`.
    ///
    /// Not a second derivation: each shape contributes the *same*
    /// `layout_shape_body` output that became its [`TextItem`]s, bracketed by
    /// the `DrawCommand::Transform` that places it on the slide. So a
    /// screenshot and the geometry it would highlight come from one layout, by
    /// construction — which is the property the LibreOffice screenshot path
    /// could never offer.
    pub layouts: Vec<LayoutedPage>,
    /// Per page, the shape each run of items came from. Parallel to `pages`.
    ///
    /// Derived geometry has no external oracle — PPTX declares no coordinate
    /// below the shape, so there is nothing to diff a line box against. The
    /// one check that *is* available is that a shape's text lands inside the
    /// box that shape gave it, and that requires knowing which shape each item
    /// came from. Kept on the result rather than recomputed by a probe because
    /// a probe that re-derived the association would be re-deriving the thing
    /// under test.
    pub placements: Vec<Vec<ShapePlacement>>,
    /// Table cells whose text this pass placed.
    pub placed_table_cells: usize,
    /// Table cells carrying text that could not be placed: a frame with no
    /// rectangle, or a table whose `a:tblGrid` gives its columns no width.
    /// The markdown emitter still emits their text, so this is a geometry gap
    /// for those cells rather than a content drop.
    pub unplaced_table_cells: usize,
    /// SmartArt text bodies. The frame has a rect, but the text lives in
    /// `ppt/diagrams/data*.xml` with its own layout algorithm; the markdown
    /// emitter places the *frame* in reading order, which is a weaker claim
    /// than a per-run box.
    pub unplaced_diagram_bodies: usize,
    /// Shapes the paint walk emitted a [`DrawCommand::Path`] for.
    pub painted_shapes: usize,
    /// Shapes that resolve to a fill or a stroke but whose geometry this pass
    /// cannot build — an unimplemented preset, or none declared. Their text is
    /// still laid out; only their ink is missing.
    pub unpainted_shapes: usize,
    /// Painted shapes whose `<a:ln>` names no colour of its own, so the shared
    /// resolver defaults it to black where DrawingML would inherit
    /// `p:style/a:lnRef`. The census put this at 7.7% of strokes; it is the one
    /// place this pass paints something *wrong* rather than painting nothing,
    /// and it is counted rather than hidden until `p:style` is parsed.
    pub outlines_defaulted_black: usize,
}

/// Where one text body's items landed on its slide — a shape's, or a single
/// table cell's.
pub struct ShapePlacement {
    /// Which of the two layout paths produced this box.
    ///
    /// Not decoration: a table cell's rectangle is *derived* (grid prefix sums,
    /// grown rows) where a shape's is *declared*, so the two have different
    /// failure modes and a check that pooled them would let a cell-only bug —
    /// a dropped `a:tcPr` anchor, say — hide inside 4,938 correct shapes.
    pub kind: PlacementKind,
    /// The box's unrotated rectangle in slide Pt: `(x, y, width, height)`.
    pub rect: (f32, f32, f32, f32),
    /// Counter-clockwise degrees, matching [`TextItem::rotation`].
    pub rotation: f32,
    /// Whether the body's `a:normAutofit` shrinks it (`@fontScale` < 100%).
    pub shrunk: bool,
    /// Half-open range into the page's `text_items`.
    pub items: std::ops::Range<usize>,
    /// Index into the page's `commands` of the `Transform(Begin)` that placed
    /// this box.
    ///
    /// An explicit link, not an ordinal one. The command list also carries the
    /// paint walk's shapes — four in five of which have no text body at all —
    /// so "the *n*th bracket is the *n*th placement" was only ever true by
    /// accident of the text walk running alone, and a check built on it would
    /// silently stop testing anything the moment a second producer appeared.
    pub bracket: usize,
}

/// Which layout path a [`ShapePlacement`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacementKind {
    /// A `p:sp` text body, in the rectangle the file declares for the shape.
    Shape,
    /// One `a:tc`, in a rectangle derived from the table's grid and rows.
    TableCell,
}

/// Lay every slide out and return one [`Page`] per slide.
///
/// `registry` must be the registry the caller intends to measure with; it is
/// threaded to both the measurer used here and the converter, which re-measures
/// to recover each item's box. Passing two different registries would produce
/// boxes that do not match the layout that placed them.
pub fn slides_to_pages(
    data: &[u8],
    registry: &FontRegistry,
) -> Result<SlideGeometry, LiteParseError> {
    let pkg = pptx::walk(data)
        .map_err(|e| LiteParseError::Conversion(format!("pptx parse failed: {e}")))?;
    Ok(layout_deck(&pkg, registry))
}

fn layout_deck(pkg: &PresentationPackage, registry: &FontRegistry) -> SlideGeometry {
    // Parsed once per *theme part*, not once per deck and not once per slide.
    // A deck-wide theme was tolerable while the theme only supplied fonts; a
    // fill resolves its colour through the theme's colour matrix, and a slide
    // under a second master would then take a real, wrong colour from the
    // first master's palette — which is worse than no colour at all, because
    // nothing about the output says it came from the wrong place.
    let mut themes: HashMap<String, Option<Theme>> = HashMap::new();

    let measurer = TextMeasurer::new(registry);
    let (slide_w, slide_h) = pkg.info.slide_size_pt();
    let page_size = PtSize {
        width: Pt::new(slide_w as f32),
        height: Pt::new(slide_h as f32),
    };

    let mut deck = Deck::new(pkg);
    let mut out = SlideGeometry {
        pages: Vec::with_capacity(pkg.slides.len()),
        layouts: Vec::with_capacity(pkg.slides.len()),
        placements: Vec::with_capacity(pkg.slides.len()),
        placed_table_cells: 0,
        unplaced_table_cells: 0,
        unplaced_diagram_bodies: 0,
        painted_shapes: 0,
        unpainted_shapes: 0,
        outlines_defaulted_black: 0,
    };

    for (idx, slide) in pkg.slides.iter().enumerate() {
        let mut items = Vec::new();
        let mut placements = Vec::new();
        let mut commands = Vec::new();
        let theme = slide_theme(pkg, slide, &mut themes);
        // A slide whose shape tree will not parse still yields a page. Page
        // count must equal slide count — a consumer indexes pages by slide.
        if let Some(prepared) = deck.prepare(slide) {
            let cascade = prepared.cascade();
            let mut ctx = ShapeCtx {
                cascade,
                theme,
                measurer: &measurer,
                registry,
                items: &mut items,
                commands: &mut commands,
                placements: &mut placements,
                placed_table_cells: &mut out.placed_table_cells,
                unplaced_table_cells: &mut out.unplaced_table_cells,
                unplaced_diagram_bodies: &mut out.unplaced_diagram_bodies,
                painted_shapes: &mut out.painted_shapes,
                unpainted_shapes: &mut out.unpainted_shapes,
                outlines_defaulted_black: &mut out.outlines_defaulted_black,
            };
            // Two walks over one `prepare`, in the order the raster wants
            // them. See [`paint_shape`] for why they cannot be one walk.
            paint_shapes(&prepared.shapes, &mut ctx);
            for shape in reading_order(&prepared.shapes) {
                layout_shape(shape, &mut ctx);
            }
        }

        out.placements.push(placements);
        out.layouts.push(LayoutedPage {
            commands,
            page_size,
            // A DOCX-only side-channel: it indexes the flattened body blocks a
            // page starts, and a slide is one page with no such flattening.
            block_starts: Vec::new(),
        });
        let content_bounds = union_bounds(&items);
        out.pages.push(Page {
            page_number: idx + 1,
            page_width: page_size.width.raw(),
            page_height: page_size.height.raw(),
            content_bounds,
            text_items: items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations: None,
            form_fields: None,
            structure_tree: None,
        });
    }

    out
}

struct ShapeCtx<'a, 'r> {
    cascade: TextCascade<'a>,
    theme: Option<&'a Theme>,
    measurer: &'a TextMeasurer<'r>,
    registry: &'a FontRegistry,
    items: &'a mut Vec<TextItem>,
    /// The slide's draw commands, in slide coordinates once each shape's run
    /// is read through the bracket that opens it.
    commands: &'a mut Vec<DrawCommand>,
    placements: &'a mut Vec<ShapePlacement>,
    placed_table_cells: &'a mut usize,
    unplaced_table_cells: &'a mut usize,
    unplaced_diagram_bodies: &'a mut usize,
    painted_shapes: &'a mut usize,
    unpainted_shapes: &'a mut usize,
    outlines_defaulted_black: &'a mut usize,
}

impl ShapeCtx<'_, '_> {
    /// Add one body's commands to the slide, wrapped in the bracket that
    /// places them.
    ///
    /// `commands` stay in the body-local space `layout_shape_body` emitted
    /// them in — the same values that go on to become [`TextItem`]s — so the
    /// bracket is the *only* place a slide coordinate is introduced, and the
    /// two consumers cannot drift apart by one of them shifting and the other
    /// not.
    /// Returns the index of the `Begin` mark, which is the placement's link
    /// back into the command list.
    fn push_bracketed(&mut self, placement: ShapeTransform, commands: Vec<DrawCommand>) -> usize {
        let at = self.commands.len();
        self.commands
            .push(DrawCommand::Transform(TransformMark::Begin(placement)));
        self.commands.extend(commands);
        self.commands
            .push(DrawCommand::Transform(TransformMark::End));
        at
    }
}

/// Paint every shape on the slide, in document order.
///
/// **A second traversal, and it has to be.** The text walk uses
/// [`reading_order`], which sorts shapes into bands and drops chrome; both
/// halves are right for reading and wrong for painting. §19.3.1.45 makes
/// document order z-order, and the paint census found that of the 1,263 corpus
/// pairs where two fills overlap — i.e. where paint order is observable at all
/// — reading order sequences **368 (29.1%)** the wrong way round, on 7.3% of
/// slides. A backdrop panel authored last and read first would paint over the
/// content it belongs under.
///
/// The two walks share one `Deck::prepare`, so the rectangles and the cascade
/// they see are the same ones by construction. What is *not* shared is the
/// chrome filter: this walk paints chrome, the text walk still drops it, and
/// the 347 chrome shapes that carry text are a divergence this step leaves
/// open rather than closes (see the module-level note in the plan).
fn paint_shapes(shapes: &[Shape], ctx: &mut ShapeCtx<'_, '_>) {
    for shape in shapes {
        paint_shape(shape, ctx);
        if let ShapeKind::Group(group) = &shape.kind {
            // A group's own `grpSpPr` fill is not lowered to `ShapeProperties`
            // at all, so a group paints nothing itself — its children carry
            // every fill, in their own declaration order.
            paint_shapes(&group.children, ctx);
        }
    }
}

/// One shape's fill and outline as a [`DrawCommand::Path`], if it puts ink on
/// the slide.
///
/// Unlike a text body this needs no bracket: `Path` carries its own
/// origin/rotation/flip/extent and the painter composes them, so the shape is
/// self-placing. That also lets it carry the two flips, which a text bracket
/// deliberately does not — §20.1.7.6 mirrors a shape's *geometry*, and
/// PowerPoint does not mirror the text inside it.
fn paint_shape(shape: &Shape, ctx: &mut ShapeCtx<'_, '_>) {
    let Some(props) = shape_properties(shape) else {
        return;
    };
    // `p:style`'s `fillRef`/`lnRef`/`effectRef` are not parsed yet, so the
    // three style arguments are `None` and 3,381 corpus shapes with no `spPr`
    // fill element resolve to nothing. That is an *under*-paint: visible, but
    // never misleading.
    let visuals = resolve_shape_visuals(Some(props), None, None, None, ctx.theme);
    let paints = !matches!(visuals.fill, ResolvedFill::None) || visuals.stroke.is_some();
    if !paints {
        return;
    }

    let Some(slide_rect) = shape.slide_rect else {
        *ctx.unpainted_shapes += 1;
        return;
    };
    // The *bounding* box, not the raw rect: `pptx::geometry` composes group
    // transforms into `slide_rect`, and a rotated or skewed child's ink lives
    // in the box that composition produced.
    let box_ = slide_rect.bounding_box();
    let extent = PtSize {
        width: emu_to_pt(box_.size.width.raw()),
        height: emu_to_pt(box_.size.height.raw()),
    };
    let Some(geometry) = props.geometry.as_ref() else {
        *ctx.unpainted_shapes += 1;
        return;
    };
    let Some(path) = build_geometry(geometry, extent) else {
        // An unimplemented preset. Counted, never approximated by its bounding
        // box: a `rect` drawn where a `cloud` was asked for is a wrong slide,
        // not a coarse one.
        *ctx.unpainted_shapes += 1;
        return;
    };

    if visuals.stroke.is_some() && props.outline.as_ref().is_some_and(|o| o.fill.is_none()) {
        *ctx.outlines_defaulted_black += 1;
    }
    *ctx.painted_shapes += 1;
    ctx.commands.push(DrawCommand::Path {
        origin: PtOffset::new(
            emu_to_pt(box_.origin.x.raw()),
            emu_to_pt(box_.origin.y.raw()),
        ),
        rotation: slide_rect.rotation,
        flip_h: slide_rect.flip_h,
        flip_v: slide_rect.flip_v,
        extent,
        paths: path.paths,
        fill: visuals.fill,
        stroke: visuals.stroke,
        effects: visuals.effects,
    });
}

/// The `spPr` a shape paints from. Groups and graphic frames have none of
/// their own — a table's cell fills are `a:tcPr`, which this pass does not
/// paint yet.
fn shape_properties(shape: &Shape) -> Option<&liteparse_ooxml::model::ShapeProperties> {
    match &shape.kind {
        ShapeKind::AutoShape(sp) => sp.properties.as_ref(),
        ShapeKind::Connector(c) => c.properties.as_ref(),
        ShapeKind::Picture(p) => p.shape_properties.as_ref(),
        ShapeKind::Group(_) | ShapeKind::GraphicFrame(_) => None,
    }
}

/// The theme governing one slide, parsed once per theme *part*.
fn slide_theme<'a>(
    pkg: &PresentationPackage,
    slide: &pptx::SlideParts,
    cache: &'a mut HashMap<String, Option<Theme>>,
) -> Option<&'a Theme> {
    let path = slide.theme_path.clone()?;
    cache
        .entry(path)
        .or_insert_with(|| {
            pkg.theme_for(slide)
                .and_then(|bytes| liteparse_ooxml::docx::parse::theme::parse_theme(bytes).ok())
        })
        .as_ref()
}

fn layout_shape(shape: &Shape, ctx: &mut ShapeCtx<'_, '_>) {
    match &shape.kind {
        ShapeKind::AutoShape(sp) => {
            if let Some(body) = &sp.text {
                layout_text_shape(shape, body, ctx);
            }
        }
        ShapeKind::Group(group) => {
            // Children carry composed `slide_rect`s already, so a group needs
            // no coordinate work here — only the same reading order the
            // markdown emitter applies within it.
            for child in reading_order(&group.children) {
                layout_shape(child, ctx);
            }
        }
        ShapeKind::GraphicFrame(frame) => match &frame.payload {
            pptx::GraphicFramePayload::Table(table) => layout_table(shape, table, ctx),
            pptx::GraphicFramePayload::Diagram { .. } => *ctx.unplaced_diagram_bodies += 1,
            pptx::GraphicFramePayload::Unsupported { .. } => {}
        },
        ShapeKind::Picture(_) | ShapeKind::Connector(_) => {}
    }
}

/// Lay one shape's text body out inside its rectangle and append the resulting
/// items, translated and rotated onto the slide.
fn layout_text_shape(shape: &Shape, body: &TextBody, ctx: &mut ShapeCtx<'_, '_>) {
    // A shape with no rectangle cannot be laid out at all: there is nothing to
    // wrap inside. The geometry pass measured 0 of these across the corpus, and
    // the markdown emitter still emits such a shape's text — so this is a
    // geometry gap for that shape, not a content drop.
    let Some(slide_rect) = shape.slide_rect else {
        return;
    };
    let extent = PtSize {
        width: emu_to_pt(slide_rect.rect.size.width.raw()),
        height: emu_to_pt(slide_rect.rect.size.height.raw()),
    };
    if extent.width <= Pt::ZERO || extent.height <= Pt::ZERO {
        return;
    }

    let title = is_title(shape.placeholder.as_ref());
    // Rung 2 is the shape's own list style, exactly as the markdown emitter
    // layers it on.
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    let auto_fit = ShapeAutoFit::from_body(body.body_pr.as_ref().and_then(|bp| bp.auto_fit));
    let default_family = theme_family(ctx.theme, title);

    let mut blocks = Vec::with_capacity(body.paragraphs.len());
    for para in &body.paragraphs {
        let resolved = cascade.resolve(&para.properties, shape.placeholder.as_ref());
        blocks.push(paragraph_block(
            para,
            &resolved,
            &default_family,
            auto_fit,
            ctx,
        ));
    }

    // The fallback height for a line that states none — an empty paragraph
    // between two bullets. Scaled by the body's own shrink, for the same reason
    // the DOCX path scales it: otherwise a shrunk body keeps full-size blanks.
    let line_height = auto_fit.scale_font(
        ctx.measurer
            .default_line_height(&default_family, spec_default_size()),
    );

    let commands = layout_shape_body(&blocks, extent, body.body_pr.as_ref(), line_height);
    if commands.is_empty() {
        return;
    }

    let frame = Frame::new(slide_rect, extent);
    let bracket = ctx.push_bracketed(frame.place((0.0, 0.0), extent), commands.clone());
    let items = commands_to_items(commands, extent, ctx);
    let first_item = ctx.items.len();
    frame.push_items(items, (0.0, 0.0), ctx.items);

    ctx.placements.push(ShapePlacement {
        kind: PlacementKind::Shape,
        rect: (
            frame.origin.0,
            frame.origin.1,
            extent.width.raw(),
            extent.height.raw(),
        ),
        rotation: frame.item_rotation(),
        shrunk: auto_fit != ShapeAutoFit::NONE,
        items: first_item..ctx.items.len(),
        bracket,
    });
}

/// Convert a laid-out body's draw commands into [`TextItem`]s, in body-local Pt.
///
/// Goes through the DOCX path's own `DrawCommand` → `TextItem` code, on a
/// synthetic page the size of the body's box, so the two formats agree on how a
/// baseline becomes a box (ascent above, descent below, width re-measured).
fn commands_to_items(
    commands: Vec<liteparse_ooxml::render::layout::draw_command::DrawCommand>,
    extent: PtSize,
    ctx: &ShapeCtx<'_, '_>,
) -> Vec<TextItem> {
    if commands.is_empty() {
        return Vec::new();
    }
    let page = LayoutedPage {
        commands,
        page_size: extent,
        block_starts: Vec::new(),
    };
    let converted = docx_layout::layout_to_pages(&[page], ctx.registry, false, false);
    converted
        .pages
        .into_iter()
        .next()
        .map(|p| p.text_items)
        .unwrap_or_default()
}

/// The rectangle a body's items are placed against: where it sits on the slide,
/// and the rotation the whole frame carries.
///
/// A table needs this separated out from the body layout because one frame
/// holds many boxes — every cell rotates about the **frame's** centre, not its
/// own, or a rotated table would fan its cells apart.
struct Frame {
    origin: (f32, f32),
    /// Frame-local centre of rotation.
    centre: (f32, f32),
    /// Clockwise degrees, as the file declares them.
    rot_deg: f32,
    /// The same angle unconverted, for the draw-command bracket — which takes
    /// 60000ths of a degree, exactly as `a:xfrm@rot` states them.
    rotation: liteparse_ooxml::model::dimension::Dimension<
        liteparse_ooxml::model::dimension::SixtieThousandthDeg,
    >,
}

impl Frame {
    fn new(slide_rect: liteparse_ooxml::pptx::SlideRect, extent: PtSize) -> Self {
        Self {
            origin: (
                emu_to_pt(slide_rect.rect.origin.x.raw()).raw(),
                emu_to_pt(slide_rect.rect.origin.y.raw()).raw(),
            ),
            centre: (extent.width.raw() * 0.5, extent.height.raw() * 0.5),
            // OOXML `@rot` is clockwise-positive; `TextItem::rotation` is
            // counter-clockwise degrees.
            rot_deg: slide_rect.rotation.raw() as f32 / ANGLE_UNITS_PER_DEGREE,
            rotation: slide_rect.rotation,
        }
    }

    /// The placement bracket for a box of size `extent` whose top-left sits at
    /// `offset` within this frame.
    ///
    /// A rotation about the *frame's* centre is not a rotation about the
    /// box's, and a table is where the difference shows: every cell turns with
    /// the table, so charging each one its own centre would fan them apart.
    /// A rigid motion decomposes, though — rotate the box about its own centre
    /// by the frame's angle, then put that centre where the frame's rotation
    /// sends it — and the second half is all this computes. For a shape,
    /// `offset` is `(0, 0)` and the two centres coincide, so it reduces to the
    /// frame origin.
    ///
    /// **Flips are deliberately not carried.** `a:xfrm@flipH/@flipV` mirror a
    /// shape's *geometry*; PowerPoint does not mirror the text inside it, and
    /// the item path ignores them for the same reason.
    fn place(&self, offset: (f32, f32), extent: PtSize) -> ShapeTransform {
        let (half_w, half_h) = (extent.width.raw() * 0.5, extent.height.raw() * 0.5);
        let (cx, cy) = (offset.0 + half_w, offset.1 + half_h);
        let (rx, ry) = if self.rot_deg == 0.0 {
            (cx, cy)
        } else {
            let (sin, cos) = self.rot_deg.to_radians().sin_cos();
            let (dx, dy) = (cx - self.centre.0, cy - self.centre.1);
            (
                self.centre.0 + dx * cos - dy * sin,
                self.centre.1 + dx * sin + dy * cos,
            )
        };
        ShapeTransform {
            origin: PtOffset::new(
                Pt::new(self.origin.0 + rx - half_w),
                Pt::new(self.origin.1 + ry - half_h),
            ),
            rotation: self.rotation,
            flip_h: false,
            flip_v: false,
            extent,
        }
    }

    /// The angle an item placed in this frame reports.
    fn item_rotation(&self) -> f32 {
        -self.rot_deg
    }

    /// Append `items` — in box-local Pt — onto the slide, where the box's own
    /// top-left sits at `offset` within the frame.
    fn push_items(&self, items: Vec<TextItem>, offset: (f32, f32), out: &mut Vec<TextItem>) {
        let rotate = self.rot_deg != 0.0;
        let (sin, cos) = (
            self.rot_deg.to_radians().sin(),
            self.rot_deg.to_radians().cos(),
        );
        for mut item in items {
            item.x += offset.0;
            item.y += offset.1;
            if rotate {
                // Rotate the box's top-left about the frame centre, in
                // frame-local space, then translate. The item keeps its
                // unrotated width and height and states its angle, matching how
                // the PDF path reports a rotated run — a rotated AABB would
                // silently widen every box.
                let (dx, dy) = (item.x - self.centre.0, item.y - self.centre.1);
                item.x = self.centre.0 + dx * cos - dy * sin;
                item.y = self.centre.1 + dx * sin + dy * cos;
                item.rotation = self.item_rotation();
            }
            item.x += self.origin.0;
            item.y += self.origin.1;
            out.push(item);
        }
    }
}

/// Lay a DrawingML table's cells out and append their items.
///
/// **The second layout path.** A cell's rectangle is not declared anywhere: it
/// is derived from `a:gridCol` prefix sums across and `a:tr@h` down, both
/// measured from the frame's own origin. Once the rectangle exists a cell is an
/// ordinary DrawingML text body — `a:tcPr` carries the same four insets and the
/// same anchor as an `a:bodyPr` — so everything below the rectangle is the
/// shape path's code.
///
/// Two corpus facts decide the derivation, and both contradict the obvious
/// implementation:
///
/// - **The frame's `@cx` is not the table's width.** It agrees with the grid on
///   26 of 36 corpus tables; the other 10 are two decks whose producer writes a
///   constant 236.2pt for every frame, up to 4x wrong. The grid is authoritative
///   and the frame supplies only the origin.
/// - **`a:tr@h` is a minimum, not a height** (§21.1.3.18). 16 corpus rows
///   declare `h="0"`, which under a literal reading stacks every cell of the
///   table at its top edge. So each row is grown to its tallest cell, which is
///   why cells must be measured before any of them can be placed.
fn layout_table(shape: &Shape, table: &pptx::Table, ctx: &mut ShapeCtx<'_, '_>) {
    let text_cells = || {
        table
            .rows
            .iter()
            .flat_map(|r| r.cells.iter())
            .filter(|c| !c.is_absorbed() && c.text.as_ref().is_some_and(|t| !t.is_empty()))
            .count()
    };
    let Some(slide_rect) = shape.slide_rect else {
        *ctx.unplaced_table_cells += text_cells();
        return;
    };
    let col_edges = prefix_edges(table.grid.iter().map(|w| emu_to_pt(w.raw())));
    if col_edges.len() < 2 || col_edges[col_edges.len() - 1] <= Pt::ZERO {
        // No grid means no cell has a width to wrap in. Counted, not dropped
        // silently — the markdown emitter still emits this table's text.
        *ctx.unplaced_table_cells += text_cells();
        return;
    }

    let default_family = theme_family(ctx.theme, false);
    let line_height = ctx
        .measurer
        .default_line_height(&default_family, spec_default_size());

    // Pass 1: build each cell's blocks and its width, and measure the height it
    // needs. Blocks are kept because pass 2 stacks the very same ones — a
    // rebuild would risk measuring one thing and placing another.
    let mut cells: Vec<CellLayout> = Vec::new();
    for (row_idx, row) in table.rows.iter().enumerate() {
        let mut col = 0usize;
        for cell in &row.cells {
            let span = cell.grid_span.max(1) as usize;
            // An absorbed cell still holds its slot in the grid (§21.1.3.16),
            // so the cursor advances past it even though nothing is drawn.
            let start = col;
            col += span;
            if cell.is_absorbed() || start + 1 >= col_edges.len() {
                continue;
            }
            let Some(body) = &cell.text else { continue };
            let end = col.min(col_edges.len() - 1);
            let width = col_edges[end] - col_edges[start];
            if width <= Pt::ZERO {
                *ctx.unplaced_table_cells += usize::from(!body.is_empty());
                continue;
            }

            let body_pr = cell.properties.text_body_properties();
            let blocks = cell_blocks(body, &default_family, ctx);
            let needed = measure_shape_body(&blocks, width, Some(&body_pr), line_height);
            cells.push(CellLayout {
                row: row_idx,
                row_span: cell.row_span.max(1) as usize,
                left: col_edges[start],
                width,
                blocks,
                body_pr,
                needed,
            });
        }
    }

    let declared: Vec<Pt> = table
        .rows
        .iter()
        .map(|r| {
            r.height
                .map_or(Pt::ZERO, |h| emu_to_pt(h.raw()))
                .max(Pt::ZERO)
        })
        .collect();
    let row_edges = prefix_edges(grown_row_heights(&declared, &cells).into_iter());

    // Pass 2: place. Rotation is the frame's, about the frame's own centre.
    let frame = Frame::new(
        slide_rect,
        PtSize {
            width: emu_to_pt(slide_rect.rect.size.width.raw()),
            height: emu_to_pt(slide_rect.rect.size.height.raw()),
        },
    );
    for cell in cells {
        let bottom = row_edges[(cell.row + cell.row_span).min(row_edges.len() - 1)];
        let extent = PtSize {
            width: cell.width,
            height: bottom - row_edges[cell.row],
        };
        let commands = layout_shape_body(&cell.blocks, extent, Some(&cell.body_pr), line_height);
        if commands.is_empty() {
            continue;
        }
        let offset = (cell.left.raw(), row_edges[cell.row].raw());
        let bracket = ctx.push_bracketed(frame.place(offset, extent), commands.clone());
        let items = commands_to_items(commands, extent, ctx);
        let first_item = ctx.items.len();
        frame.push_items(items, offset, ctx.items);
        if ctx.items.len() == first_item {
            continue;
        }
        *ctx.placed_table_cells += 1;
        // One placement per cell, not per table: the containment check is only
        // worth anything against the box the text was actually wrapped in.
        ctx.placements.push(ShapePlacement {
            kind: PlacementKind::TableCell,
            rect: (
                frame.origin.0 + offset.0,
                frame.origin.1 + offset.1,
                extent.width.raw(),
                extent.height.raw(),
            ),
            rotation: frame.item_rotation(),
            shrunk: false,
            items: first_item..ctx.items.len(),
            bracket,
        });
    }
}

/// Turn a run of lengths into the `n + 1` edges they define, starting at zero.
///
/// Both of a cell's coordinates come from one of these: `a:gridCol` widths
/// across, grown row heights down. `edges[i]` is track `i`'s near edge, so a
/// cell spanning `[i, i + span)` runs from `edges[i]` to `edges[i + span]` and a
/// merge needs no special case.
fn prefix_edges(lengths: impl Iterator<Item = Pt>) -> Vec<Pt> {
    let mut edges = vec![Pt::ZERO];
    let mut acc = Pt::ZERO;
    for len in lengths {
        acc += len;
        edges.push(acc);
    }
    edges
}

/// §21.1.3.18: each row's declared `@h` raised to fit its tallest cell.
///
/// `@h` is a **minimum**, and treating it as the height is not a rounding
/// error: 16 corpus rows declare `h="0"`, which stacks every cell of those
/// tables at the table's top edge.
///
/// A row-spanning cell is deliberately excluded from its row's growth. Its
/// content is shared across every row it covers, so charging the whole height
/// to the first of them would push every later row down — the spanning cell
/// instead takes the summed rectangle and may overflow it, which is what
/// `@vertOverflow`'s default already describes.
fn grown_row_heights(declared: &[Pt], cells: &[CellLayout]) -> Vec<Pt> {
    let mut heights = declared.to_vec();
    for cell in cells {
        if cell.row_span == 1
            && let Some(h) = heights.get_mut(cell.row)
        {
            *h = (*h).max(cell.needed);
        }
    }
    heights
}

/// One table cell, measured and waiting for its row's height to be decided.
struct CellLayout {
    row: usize,
    row_span: usize,
    /// Frame-local left edge and the width the grid gives this cell.
    left: Pt,
    width: Pt,
    blocks: Vec<LayoutBlock>,
    body_pr: liteparse_ooxml::model::BodyProperties,
    /// Height this cell's text needs at `width`, insets included.
    needed: Pt,
}

/// A cell's paragraphs as layout blocks.
///
/// Resolved exactly as `pptx::cell_text` resolves them for markdown — the
/// cell's own `a:lstStyle` as the shape rung and **no placeholder**, since a
/// cell fills none. Diverging here would box text the emitter never wrote.
fn cell_blocks(
    body: &TextBody,
    default_family: &str,
    ctx: &mut ShapeCtx<'_, '_>,
) -> Vec<LayoutBlock> {
    let cascade = TextCascade {
        shape: Some(&body.list_style),
        ..ctx.cascade
    };
    body.paragraphs
        .iter()
        .map(|para| {
            let resolved = cascade.resolve(&para.properties, None);
            paragraph_block(para, &resolved, default_family, ShapeAutoFit::NONE, ctx)
        })
        .collect()
}

/// One `a:p` as a [`LayoutBlock::Paragraph`], with every run measured.
fn paragraph_block(
    para: &TextParagraph,
    resolved: &ResolvedTextStyle,
    default_family: &str,
    auto_fit: ShapeAutoFit,
    ctx: &mut ShapeCtx<'_, '_>,
) -> LayoutBlock {
    let mut fragments = Vec::new();
    collect_run_fragments(
        &para.content,
        resolved,
        default_family,
        auto_fit,
        ctx,
        &mut fragments,
    );

    LayoutBlock::Paragraph {
        fragments,
        style: paragraph_style(resolved, auto_fit),
        page_break_before: false,
        footnotes: Vec::new(),
        floating_images: Vec::new(),
        floating_shapes: Vec::new(),
    }
}

fn collect_run_fragments(
    inlines: &[liteparse_ooxml::model::Inline],
    resolved: &ResolvedTextStyle,
    default_family: &str,
    auto_fit: ShapeAutoFit,
    ctx: &mut ShapeCtx<'_, '_>,
    fragments: &mut Vec<Fragment>,
) {
    use liteparse_ooxml::model::{Inline, RunElement};

    for inline in inlines {
        match inline {
            Inline::TextRun(run) => {
                let mut props = run.properties.clone();
                resolved.apply_to_run(&mut props);
                // The cascade resolves size but has no font rung, so a run that
                // named `+mn-lt` still carries a `ThemeFontRef` here and a run
                // that named nothing carries neither. Resolve the reference,
                // then let `font_props_from_run` fall back to the theme face
                // for the 26.3% that named no family at all.
                if let Some(theme) = ctx.theme {
                    resolve_font_set_themes(&mut props.fonts, theme);
                }
                let font = font_props_from_run(
                    &props,
                    default_family,
                    // `apply_to_run` guarantees a size — `run_defaults.font_size`
                    // is always `Some` — so this is unreachable in practice and
                    // is the §20.1.2.1 spec default rather than a guess.
                    spec_default_size(),
                    auto_fit,
                );
                let color = RgbColor { r: 0, g: 0, b: 0 };
                for element in &run.content {
                    match element {
                        RunElement::Text(text) => {
                            emit_run_fragments(text, &font, color, None, ctx.measurer, fragments)
                        }
                        RunElement::Tab => fragments.push(Fragment::Tab {
                            line_height: font.size,
                            font: std::rc::Rc::new(font.clone()),
                            color,
                            fitting_width: None,
                        }),
                        RunElement::LineBreak(_) => fragments.push(Fragment::LineBreak {
                            line_height: font.size,
                        }),
                        _ => {}
                    }
                }
            }
            Inline::Hyperlink(link) => {
                collect_run_fragments(
                    &link.content,
                    resolved,
                    default_family,
                    auto_fit,
                    ctx,
                    fragments,
                );
            }
            _ => {}
        }
    }
}

fn paragraph_style(resolved: &ResolvedTextStyle, auto_fit: ShapeAutoFit) -> ParagraphStyle {
    let size = resolved
        .run_defaults
        .font_size
        .map(Pt::from)
        .unwrap_or_else(spec_default_size);

    ParagraphStyle {
        alignment: resolved.alignment.unwrap_or(Alignment::Start),
        space_before: spacing_to_pt(resolved.space_before, size),
        space_after: spacing_to_pt(resolved.space_after, size),
        // §21.1.2.2.7 `@marL` is the left edge of the whole paragraph and
        // `@indent` is the first line's offset from it — the same split as
        // §17.3.1.12's `left`/`firstLine`, and a hanging indent is negative in
        // both. So these map across directly.
        indent_left: resolved.margin_left.map(Pt::from).unwrap_or(Pt::ZERO),
        indent_right: resolved.margin_right.map(Pt::from).unwrap_or(Pt::ZERO),
        indent_first_line: resolved.indent.map(Pt::from).unwrap_or(Pt::ZERO),
        line_spacing: match resolved.line_spacing {
            // `a:spcPct` is a multiple of the line's natural height, which is
            // exactly what `Auto` means here.
            Some(Spacing::Percent(p)) => LineSpacingRule::Auto(p.to_fraction()),
            // `a:spcPts` is an absolute height, and DrawingML treats it as a
            // minimum rather than a cap.
            Some(Spacing::Points(p)) => LineSpacingRule::AtLeast(Pt::new(p.to_points_f32())),
            None => LineSpacingRule::Auto(1.0),
        },
        auto_fit,
        default_tab_stop: resolved
            .default_tab_size
            .map(Pt::from)
            .unwrap_or(Pt::new(36.0)),
        ..ParagraphStyle::default()
    }
}

/// `a:spcBef`/`a:spcAft` as a height.
///
/// The percentage form is a percentage **of the font size**, not of the line
/// box — §21.1.2.2.9 defines it against the text size — so it needs the
/// paragraph's resolved size to become a length.
fn spacing_to_pt(spacing: Option<Spacing>, size: Pt) -> Pt {
    match spacing {
        Some(Spacing::Percent(p)) => Pt::new(size.raw() * p.to_fraction()),
        Some(Spacing::Points(p)) => Pt::new(p.to_points_f32()),
        None => Pt::ZERO,
    }
}

/// The theme face a run falls back to when it names no family: `+mj-lt` for a
/// title, `+mn-lt` for everything else (§20.1.4.1.24/§20.1.4.1.26).
///
/// Falls through to the measurer's own generic handling when the deck has no
/// theme or the theme names no latin face — which is host-dependent, and is
/// reported as such by the probe's `ResolveRule` histogram rather than hidden.
fn theme_family(theme: Option<&Theme>, title: bool) -> String {
    let latin = theme.map(|t| {
        if title {
            t.major_font.latin.clone()
        } else {
            t.minor_font.latin.clone()
        }
    });
    match latin {
        Some(f) if !f.is_empty() => f,
        _ => "Arial".to_string(),
    }
}

/// §21.1.2.2.10: the size a run takes when nothing in the cascade supplies one.
/// Matches `pptx::textcascade`'s own spec-default rung (1800 hundredths = 18pt).
fn spec_default_size() -> Pt {
    Pt::new(18.0)
}

fn emu_to_pt(emu: i64) -> Pt {
    Pt::new(emu as f32 / EMU_PER_POINT)
}

fn union_bounds(items: &[TextItem]) -> Option<crate::types::Rect> {
    let first = items.first()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.x + first.width;
    let mut bottom = first.y + first.height;
    for item in items.iter().skip(1) {
        left = left.min(item.x);
        top = top.min(item.y);
        right = right.max(item.x + item.width);
        bottom = bottom.max(item.y + item.height);
    }
    Some(crate::types::Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse_ooxml::model::{ThemeFontScheme, dimension::Dimension};

    fn theme_with(major: &str, minor: &str) -> Theme {
        Theme {
            major_font: ThemeFontScheme {
                latin: major.to_string(),
                ..Default::default()
            },
            minor_font: ThemeFontScheme {
                latin: minor.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn theme_font_splits_title_from_body() {
        // The rung that covers the census's 26.3% of runs naming no font.
        // Getting the halves the wrong way round is invisible in markdown and
        // wrong in every width.
        let theme = theme_with("Georgia", "Verdana");
        assert_eq!(theme_family(Some(&theme), true), "Georgia");
        assert_eq!(theme_family(Some(&theme), false), "Verdana");
    }

    #[test]
    fn theme_font_falls_back_when_absent_or_empty() {
        assert_eq!(theme_family(None, false), "Arial");
        let empty = theme_with("", "");
        assert_eq!(theme_family(Some(&empty), true), "Arial");
    }

    #[test]
    fn percent_spacing_is_a_fraction_of_the_font_size() {
        // §21.1.2.2.9 measures `a:spcPct` against the text size, not the line
        // box. Reading it as a line multiple would scale every gap by the
        // wrong base.
        let size = Pt::new(20.0);
        // 50000 thousandths of a percent = 50%.
        let half = Spacing::Percent(Dimension::new(50_000));
        assert_eq!(spacing_to_pt(Some(half), size), Pt::new(10.0));
        assert_eq!(spacing_to_pt(None, size), Pt::ZERO);
    }

    #[test]
    fn points_spacing_is_absolute() {
        // `a:spcPts` is in hundredths of a point.
        let pts = Spacing::Points(Dimension::new(1_200));
        assert_eq!(spacing_to_pt(Some(pts), Pt::new(20.0)), Pt::new(12.0));
    }

    #[test]
    fn emu_converts_at_12700_per_point() {
        assert_eq!(emu_to_pt(914_400), Pt::new(72.0));
        assert_eq!(emu_to_pt(0), Pt::ZERO);
    }

    fn cell(row: usize, row_span: usize, needed: f32) -> CellLayout {
        CellLayout {
            row,
            row_span,
            left: Pt::ZERO,
            width: Pt::new(100.0),
            blocks: Vec::new(),
            body_pr: liteparse_ooxml::pptx::TableCellProperties::default().text_body_properties(),
            needed: Pt::new(needed),
        }
    }

    #[test]
    fn prefix_edges_bracket_every_track() {
        let e = prefix_edges([Pt::new(10.0), Pt::new(20.0), Pt::new(5.0)].into_iter());
        assert_eq!(
            e,
            vec![Pt::ZERO, Pt::new(10.0), Pt::new(30.0), Pt::new(35.0)]
        );
        // A cell spanning columns 1..3 reads its span straight off the edges.
        assert_eq!(e[3] - e[1], Pt::new(25.0));
        // No tracks is one edge, not zero — the caller checks for `< 2`.
        assert_eq!(prefix_edges(std::iter::empty()), vec![Pt::ZERO]);
    }

    #[test]
    fn row_grows_to_its_tallest_cell_but_never_shrinks() {
        // `@h` is a minimum (§21.1.3.18): a taller cell raises the row, a
        // shorter one leaves the declared height alone.
        let declared = vec![Pt::new(20.0), Pt::new(50.0)];
        let grown = grown_row_heights(&declared, &[cell(0, 1, 35.0), cell(1, 1, 10.0)]);
        assert_eq!(grown, vec![Pt::new(35.0), Pt::new(50.0)]);
    }

    #[test]
    fn zero_declared_height_is_grown_not_taken_literally() {
        // 16 corpus rows declare `h="0"`. Taken literally every row edge is
        // zero and the whole table collapses onto its top edge — text that is
        // still inside the frame, and therefore invisible to a containment
        // check.
        let grown = grown_row_heights(&[Pt::ZERO, Pt::ZERO], &[cell(0, 1, 18.0), cell(1, 1, 24.0)]);
        assert_eq!(grown, vec![Pt::new(18.0), Pt::new(24.0)]);
        let edges = prefix_edges(grown.into_iter());
        assert_eq!(edges[1], Pt::new(18.0));
        assert!(edges[2] > edges[1], "rows must not share an edge");
    }

    #[test]
    fn a_row_spanning_cell_does_not_grow_the_row_it_starts_in() {
        // Its content belongs to every row it covers, so charging the height to
        // the first would push all the later rows down the slide.
        let declared = vec![Pt::new(10.0), Pt::new(10.0)];
        let grown = grown_row_heights(&declared, &[cell(0, 2, 500.0)]);
        assert_eq!(grown, declared);
    }

    #[test]
    fn a_cell_naming_a_row_off_the_end_is_ignored() {
        // Malformed input must not panic or silently resize the table.
        let declared = vec![Pt::new(10.0)];
        assert_eq!(grown_row_heights(&declared, &[cell(7, 1, 99.0)]), declared);
    }

    // ── the placement bracket ────────────────────────────────────────────

    fn frame(x: f32, y: f32, w: f32, h: f32, deg: f32) -> Frame {
        use liteparse_ooxml::model::geometry::{Offset, Rect, Size};
        let emu = |pt: f32| Dimension::new((pt * EMU_PER_POINT) as i64);
        let slide_rect = liteparse_ooxml::pptx::SlideRect {
            rect: Rect::new(Offset::new(emu(x), emu(y)), Size::new(emu(w), emu(h))),
            rotation: Dimension::new((deg * ANGLE_UNITS_PER_DEGREE) as i64),
            flip_h: false,
            flip_v: false,
            skewed: false,
        };
        Frame::new(slide_rect, size(w, h))
    }

    fn size(w: f32, h: f32) -> PtSize {
        PtSize {
            width: Pt::new(w),
            height: Pt::new(h),
        }
    }

    /// An unrotated shape's bracket is its own rectangle — the commands inside
    /// it are body-local, so the origin is where the body starts.
    #[test]
    fn an_unrotated_shape_brackets_at_its_own_origin() {
        let placed = frame(100.0, 50.0, 200.0, 80.0, 0.0).place((0.0, 0.0), size(200.0, 80.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (100.0, 50.0)
        );
        assert_eq!(placed.rotation.raw(), 0);
        assert_eq!(placed.extent.width, Pt::new(200.0));
    }

    /// A rotated shape turns about its own centre, so its origin does not move
    /// — only the space inside the bracket rotates. Rotating the origin as
    /// well would slide every rotated body off its shape.
    #[test]
    fn a_rotated_shape_keeps_its_origin() {
        let placed = frame(100.0, 50.0, 200.0, 80.0, 90.0).place((0.0, 0.0), size(200.0, 80.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (100.0, 50.0)
        );
        assert_eq!(placed.rotation.raw(), 5_400_000, "the bracket carries @rot");
    }

    /// A cell turns with its **table**: its centre lands where the frame's
    /// rotation sends it. Charging each cell its own centre would leave every
    /// cell in place and fan the table apart instead of turning it.
    #[test]
    fn a_cell_rotates_about_the_frame_centre_not_its_own() {
        // A 40x20 cell in the top-left of a 200x80 frame: centre (20, 10)
        // against the frame's (100, 40). A quarter turn sends the offset
        // (-80, -30) to (30, -80), so the cell's centre lands at (130, -40)
        // and its box origin half a cell up and left of that.
        let placed = frame(0.0, 0.0, 200.0, 80.0, 90.0).place((0.0, 0.0), size(40.0, 20.0));
        assert_eq!(
            (placed.origin.x.raw(), placed.origin.y.raw()),
            (110.0, -50.0)
        );

        // Unrotated, the same cell is simply at its offset in the frame.
        let flat = frame(0.0, 0.0, 200.0, 80.0, 0.0).place((5.0, 7.0), size(40.0, 20.0));
        assert_eq!((flat.origin.x.raw(), flat.origin.y.raw()), (5.0, 7.0));
    }

    /// §20.1.7.6 flips mirror a shape's *geometry*; PowerPoint does not mirror
    /// the text inside it, and the item path ignores them for the same reason.
    #[test]
    fn a_bracket_never_carries_a_flip() {
        let placed = frame(0.0, 0.0, 10.0, 10.0, 0.0).place((0.0, 0.0), size(10.0, 10.0));
        assert!(!placed.flip_h && !placed.flip_v);
    }

    #[test]
    fn union_bounds_spans_every_item() {
        let item = |x: f32, y: f32, w: f32, h: f32| TextItem {
            x,
            y,
            width: w,
            height: h,
            ..TextItem::default()
        };
        assert!(union_bounds(&[]).is_none());
        let r = union_bounds(&[item(10.0, 20.0, 5.0, 5.0), item(3.0, 40.0, 2.0, 2.0)]).unwrap();
        assert_eq!((r.x, r.y), (3.0, 20.0));
        assert_eq!((r.width, r.height), (12.0, 22.0));
    }
}
