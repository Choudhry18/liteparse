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

use liteparse_ooxml::model::{Alignment, Theme};
use liteparse_ooxml::pptx::{
    self, PresentationPackage, ResolvedTextStyle, Shape, ShapeKind, Spacing, TextBody, TextCascade,
    TextParagraph,
};
use liteparse_ooxml::render::dimension::Pt;
use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::geometry::PtSize;
use liteparse_ooxml::render::layout::ShapeAutoFit;
use liteparse_ooxml::render::layout::draw_command::LayoutedPage;
use liteparse_ooxml::render::layout::fragment::{
    Fragment, emit_run_fragments, font_props_from_run,
};
use liteparse_ooxml::render::layout::measurer::TextMeasurer;
use liteparse_ooxml::render::layout::paragraph::{LineSpacingRule, ParagraphStyle};
use liteparse_ooxml::render::layout::section::LayoutBlock;
use liteparse_ooxml::render::layout::shape_body::layout_shape_body;
use liteparse_ooxml::render::resolve::color::RgbColor;
use liteparse_ooxml::render::resolve::fonts::resolve_font_set_themes;

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
    /// Table cells carrying text. Their rectangles come from `a:gridCol`
    /// prefix sums and `a:tr@h`, not from a shape rect, which is a second
    /// layout path — and `a:tcPr` (cell margins, per-cell anchor) is not
    /// parsed at all today, so laying them out against spec defaults would put
    /// every cell's text at the wrong inset.
    pub unplaced_table_cells: usize,
    /// SmartArt text bodies. The frame has a rect, but the text lives in
    /// `ppt/diagrams/data*.xml` with its own layout algorithm; the markdown
    /// emitter places the *frame* in reading order, which is a weaker claim
    /// than a per-run box.
    pub unplaced_diagram_bodies: usize,
}

/// Where one text shape's items landed on its slide.
pub struct ShapePlacement {
    /// The shape's unrotated rectangle in slide Pt: `(x, y, width, height)`.
    pub rect: (f32, f32, f32, f32),
    /// Counter-clockwise degrees, matching [`TextItem::rotation`].
    pub rotation: f32,
    /// Whether the body's `a:normAutofit` shrinks it (`@fontScale` < 100%).
    pub shrunk: bool,
    /// Half-open range into the page's `text_items`.
    pub items: std::ops::Range<usize>,
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
    // Parsed once per deck. `pkg.theme` is the first master's theme — a
    // multi-master deck loses the per-master theme, which `pptx::walk` already
    // documents as a known limit of the package walk rather than of this pass.
    let theme = pkg
        .theme
        .as_ref()
        .and_then(|bytes| liteparse_ooxml::docx::parse::theme::parse_theme(bytes).ok());

    let measurer = TextMeasurer::new(registry);
    let (slide_w, slide_h) = pkg.info.slide_size_pt();
    let page_size = PtSize {
        width: Pt::new(slide_w as f32),
        height: Pt::new(slide_h as f32),
    };

    let mut deck = Deck::new(pkg);
    let mut out = SlideGeometry {
        pages: Vec::with_capacity(pkg.slides.len()),
        placements: Vec::with_capacity(pkg.slides.len()),
        unplaced_table_cells: 0,
        unplaced_diagram_bodies: 0,
    };

    for (idx, slide) in pkg.slides.iter().enumerate() {
        let mut items = Vec::new();
        let mut placements = Vec::new();
        // A slide whose shape tree will not parse still yields a page. Page
        // count must equal slide count — a consumer indexes pages by slide.
        if let Some(prepared) = deck.prepare(slide) {
            let cascade = prepared.cascade();
            let mut ctx = ShapeCtx {
                cascade,
                theme: theme.as_ref(),
                measurer: &measurer,
                registry,
                items: &mut items,
                placements: &mut placements,
                unplaced_table_cells: &mut out.unplaced_table_cells,
                unplaced_diagram_bodies: &mut out.unplaced_diagram_bodies,
            };
            for shape in reading_order(&prepared.shapes) {
                layout_shape(shape, &mut ctx);
            }
        }

        out.placements.push(placements);
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
    placements: &'a mut Vec<ShapePlacement>,
    unplaced_table_cells: &'a mut usize,
    unplaced_diagram_bodies: &'a mut usize,
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
            pptx::GraphicFramePayload::Table(table) => {
                *ctx.unplaced_table_cells += table
                    .rows
                    .iter()
                    .flat_map(|r| r.cells.iter())
                    .filter(|c| c.text.as_ref().is_some_and(|t| !t.is_empty()))
                    .count();
            }
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

    // Convert through the DOCX path's own `DrawCommand` → `TextItem` code, on a
    // synthetic page the size of the shape, so the two formats agree on how a
    // baseline becomes a box (ascent above, descent below, width re-measured).
    let shape_page = LayoutedPage {
        commands,
        page_size: extent,
        block_starts: Vec::new(),
    };
    let converted = docx_layout::layout_to_pages(&[shape_page], ctx.registry, false, false);
    let Some(page) = converted.pages.into_iter().next() else {
        return;
    };

    let origin_x = emu_to_pt(slide_rect.rect.origin.x.raw()).raw();
    let origin_y = emu_to_pt(slide_rect.rect.origin.y.raw()).raw();
    // OOXML `@rot` is clockwise-positive; `TextItem::rotation` is
    // counter-clockwise degrees.
    let rot_deg = slide_rect.rotation.raw() as f32 / ANGLE_UNITS_PER_DEGREE;
    let rotate = rot_deg != 0.0;
    let (cx, cy) = (extent.width.raw() * 0.5, extent.height.raw() * 0.5);
    let (sin, cos) = (rot_deg.to_radians().sin(), rot_deg.to_radians().cos());

    let first_item = ctx.items.len();
    for mut item in page.text_items {
        if rotate {
            // Rotate the box's top-left about the shape centre, in shape-local
            // space, then translate. The item keeps its unrotated width and
            // height and states its angle, matching how the PDF path reports a
            // rotated run — a rotated AABB would silently widen every box.
            let (dx, dy) = (item.x - cx, item.y - cy);
            item.x = cx + dx * cos - dy * sin;
            item.y = cy + dx * sin + dy * cos;
            item.rotation = -rot_deg;
        }
        item.x += origin_x;
        item.y += origin_y;
        ctx.items.push(item);
    }

    ctx.placements.push(ShapePlacement {
        rect: (origin_x, origin_y, extent.width.raw(), extent.height.raw()),
        rotation: -rot_deg,
        shrunk: auto_fit != ShapeAutoFit::NONE,
        items: first_item..ctx.items.len(),
    });
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
