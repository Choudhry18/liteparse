//! The shape tree (§19.3.1.45 `p:spTree`) — schema and lowering.
//!
//! A slide, layout, master or notes slide is, structurally, a `p:cSld`
//! wrapping one `p:spTree`. This module turns that tree's raw XML into typed
//! [`Shape`]s, preserving document order (which *is* PPTX z-order, unlike the
//! Word `behindDoc` approximation `raster.rs` implements) and preserving group
//! nesting rather than flattening it.
//!
//! It is deliberately **cascade-free and geometry-free**, in the same spirit as
//! [`crate::pptx::text`]: a shape that omits `<a:xfrm>` gets `transform: None`
//! here rather than an inherited or invented rectangle. Resolving that `None`
//! is the placeholder cascade's job, and composing a group's child coordinate
//! space onto its children is the geometry pass's. Both need this walk to exist
//! first, because both key off [`Placeholder`], which only exists once shapes
//! are typed.
//!
//! ## What this module has to handle
//!
//! The shape-tree children are `p:sp` (a text-bearing shape), `p:pic`
//! (never carries text), `p:cxnSp` (a connector — never carries text,
//! geometry only), `p:grpSp` (nests to arbitrary depth) and
//! `p:graphicFrame` (table, diagram, OLE object or chart).
//!
//! Two invariants this module and its cascade depend on:
//!
//! - **Every shape lacking an `xfrm` carries a `p:ph`**, with no
//!   counterexample of any kind — including pictures, which is why this
//!   module models `p:pic` itself instead of reusing the DOCX `PictureXml`,
//!   which has no slot for a placeholder (see [`PicXml`]). So the
//!   placeholder cascade is exactly the missing-geometry case, with no
//!   second fallback needed. [`Shape::needs_inherited_geometry`] names it,
//!   and `pptx_shape_probe` fails if a deck ever breaks it.
//! - **`p:graphicFrame` positions itself with `<p:xfrm>`, not `<a:xfrm>`.**
//!   Different namespace and a different parent element, so it does *not*
//!   arrive through [`SpPrXml`] like every other shape's transform. Missing
//!   this yields graphic frames silently stacked at the origin.

use serde::Deserialize;

use crate::model::dimension::{Dimension, Emu};
use crate::model::geometry::{Offset, Size};
use crate::model::{
    BodyProperties, ColorMap, DocProperties, DrawingFill, FontReference, NvPicProperties, Picture,
    ShapeProperties, StyleMatrixRef, TextAnchoringType, TextVerticalType, Transform2D,
};

use crate::docx::error::Result;
use crate::docx::parse::drawing::schema::color::StSchemeColorVal;
use crate::docx::parse::drawing::schema::fill::BlipFillXml;
use crate::docx::parse::drawing::schema::picture::{CNvPicPrXml, CNvPrXml};
use crate::docx::parse::drawing::schema::shape::{
    ExtXml, OffXml, ShapeStyleXml, SpPrXml, StTextAnchoringType, StTextVerticalType,
    StyleMatrixRefXml, XfrmXml, pick_fill,
};
use crate::docx::parse::serde_xml;

use super::geometry::SlideRect;
use super::text::{TextBody, TextBodyXml};

// ── Lowered model ────────────────────────────────────────────────────────────

/// One member of a shape tree, in document order.
#[derive(Clone, Debug)]
pub struct Shape {
    /// `cNvPr` — id, name, alt text, hidden flag. Cosmetic metadata, but the
    /// name is how authors label a shape and is the best diagnostic handle
    /// there is when chasing a bad slide.
    pub non_visual: DocProperties,
    /// `p:nvSpPr/p:nvPr/p:ph`, when this shape is a placeholder.
    pub placeholder: Option<Placeholder>,
    /// The shape's own `<a:xfrm>` (or `<p:xfrm>` for a graphic frame).
    ///
    /// `None` means the file declares no geometry, **not** that the shape sits
    /// at the origin. It is resolved later by the placeholder cascade.
    pub transform: Option<Transform2D>,
    /// True once [`crate::pptx::cascade`] has filled `transform` in from a
    /// layout or master placeholder, rather than the file declaring it here.
    ///
    /// Kept because "declared" and "inherited" are different facts about the
    /// document even though they produce the same rectangle: the geometry
    /// probe excludes inherited shapes from its source-EMU check (there is no
    /// source EMU to check against), and a debug dump that cannot tell them
    /// apart makes a bad cascade look like a bad file.
    pub transform_inherited: bool,
    /// This shape's position **on the slide**, filled by
    /// [`crate::pptx::geometry`]. `None` until that pass runs.
    ///
    /// Distinct from `transform`, which stays exactly as the file declares it —
    /// inside a group that is the group's *child* coordinate space and is not a
    /// slide position at all. Keeping both means a debug dump and the geometry
    /// probe can still see the declared EMU, which is what they check against
    /// the source XML.
    pub slide_rect: Option<SlideRect>,
    /// §19.3.1.46 `p:style` — the shape's references into the theme's style
    /// matrices, when it declares one.
    ///
    /// On [`Shape`] rather than on each [`ShapeKind`] variant because all three
    /// kinds that can carry one (`p:sp`, `p:cxnSp`, `p:pic`) carry the *same*
    /// `a:CT_ShapeStyle`, and both consumers — the fill emitter and, later, the
    /// text cascade — reach it from the shape rather than from its payload.
    /// `p:grpSp` and `p:graphicFrame` have no `style` in the schema and always
    /// leave this `None`.
    pub style: Option<ShapeStyle>,
    pub kind: ShapeKind,
}

/// §20.1.4.1.4 `a:CT_ShapeStyle` — the four theme-matrix references a
/// `p:style` holds.
///
/// The spec requires all four children, but they are modelled as `Option`
/// because a file that omits one should lose that reference and not the
/// whole shape.
///
/// Deliberately *not* four loose fields on [`Shape`] the way
/// [`crate::model::WordProcessingShape`] carries them: there they arrived one
/// at a time as Word's needs appeared, and here all four exist from the start
/// and are always read together.
#[derive(Clone, Debug, Default)]
pub struct ShapeStyle {
    /// `a:lnRef` — the theme line style. Supplies the outline's colour when
    /// `a:ln` declares none, and the **entire** outline when there is no
    /// `a:ln` at all.
    pub line_ref: Option<StyleMatrixRef>,
    /// `a:fillRef` — the theme fill style, used only when `spPr` declares no
    /// fill element of its own. `idx="0"` is a legal value here and means
    /// inherit *nothing*.
    pub fill_ref: Option<StyleMatrixRef>,
    /// `a:effectRef` — the theme effect style, consulted when the direct
    /// `a:effectLst` is absent or empty.
    pub effect_ref: Option<StyleMatrixRef>,
    /// `a:fontRef` — the shape's default text colour and theme font
    /// collection.
    ///
    /// Parsed here but **not yet consumed**: it belongs to the text cascade,
    /// not the paint walk, and wiring it into one and not the other would
    /// make the markdown emitter and the geometry pass disagree about a
    /// run's colour.
    pub font_ref: Option<FontReference>,
}

impl From<ShapeStyleXml> for ShapeStyle {
    fn from(x: ShapeStyleXml) -> Self {
        Self {
            line_ref: x.ln_ref.map(Into::into),
            fill_ref: x.fill_ref.map(Into::into),
            effect_ref: x.effect_ref.map(Into::into),
            font_ref: x.font_ref.map(Into::into),
        }
    }
}

impl Shape {
    /// True when this shape declares no transform of its own and therefore
    /// depends on the placeholder cascade for its position and size.
    ///
    /// Every shape observed with no transform is a placeholder, which is
    /// what makes the cascade a total function over this set rather than a
    /// best-effort one.
    pub fn needs_inherited_geometry(&self) -> bool {
        self.transform.is_none()
    }

    /// This shape's own text body, if the kind can carry one.
    ///
    /// Only `p:sp` has one directly; a graphic frame's text lives in its table
    /// cells and a group's in its children, so both return `None` here. Use
    /// [`Shape::visit`] to reach those.
    pub fn text(&self) -> Option<&TextBody> {
        match &self.kind {
            ShapeKind::AutoShape(sp) => sp.text.as_ref(),
            _ => None,
        }
    }

    /// Depth-first pre-order walk over this shape and every descendant,
    /// including group children.
    ///
    /// Pre-order matters: a group is visited before its children, so a visitor
    /// accumulating a coordinate transform sees the parent's frame first.
    pub fn visit(&self, f: &mut impl FnMut(&Shape)) {
        f(self);
        if let ShapeKind::Group(group) = &self.kind {
            for child in &group.children {
                child.visit(f);
            }
        }
    }
}

/// Depth-first pre-order walk over a whole shape tree.
pub fn visit_all(shapes: &[Shape], f: &mut impl FnMut(&Shape)) {
    for shape in shapes {
        shape.visit(f);
    }
}

/// The five shape-tree members, plus the graphic-frame payloads we do and do
/// not understand.
#[derive(Clone, Debug)]
pub enum ShapeKind {
    /// §19.3.1.43 `p:sp` — the text-bearing shape.
    AutoShape(Box<AutoShape>),
    /// §19.3.1.37 `p:pic`. Lands in the shared [`Picture`] model, since
    /// `p:pic` and the DOCX `pic:pic` are the same `CT_Picture` — but it is
    /// read through this module's own [`PicXml`], which adds the placeholder
    /// PresentationML allows and WordprocessingML has no concept of.
    Picture(Box<Picture>),
    /// §19.3.1.19 `p:cxnSp` — a connector. Never carries text; kept because
    /// it is real geometry a layout pass and a screenshot both need.
    Connector(Box<Connector>),
    /// §19.3.1.22 `p:grpSp`. Boxed: `Group` is by far the largest variant,
    /// and shape trees can hold thousands of shapes.
    Group(Box<Group>),
    /// §19.3.1.21 `p:graphicFrame`.
    GraphicFrame(Box<GraphicFrame>),
}

/// §19.3.1.43 `p:sp`.
#[derive(Clone, Debug, Default)]
pub struct AutoShape {
    pub properties: Option<ShapeProperties>,
    /// `p:txBody`. Can be absent — a shape with no text body is not the
    /// same as one with an empty body.
    pub text: Option<TextBody>,
    /// `p:cNvSpPr/@txBox` — the shape is a plain text box rather than a
    /// preset-geometry shape that happens to have text.
    pub is_text_box: bool,
}

/// §19.3.1.19 `p:cxnSp`.
#[derive(Clone, Debug, Default)]
pub struct Connector {
    pub properties: Option<ShapeProperties>,
}

/// §19.3.1.22 `p:grpSp`.
///
/// A group defines a **child coordinate space**: descendants' `<a:off>` are
/// expressed against `child_offset`/`child_extent`, which must be mapped onto
/// the group's own `offset`/`extent` before they mean anything on the slide.
/// That composition is the geometry pass's job — this type only carries the
/// numbers it needs.
#[derive(Clone, Debug, Default)]
pub struct Group {
    /// §20.1.8.35 — the fill a descendant's `a:grpFill` inherits.
    ///
    /// A [`ShapeProperties`] would be the obvious type and is the wrong one:
    /// §19.3.1.23's `p:grpSpPr` is `a:xfrm` + `EG_FillProperties` +
    /// `EG_EffectProperties` + `a:scene3d`, so a group has **no geometry and
    /// no `a:ln`** — those two fields could never be populated — and its
    /// transform is already hoisted onto [`Shape::transform`], where the
    /// geometry pass reads it. Carrying the one member that has a consumer
    /// keeps the second copy of the transform from existing to drift.
    ///
    /// A group never paints this itself: it has no geometry to put it in.
    /// The fill exists only as the source of the children's inheritance.
    pub fill: Option<DrawingFill>,
    /// `a:chOff` — the origin of the child coordinate space.
    pub child_offset: Option<Offset<Emu>>,
    /// `a:chExt` — the extent of the child coordinate space.
    pub child_extent: Option<Size<Emu>>,
    /// In document order, which is z-order. Groups can nest.
    pub children: Vec<Shape>,
}

/// §19.3.1.21 `p:graphicFrame`.
#[derive(Clone, Debug)]
pub struct GraphicFrame {
    pub payload: GraphicFramePayload,
}

/// What a graphic frame actually holds, keyed by `a:graphicData/@uri`.
#[derive(Clone, Debug)]
pub enum GraphicFramePayload {
    /// A DrawingML table (`a:tbl`).
    Table(Box<Table>),
    /// SmartArt, which does not hold its own content: the frame carries
    /// only `dgm:relIds`, and the text lives in the data part that `@r:dm`
    /// points at (`ppt/diagrams/data*.xml`). Resolving the relationship
    /// needs the owning [`Part`](crate::pptx::Part), which this layer does
    /// not have, so the id is surfaced for the caller to resolve.
    ///
    /// Worth its own variant rather than `Unsupported`: diagram data parts
    /// routinely carry real text that a shape walk alone can never see.
    Diagram { data_rel: String },
    /// Embedded OLE or a chart — payloads we genuinely do not read.
    ///
    /// Kept as a typed variant rather than dropped so that a caller can emit
    /// a placeholder and a probe can *count* what is being lost. Silently
    /// discarding them would make the gap invisible.
    Unsupported { uri: String },
}

/// §21.1.3.13 `a:tbl`.
///
/// PPTX expresses merges differently from WordprocessingML, and the difference
/// is load-bearing when this is lowered to `Block::MergedTable`: the
/// **origin** cell carries `gridSpan`/`rowSpan` giving the size of the merged
/// region, and every cell the region absorbs is still *present* in the XML,
/// flagged `hMerge`/`vMerge`. So a consumer building an occupancy grid must
/// drop the flagged cells rather than expecting them to be missing.
#[derive(Clone, Debug, Default)]
pub struct Table {
    /// `a:tblGrid/a:gridCol/@w`, one entry per column.
    pub grid: Vec<Dimension<Emu>>,
    pub rows: Vec<TableRow>,
    /// `a:tblPr/@firstRow` — the table style paints row 0 as a header. The
    /// only header signal PPTX gives.
    pub first_row: bool,
    pub first_col: bool,
    pub band_row: bool,
}

/// §21.1.3.18 `a:tr`.
#[derive(Clone, Debug, Default)]
pub struct TableRow {
    /// `@h` — declared row height. A *minimum*: the rendered row grows to
    /// fit its content.
    pub height: Option<Dimension<Emu>>,
    pub cells: Vec<TableCell>,
}

/// §21.1.3.16 `a:tc`.
#[derive(Clone, Debug, Default)]
pub struct TableCell {
    /// `a:txBody`. The one place `a:txBody` rather than `p:txBody` appears.
    pub text: Option<TextBody>,
    /// `@gridSpan` — columns this cell spans. 1 unless it is a merge origin.
    pub grid_span: u32,
    /// `@rowSpan` — rows this cell spans. 1 unless it is a merge origin.
    pub row_span: u32,
    /// `@hMerge` — this cell is absorbed by a horizontal merge to its left.
    pub h_merge: bool,
    /// `@vMerge` — this cell is absorbed by a vertical merge above it.
    pub v_merge: bool,
    /// `a:tcPr`, the text-relevant half.
    pub properties: TableCellProperties,
}

impl TableCell {
    /// True when this cell is covered by a neighbour's merge and must not
    /// occupy a slot in an occupancy grid.
    pub fn is_absorbed(&self) -> bool {
        self.h_merge || self.v_merge
    }
}

/// §21.1.3.17 `a:tcPr` — the half of a cell's properties that decides where its
/// text sits.
///
/// Fills, borders (`a:lnL`/`lnR`/`lnT`/`lnB`/`lnTlToBr`/`lnBlToTr`) and
/// `@horzOverflow` are deliberately absent: the first two are paint, and the
/// third is essentially never used in practice. `@anchorCtr` is likewise
/// unread — it centres the text *block* horizontally, which this model has
/// no way to express.
///
/// **A cell's own `a:bodyPr` is not an alternative source.** §21.1.3.16
/// puts the insets and the anchor here on `a:tcPr` instead, so a cell's
/// `a:bodyPr` is expected to be bare.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TableCellProperties {
    /// `@marL`, in EMU. `None` is the §21.1.3.17 default of 91440.
    pub left_margin: Option<Dimension<Emu>>,
    /// `@marT`. `None` is the default of 45720.
    pub top_margin: Option<Dimension<Emu>>,
    /// `@marR`. `None` is the default of 91440.
    pub right_margin: Option<Dimension<Emu>>,
    /// `@marB`. `None` is the default of 45720.
    pub bottom_margin: Option<Dimension<Emu>>,
    /// `@anchor` — where the text sits vertically in the cell.
    ///
    /// **Not a rare attribute**: a non-top anchor is common. Ignoring it
    /// tops every cell's text.
    pub anchor: Option<TextAnchoringType>,
    /// `@vert` — vertical text. Rare in practice, and carried only so a
    /// consumer can tell "horizontal" from "not modelled".
    pub vert: Option<TextVerticalType>,
}

impl TableCellProperties {
    /// This cell's properties as the equivalent `a:bodyPr`, so a cell can be
    /// laid out by the same DrawingML text-body code as a shape.
    ///
    /// The mapping is exact rather than approximate, and that is a fact about
    /// the spec rather than a convenience: §21.1.3.17's four cell margins carry
    /// the **same defaults** as §20.1.2.1.1's four insets (91440 EMU
    /// horizontal, 45720 vertical), so an absent attribute means the same
    /// length on both elements and `None` can be passed straight through.
    ///
    /// The four fields left `None` are ones a cell genuinely cannot declare —
    /// a cell has no text rotation, wrap mode, or autofit — so their spec
    /// defaults are the right reading and not a dropped value. `@vertOverflow`
    /// is the one to keep an eye on: its default is `overflow`, which is what a
    /// cell taller than its row does *after* the row has been grown to fit.
    pub fn text_body_properties(&self) -> BodyProperties {
        BodyProperties {
            rotation: None,
            vert: self.vert,
            wrap: None,
            left_inset: self.left_margin,
            top_inset: self.top_margin,
            right_inset: self.right_margin,
            bottom_inset: self.bottom_margin,
            anchor: self.anchor,
            vert_overflow: None,
            auto_fit: None,
        }
    }
}

/// §19.3.1.36 `p:ph` — the placeholder a shape fills.
///
/// Both attributes are optional in the file and both have spec defaults that
/// the cascade depends on, so they are materialized here rather than left as
/// `Option` for every consumer to re-derive. Both defaults are exercised
/// routinely, not just hypothetical fallbacks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// `@type`, defaulting to `body` per §19.7.10.
    pub kind: PlaceholderKind,
    /// `@idx`, defaulting to 0 per §19.7.10.
    ///
    /// **`u32`, not a narrower integer.** `idx="4294967295"` (`u32::MAX`) is
    /// a legal `xsd:unsignedInt` value and appears in real decks; anything
    /// narrower would silently wrap or hard-fail on it.
    pub idx: u32,
}

/// §19.7.10 ST_PlaceholderType.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PlaceholderKind {
    Title,
    /// The `@type` default, and the most common value in practice.
    #[default]
    Body,
    CtrTitle,
    SubTitle,
    Dt,
    Ftr,
    SldNum,
    SldImg,
    Pic,
    Hdr,
    Tbl,
    Chart,
    ClipArt,
    Dgm,
    Media,
    Obj,
    /// An unrecognised `@type`. Per §17.17 an invalid attribute value is
    /// treated as absent, and absent means `body` — but folding it into
    /// [`PlaceholderKind::Body`] here would make a typo indistinguishable from
    /// a real body placeholder, so it is kept separate and the cascade decides.
    Unknown,
}

impl PlaceholderKind {
    /// The layout→master match rule, per python-pptx's 14-entry collapse map.
    ///
    /// The cascade does **not** match the same way at every level: slide →
    /// layout matches on `idx`, but layout → master matches on *type*, and
    /// only after collapsing the many body-ish types onto `body`. A master
    /// holds no `pic` or `tbl` placeholder to match against, so without this
    /// collapse those shapes find nothing and lose their inherited geometry.
    pub fn collapsed_for_master(self) -> PlaceholderKind {
        use PlaceholderKind::*;
        match self {
            Title | CtrTitle => Title,
            Dt => Dt,
            Ftr => Ftr,
            SldNum => SldNum,
            // Everything body-ish collapses: the master models only one.
            Body | SubTitle | Obj | Chart | Tbl | ClipArt | Dgm | Media | Pic | SldImg | Hdr
            | Unknown => Body,
        }
    }
}

/// §19.3.1.1 `<p:bg>` — the background of one slide, layout or master.
///
/// The two arms are the element's own `xsd:choice` and they resolve against
/// different things, which is why this is an enum rather than a struct with
/// two `Option`s: [`Self::Properties`] is self-contained, while
/// [`Self::Reference`] is an index into the *theme*, and under a convention
/// that is not the one a shape's `<a:fillRef>` uses. See
/// [`resolve_background_fill`] for that trap.
///
/// [`resolve_background_fill`]: crate::render::resolve::shape_visuals::resolve_background_fill
///
/// Every slide is expected to resolve to an effective background of some
/// kind, whether declared directly or inherited through the chain.
#[derive(Clone, Debug)]
pub enum Background {
    /// §19.3.1.2 `<p:bgPr>` — an explicit fill.
    ///
    /// Its `EG_EffectProperties` sibling is **not** modelled: the rasterizer
    /// renders no effects at this tier, so keeping the field would only
    /// misreport coverage. `a:scene3d`/`a:sp3d` and `@shadeToTitle` are
    /// dropped for the same reason.
    Properties(DrawingFill),
    /// §19.3.1.3 `<p:bgRef>` — an index into the theme's style matrix.
    Reference(StyleMatrixRef),
}

impl Background {
    fn from_xml(x: BgXml) -> Option<Self> {
        // §19.3.1.1 is a choice, so at most one arm is present. `bgPr` is
        // checked first only to give a malformed both-present element a
        // deterministic reading.
        if let Some(pr) = x.bg_pr {
            // A `<p:bgPr>` with no recognized fill child is malformed (the
            // spec requires one). Treat it as "declares nothing" so the
            // cascade keeps looking upward, rather than as an opaque
            // transparent background that would mask the master's.
            return pick_fill(
                pr.no_fill,
                pr.grp_fill,
                pr.solid_fill,
                pr.grad_fill,
                pr.blip_fill,
                pr.patt_fill,
            )
            .map(Background::Properties);
        }
        x.bg_ref.map(|r| Background::Reference(r.into()))
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Parse the shape tree out of a slide, layout, master or notes-slide part.
///
/// Takes the **whole part's** bytes — `p:sld`, `p:sldLayout`, `p:sldMaster` or
/// `p:notes` — and digs to `p:cSld/p:spTree`. All four roots differ only in the
/// siblings of `cSld`, which are ignored here, so one entry point serves them
/// all.
///
/// A part with no shape tree yields an empty `Vec` rather than an error: the
/// fail-open posture `ATTRIBUTION.md` records for this vendor.
///
/// Reads only `p:spTree`. Callers that also need the part's background — the
/// painter does; the markdown emitter does not — want [`parse_slide_part`],
/// which returns both from a single deserialization.
pub fn parse_shape_tree(data: &[u8]) -> Result<Vec<Shape>> {
    Ok(parse_slide_part(data)?.shapes)
}

/// Parse one drawing object — an `sp`, `grpSp`, or `cxnSp` element sliced
/// verbatim out of a part — into the lowered [`Shape`] model.
///
/// Exists for the SpreadsheetML drawing reader: an `xdr:sp` carries the same
/// CT_Shape content model as a `p:sp` (quick-xml's serde layer matches local
/// names with prefixes stripped, the same reading `parse_text_body` relies
/// on), so the XLSX paint layer reuses this module's tree — `spPr`, `style`,
/// group child spaces, MCE — instead of growing a second parser for it.
///
/// Any other root element yields `None` rather than an error, so a caller
/// slicing by local name does not have to pre-filter exactly.
pub fn parse_single_object(data: &[u8]) -> Result<Option<Shape>> {
    let mut reader = quick_xml::Reader::from_reader(data);
    let mut buf = Vec::new();
    let root = loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?
        {
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => {
                let name = e.name();
                let name = name.as_ref();
                let cut = name.iter().position(|&b| b == b':').map_or(0, |i| i + 1);
                break name[cut..].to_vec();
            }
            quick_xml::events::Event::Eof => return Ok(None),
            _ => {}
        }
    };
    Ok(match root.as_slice() {
        b"sp" => Some(lower_sp(serde_xml::from_xml::<SpXml>(data)?)),
        b"grpSp" => Some(lower_grp(serde_xml::from_xml::<GrpSpXml>(data)?)),
        b"cxnSp" => Some(lower_cxn(serde_xml::from_xml::<CxnSpXml>(data)?)),
        _ => None,
    })
}

/// §19.2.1.32 `@showMasterSp` alone, read from the root element without
/// deserializing the part.
///
/// [`parse_slide_part`] already returns this as
/// [`SlidePart::show_inherited_shapes`], and that is what every *walk* should
/// use. This exists for the one caller that needs the attribute **without**
/// the shape tree: a deck-wide tally of inherited text has to know which
/// slides actually show an inherited layer, and visits every slide before
/// either walk begins. Deserializing every slide part a second time just to
/// read one boolean is measurably more expensive than this targeted read.
///
/// The duplication is deliberate and is fenced by
/// `cheap_reader_agrees_with_the_full_parse`, which asserts the two answers
/// match on the same bytes — a second reader of an attribute is only safe while
/// something fails when they disagree.
///
/// Defaults to **true**, per the schema, and on anything it cannot read: a
/// part whose XML is malformed is the full parse's problem to report, and
/// answering "shows its inherited shapes" here keeps the failure in one
/// place.
pub fn shows_inherited_shapes(data: &[u8]) -> bool {
    let mut reader = quick_xml::Reader::from_reader(data);
    let mut buf = Vec::new();
    loop {
        // The root element is the first `Start` — `Empty` cannot be it, since a
        // `p:sld` with no `p:cSld` is still not self-closing in any producer,
        // and an empty root has no inherited shapes to draw anyway.
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                // The true-family is `AttrBool`'s, spelled the same way on
                // purpose: anything else present — `0`, `off`, a typo — is
                // false there, and a reader that only special-cased `"0"`
                // would answer differently on the odd ones.
                return e
                    .try_get_attribute("showMasterSp")
                    .ok()
                    .flatten()
                    .and_then(|a| a.unescape_value().ok())
                    .is_none_or(|v| matches!(v.as_ref(), "1" | "true" | "on"));
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => return true,
            _ => buf.clear(),
        }
    }
}

/// A `p:cSld`'s two payloads: its shape tree and its background.
///
/// One struct rather than two entry points because both come out of the same
/// `p:cSld`, and parsing the part twice to get them would double the cost of
/// the deck walk for a field that is one element wide.
#[derive(Clone, Debug)]
pub struct SlidePart {
    pub shapes: Vec<Shape>,
    /// §19.3.1.1 `<p:bg>`, when *this* part declares one. `None` means "not
    /// declared here", **not** "no background" — the value is inherited,
    /// and resolving it is [`crate::pptx::cascade::resolve_background`]'s
    /// job. Most slides inherit their background from the master rather
    /// than declaring their own.
    pub background: Option<Background>,
    /// §19.3.1.6 `p:clrMap` (master) or §19.3.1.7 `p:clrMapOvr`'s
    /// `a:overrideClrMapping` (slide/layout), when *this* part states one.
    ///
    /// `None` means "not stated here" and inherits, exactly like `background`
    /// — and a `p:clrMapOvr` holding `a:masterClrMapping` is a `None`, because
    /// that element's entire content is "inherit".
    pub color_map: Option<ColorMap>,
    /// §19.2.1.32 `p:sld/@showMasterSp` and §19.3.1.39 `p:sldLayout`'s — "draw
    /// the shapes the part above this one supplies". Defaults to **true**,
    /// per both elements' schema.
    ///
    /// A `bool` rather than an `Option`, because unlike `background` and
    /// `color_map` this attribute does not cascade: it is a statement about
    /// *this* part's own rendering, and a slide that says nothing shows its
    /// layout's shapes whatever the layout says about the master's. A
    /// master's root has no such attribute and always reads `true`.
    pub show_inherited_shapes: bool,
}

impl Default for SlidePart {
    fn default() -> Self {
        Self {
            shapes: Vec::new(),
            background: None,
            color_map: None,
            show_inherited_shapes: true,
        }
    }
}

/// Parse a slide, layout, master or notes-slide part into its shapes and its
/// own (uninherited) background.
pub fn parse_slide_part(data: &[u8]) -> Result<SlidePart> {
    let part: SlidePartXml = serde_xml::from_xml(data)?;
    // The colour map is a sibling of `p:cSld`, not a child, so it survives a
    // part with no shape tree at all.
    let color_map = part
        .clr_map
        .or_else(|| part.clr_map_ovr.and_then(|o| o.override_clr_mapping))
        .map(ClrMapXml::into_color_map);
    // Also a sibling of `p:cSld`, and read before the early return for the same
    // reason as the colour map.
    let show_inherited_shapes = part.show_master_sp.is_none_or(|b| b.0);
    let Some(c_sld) = part.c_sld else {
        return Ok(SlidePart {
            color_map,
            show_inherited_shapes,
            ..Default::default()
        });
    };
    Ok(SlidePart {
        shapes: c_sld
            .sp_tree
            .map(|t| lower_children(t.children))
            .unwrap_or_default(),
        background: c_sld.bg.and_then(Background::from_xml),
        color_map,
        show_inherited_shapes,
    })
}

// ── Schema ───────────────────────────────────────────────────────────────────
//
// As in `pptx::text`, coverage is driven by what real decks actually use
// rather than by reading §19.3 front to back.

#[derive(Deserialize)]
struct SlidePartXml {
    #[serde(rename = "cSld", default)]
    c_sld: Option<CSldXml>,
    /// §19.3.1.6, on `p:sldMaster` only.
    #[serde(rename = "clrMap", default)]
    clr_map: Option<ClrMapXml>,
    /// §19.3.1.7, on `p:sld` and `p:sldLayout` only. Never both, since no
    /// part's content model allows the two elements together.
    #[serde(rename = "clrMapOvr", default)]
    clr_map_ovr: Option<ClrMapOvrXml>,
    /// §19.2.1.32 / §19.3.1.39 `@showMasterSp`, on `p:sld` and `p:sldLayout`.
    /// Absent on `p:sldMaster` and `p:notes`, which is why it is an `Option`
    /// collapsed to the schema default of `true` by the caller.
    #[serde(rename = "@showMasterSp", default)]
    show_master_sp: Option<crate::docx::parse::primitives::toggles::AttrBool>,
}

/// §19.3.1.6 CT_ColorMapping. Every attribute is *required* by the schema, but
/// each is read as optional and defaulted individually: a master missing one is
/// malformed, and per §17.17 the honest reading of a missing/invalid value is
/// the default mapping rather than a rejected deck.
#[derive(Deserialize)]
struct ClrMapXml {
    #[serde(
        rename = "@bg1",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    bg1: Option<StSchemeColorVal>,
    #[serde(
        rename = "@tx1",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    tx1: Option<StSchemeColorVal>,
    #[serde(
        rename = "@bg2",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    bg2: Option<StSchemeColorVal>,
    #[serde(
        rename = "@tx2",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    tx2: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent1",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent1: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent2",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent2: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent3",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent3: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent4",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent4: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent5",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent5: Option<StSchemeColorVal>,
    #[serde(
        rename = "@accent6",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    accent6: Option<StSchemeColorVal>,
    #[serde(
        rename = "@hlink",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    hlink: Option<StSchemeColorVal>,
    #[serde(
        rename = "@folHlink",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    fol_hlink: Option<StSchemeColorVal>,
}

impl ClrMapXml {
    fn into_color_map(self) -> ColorMap {
        let d = ColorMap::default();
        ColorMap {
            bg1: self.bg1.map_or(d.bg1, Into::into),
            tx1: self.tx1.map_or(d.tx1, Into::into),
            bg2: self.bg2.map_or(d.bg2, Into::into),
            tx2: self.tx2.map_or(d.tx2, Into::into),
            accent1: self.accent1.map_or(d.accent1, Into::into),
            accent2: self.accent2.map_or(d.accent2, Into::into),
            accent3: self.accent3.map_or(d.accent3, Into::into),
            accent4: self.accent4.map_or(d.accent4, Into::into),
            accent5: self.accent5.map_or(d.accent5, Into::into),
            accent6: self.accent6.map_or(d.accent6, Into::into),
            hlink: self.hlink.map_or(d.hlink, Into::into),
            folink: self.fol_hlink.map_or(d.folink, Into::into),
        }
    }
}

/// §19.3.1.7 CT_ColorMappingOverride — a choice of `a:masterClrMapping`
/// (inherit) and `a:overrideClrMapping` (a full CT_ColorMapping stated
/// locally).
#[derive(Deserialize)]
struct ClrMapOvrXml {
    #[serde(rename = "overrideClrMapping", default)]
    override_clr_mapping: Option<ClrMapXml>,
}

#[derive(Deserialize)]
struct CSldXml {
    /// §19.3.1.1 — precedes `spTree` in the content model.
    #[serde(rename = "bg", default)]
    bg: Option<BgXml>,
    #[serde(rename = "spTree", default)]
    sp_tree: Option<SpTreeXml>,
}

/// §19.3.1.1 CT_Background — `xsd:choice` of `bgPr` and `bgRef`.
#[derive(Deserialize)]
struct BgXml {
    #[serde(rename = "bgPr", default)]
    bg_pr: Option<BgPrXml>,
    #[serde(rename = "bgRef", default)]
    bg_ref: Option<StyleMatrixRefXml>,
}

/// §19.3.1.2 CT_BackgroundProperties.
///
/// The six fill members are spelled out and routed through the DrawingML
/// [`pick_fill`] rather than re-derived, because this is the same
/// `EG_FillProperties` group `<a:spPr>` carries — see [`SpPrXml`].
#[derive(Deserialize, Default)]
struct BgPrXml {
    #[serde(rename = "noFill", default)]
    no_fill: Option<crate::docx::parse::drawing::schema::fill::Empty>,
    #[serde(rename = "solidFill", default)]
    solid_fill: Option<crate::docx::parse::drawing::schema::fill::SolidFillXml>,
    #[serde(rename = "gradFill", default)]
    grad_fill: Option<crate::docx::parse::drawing::schema::fill::GradFillXml>,
    #[serde(rename = "blipFill", default)]
    blip_fill: Option<crate::docx::parse::drawing::schema::fill::BlipFillXml>,
    #[serde(rename = "pattFill", default)]
    patt_fill: Option<crate::docx::parse::drawing::schema::fill::PattFillXml>,
    #[serde(rename = "grpFill", default)]
    grp_fill: Option<crate::docx::parse::drawing::schema::fill::Empty>,
}

#[derive(Deserialize, Default)]
struct SpTreeXml {
    /// Members interleave freely and their order **is** z-order, so they are
    /// collected as one ordered `$value` union rather than as five independent
    /// `Vec`s — the same reason `TextParagraphXml` does it for `a:r`/`a:br`.
    #[serde(rename = "$value", default)]
    children: Vec<ShapeTreeChildXml>,
}

#[derive(Deserialize)]
enum ShapeTreeChildXml {
    #[serde(rename = "sp")]
    Sp(Box<SpXml>),
    #[serde(rename = "pic")]
    Pic(Box<PicXml>),
    #[serde(rename = "cxnSp")]
    CxnSp(Box<CxnSpXml>),
    #[serde(rename = "grpSp")]
    GrpSp(Box<GrpSpXml>),
    #[serde(rename = "graphicFrame")]
    GraphicFrame(Box<GraphicFrameXml>),
    #[serde(rename = "AlternateContent")]
    AlternateContent(Box<AlternateContentXml>),
    /// `p:nvGrpSpPr`, `p:grpSpPr` and `p:extLst` arrive here too — they are
    /// captured by named fields on `GrpSpXml` where they matter and ignored
    /// otherwise. Also absorbs genuinely unmodelled members such as
    /// `p:contentPart`; without this arm one of them fails the whole part.
    #[serde(other)]
    Other,
}

/// §M.2.2 `mc:AlternateContent`.
///
/// **The branch yielding the most text wins**, ties going to `Choice`. This is
/// deliberately *not* the MCE rule, and the deviation was forced by a
/// measurement rather than chosen up front.
///
/// Strict MCE says: use a `Choice` only if you understand every namespace in
/// its `@Requires`, else fall through to `Fallback`. We understand none of
/// the namespaces PowerPoint commonly requires (`v`, `p14`, `cx1`, `a14`),
/// so that rule always picks `Fallback`. It is the right rule for a
/// *renderer* — the fallback is what an old viewer is meant to draw — and
/// the wrong one for a text extractor, because the fallback is routinely a
/// **rasterized picture of the content**.
///
/// The case that motivates this: a slide equation can be written as
/// `mc:Choice Requires="a14"` holding a `p:sp` whose paragraph interleaves
/// `<a14:m><m:oMath>` blocks with real `<a:r>` runs, beside an
/// `mc:Fallback` holding a `p:pic` of the same equation. Preferring the
/// fallback drops the `<a:r>` runs between the math blocks — real text a
/// renderer doesn't need but an extractor does.
///
/// Comparing text also handles the opposite case without a second rule:
/// `mc:Choice` holding a `p:contentPart` (ink) lowers to nothing, so its
/// `p:pic` fallback wins on its own merits.
#[derive(Deserialize, Default)]
struct AlternateContentXml {
    #[serde(rename = "Choice", default)]
    choices: Vec<McBranchXml>,
    #[serde(rename = "Fallback", default)]
    fallback: Option<McBranchXml>,
}

#[derive(Deserialize, Default)]
struct McBranchXml {
    #[serde(rename = "$value", default)]
    children: Vec<ShapeTreeChildXml>,
}

impl AlternateContentXml {
    /// Lowers every branch and keeps the one with the most text. Branches are
    /// mutually exclusive alternatives — exactly one may survive — so lowering
    /// them all is not double-counting, and there are only ever a handful.
    fn into_shapes(self) -> Vec<Shape> {
        let mut best: Option<(usize, Vec<Shape>)> = None;
        let branches = self
            .choices
            .into_iter()
            .chain(self.fallback)
            .map(|b| lower_children(b.children));
        for shapes in branches {
            let len = text_len(&shapes);
            // `>` not `>=` keeps the earliest branch on a tie, and `Choice`
            // comes first: it is the higher-fidelity representation, so a
            // fallback that merely matches it should not displace it.
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, shapes));
            }
        }
        best.map(|(_, shapes)| shapes).unwrap_or_default()
    }
}

/// Total characters of text reachable from these shapes, groups and table cells
/// included. Used only to choose between `mc:AlternateContent` branches.
fn text_len(shapes: &[Shape]) -> usize {
    let mut total = 0;
    visit_all(shapes, &mut |shape| {
        if let Some(body) = shape.text() {
            total += body.plain_text().len();
        }
        if let ShapeKind::GraphicFrame(frame) = &shape.kind
            && let GraphicFramePayload::Table(table) = &frame.payload
        {
            for row in &table.rows {
                for cell in &row.cells {
                    if let Some(body) = &cell.text {
                        total += body.plain_text().len();
                    }
                }
            }
        }
    });
    total
}

#[derive(Deserialize)]
struct SpXml {
    #[serde(rename = "nvSpPr", default)]
    nv_sp_pr: Option<NvSpPrXml>,
    #[serde(rename = "spPr", default)]
    sp_pr: Option<SpPrXml>,
    /// §19.3.1.46 `p:style`. Same `a:CT_ShapeStyle` the DOCX `wps:style`
    /// holds, so the schema type is shared — the content models coincide
    /// element for element, which is the only case where reusing one is safe.
    #[serde(rename = "style", default)]
    style: Option<ShapeStyleXml>,
    #[serde(rename = "txBody", default)]
    tx_body: Option<TextBodyXml>,
}

#[derive(Debug, Deserialize)]
struct NvSpPrXml {
    #[serde(rename = "cNvPr")]
    cnv_pr: CNvPrXml,
    #[serde(rename = "cNvSpPr", default)]
    cnv_sp_pr: Option<CNvSpPrXml>,
    #[serde(rename = "nvPr", default)]
    nv_pr: Option<NvPrXml>,
}

#[derive(Debug, Deserialize, Default)]
struct CNvSpPrXml {
    #[serde(rename = "@txBox", default)]
    tx_box: Option<crate::docx::parse::primitives::toggles::AttrBool>,
}

#[derive(Debug, Deserialize, Default)]
struct NvPrXml {
    #[serde(rename = "ph", default)]
    ph: Option<PhXml>,
}

#[derive(Debug, Deserialize, Default)]
struct PhXml {
    #[serde(rename = "@type", default)]
    ph_type: Option<String>,
    /// Lenient: an unparseable `@idx` becomes the spec default 0 rather than
    /// failing the part.
    #[serde(
        rename = "@idx",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::or_default"
    )]
    idx: u32,
}

impl PhXml {
    fn into_model(self) -> Placeholder {
        Placeholder {
            kind: match self.ph_type.as_deref().map(str::trim) {
                None | Some("") | Some("body") => PlaceholderKind::Body,
                Some("title") => PlaceholderKind::Title,
                Some("ctrTitle") => PlaceholderKind::CtrTitle,
                Some("subTitle") => PlaceholderKind::SubTitle,
                Some("dt") => PlaceholderKind::Dt,
                Some("ftr") => PlaceholderKind::Ftr,
                Some("sldNum") => PlaceholderKind::SldNum,
                Some("sldImg") => PlaceholderKind::SldImg,
                Some("pic") => PlaceholderKind::Pic,
                Some("hdr") => PlaceholderKind::Hdr,
                Some("tbl") => PlaceholderKind::Tbl,
                Some("chart") => PlaceholderKind::Chart,
                Some("clipArt") => PlaceholderKind::ClipArt,
                Some("dgm") => PlaceholderKind::Dgm,
                Some("media") => PlaceholderKind::Media,
                Some("obj") => PlaceholderKind::Obj,
                Some(_) => PlaceholderKind::Unknown,
            },
            idx: self.idx,
        }
    }
}

/// §19.3.1.37 `p:pic`.
///
/// Mirrors the shared `PictureXml` field for field — same `CT_Picture` — but
/// adds the one thing PresentationML has and WordprocessingML does not: a
/// picture can be a **placeholder**.
///
/// Reusing `PictureXml` directly would lose the `p:ph`, and some
/// placeholder pictures declare no `xfrm` at all and therefore have no
/// geometry from any other source — which is why "every shape without an
/// `xfrm` is a placeholder" only holds when pictures carry `p:ph` too.
#[derive(Debug, Deserialize)]
struct PicXml {
    #[serde(rename = "nvPicPr")]
    nv_pic_pr: PicNvPrXml,
    #[serde(rename = "blipFill")]
    blip_fill: BlipFillXml,
    #[serde(rename = "spPr", default)]
    sp_pr: Option<SpPrXml>,
    #[serde(rename = "style", default)]
    style: Option<ShapeStyleXml>,
}

#[derive(Debug, Deserialize)]
struct PicNvPrXml {
    #[serde(rename = "cNvPr")]
    cnv_pr: CNvPrXml,
    #[serde(rename = "cNvPicPr", default)]
    cnv_pic_pr: Option<CNvPicPrXml>,
    /// The PresentationML addition — `p:nvPr/p:ph`.
    #[serde(rename = "nvPr", default)]
    nv_pr: Option<NvPrXml>,
}

#[derive(Debug, Deserialize)]
struct CxnSpXml {
    #[serde(rename = "nvCxnSpPr", default)]
    nv_cxn_sp_pr: Option<NvCxnSpPrXml>,
    #[serde(rename = "spPr", default)]
    sp_pr: Option<SpPrXml>,
    #[serde(rename = "style", default)]
    style: Option<ShapeStyleXml>,
}

#[derive(Debug, Deserialize)]
struct NvCxnSpPrXml {
    #[serde(rename = "cNvPr")]
    cnv_pr: CNvPrXml,
}

#[derive(Deserialize)]
struct GrpSpXml {
    #[serde(rename = "nvGrpSpPr", default)]
    nv_grp_sp_pr: Option<NvGrpSpPrXml>,
    #[serde(rename = "grpSpPr", default)]
    grp_sp_pr: Option<GrpSpPrXml>,
    /// Members arrive through `$value` alongside `nvGrpSpPr`/`grpSpPr`, which
    /// the named fields above capture and the `Other` arm then ignores.
    #[serde(rename = "$value", default)]
    children: Vec<ShapeTreeChildXml>,
}

#[derive(Debug, Deserialize)]
struct NvGrpSpPrXml {
    #[serde(rename = "cNvPr")]
    cnv_pr: CNvPrXml,
}

/// §19.3.1.23 `p:grpSpPr` — `a:xfrm` + `EG_FillProperties` +
/// `EG_EffectProperties` + `a:scene3d`. There is deliberately no `ln` field:
/// the schema gives a group no outline. The six fill members route through
/// [`pick_fill`], the same collapse `SpPrXml` and `p:bgPr` use.
#[derive(Debug, Deserialize, Default)]
struct GrpSpPrXml {
    #[serde(rename = "xfrm", default)]
    xfrm: Option<GroupXfrmXml>,
    #[serde(rename = "@bwMode", default)]
    _bw_mode: Option<String>,
    #[serde(rename = "noFill", default)]
    no_fill: Option<crate::docx::parse::drawing::schema::fill::Empty>,
    #[serde(rename = "solidFill", default)]
    solid_fill: Option<crate::docx::parse::drawing::schema::fill::SolidFillXml>,
    #[serde(rename = "gradFill", default)]
    grad_fill: Option<crate::docx::parse::drawing::schema::fill::GradFillXml>,
    #[serde(rename = "blipFill", default)]
    blip_fill: Option<crate::docx::parse::drawing::schema::fill::BlipFillXml>,
    #[serde(rename = "pattFill", default)]
    patt_fill: Option<crate::docx::parse::drawing::schema::fill::PattFillXml>,
    #[serde(rename = "grpFill", default)]
    grp_fill: Option<crate::docx::parse::drawing::schema::fill::Empty>,
}

/// A group's `<a:xfrm>`, which is `XfrmXml` **plus** `chOff`/`chExt`.
///
/// Deliberately a separate type rather than widening [`XfrmXml`]: that type has
/// a 1:1 `From` impl onto [`Transform2D`], which is `Copy` and has no slot for
/// a child coordinate space. Widening it would either break that mapping or
/// leave two fields that are always `None` on every non-group shape.
#[derive(Debug, Deserialize, Default)]
struct GroupXfrmXml {
    #[serde(
        rename = "@rot",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    rot: Option<Dimension<crate::model::dimension::SixtieThousandthDeg>>,
    #[serde(rename = "@flipH", default)]
    flip_h: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "@flipV", default)]
    flip_v: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "off", default)]
    off: Option<OffXml>,
    #[serde(rename = "ext", default)]
    ext: Option<ExtXml>,
    #[serde(rename = "chOff", default)]
    ch_off: Option<OffXml>,
    #[serde(rename = "chExt", default)]
    ch_ext: Option<ExtXml>,
}

impl GroupXfrmXml {
    fn transform(&self) -> Transform2D {
        Transform2D {
            rotation: self.rot,
            flip_h: self.flip_h.as_ref().map(|b| b.0),
            flip_v: self.flip_v.as_ref().map(|b| b.0),
            offset: self.off.as_ref().map(|o| Offset { x: o.x, y: o.y }),
            extent: self.ext.as_ref().map(|e| Size {
                width: e.cx,
                height: e.cy,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GraphicFrameXml {
    #[serde(rename = "nvGraphicFramePr", default)]
    nv_graphic_frame_pr: Option<NvGraphicFramePrXml>,
    /// **`p:xfrm`, not `a:xfrm`** — a graphic frame positions itself with a
    /// PresentationML element that is a direct child of the frame, not through
    /// `spPr` like every other shape. quick-xml drops the prefix, so this
    /// `rename` reads the right element; the trap is forgetting the field
    /// exists, not spelling it.
    #[serde(rename = "xfrm", default)]
    xfrm: Option<XfrmXml>,
    #[serde(rename = "graphic", default)]
    graphic: Option<GraphicXml>,
}

#[derive(Debug, Deserialize)]
struct NvGraphicFramePrXml {
    #[serde(rename = "cNvPr")]
    cnv_pr: CNvPrXml,
}

#[derive(Debug, Deserialize, Default)]
struct GraphicXml {
    #[serde(rename = "graphicData", default)]
    graphic_data: Option<GraphicDataXml>,
}

#[derive(Debug, Deserialize, Default)]
struct GraphicDataXml {
    #[serde(rename = "@uri", default)]
    uri: String,
    #[serde(rename = "tbl", default)]
    tbl: Option<TblXml>,
    #[serde(rename = "relIds", default)]
    rel_ids: Option<DgmRelIdsXml>,
}

/// §21.4.2.19 `dgm:relIds`. Four relationship ids; `@r:dm` is the data model,
/// the only one carrying text (`@r:lo` layout, `@r:qs` quick style,
/// `@r:cs` colours are all presentation).
#[derive(Debug, Deserialize, Default)]
struct DgmRelIdsXml {
    #[serde(rename = "@dm", default)]
    dm: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct TblXml {
    #[serde(rename = "tblPr", default)]
    tbl_pr: Option<TblPrXml>,
    #[serde(rename = "tblGrid", default)]
    tbl_grid: Option<TblGridXml>,
    #[serde(rename = "tr", default)]
    rows: Vec<TrXml>,
}

#[derive(Debug, Deserialize, Default)]
struct TblPrXml {
    #[serde(rename = "@firstRow", default)]
    first_row: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "@firstCol", default)]
    first_col: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "@bandRow", default)]
    band_row: Option<crate::docx::parse::primitives::toggles::AttrBool>,
}

#[derive(Debug, Deserialize, Default)]
struct TblGridXml {
    #[serde(rename = "gridCol", default)]
    cols: Vec<GridColXml>,
}

#[derive(Debug, Deserialize)]
struct GridColXml {
    #[serde(
        rename = "@w",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::nonneg_or_default"
    )]
    w: Dimension<Emu>,
}

#[derive(Debug, Deserialize, Default)]
struct TrXml {
    #[serde(
        rename = "@h",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    h: Option<Dimension<Emu>>,
    #[serde(rename = "tc", default)]
    cells: Vec<TcXml>,
}

#[derive(Debug, Deserialize, Default)]
struct TcXml {
    #[serde(rename = "txBody", default)]
    tx_body: Option<TextBodyXml>,
    #[serde(
        rename = "@gridSpan",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    grid_span: Option<u32>,
    #[serde(
        rename = "@rowSpan",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    row_span: Option<u32>,
    #[serde(rename = "@hMerge", default)]
    h_merge: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "@vMerge", default)]
    v_merge: Option<crate::docx::parse::primitives::toggles::AttrBool>,
    #[serde(rename = "tcPr", default)]
    tc_pr: Option<TcPrXml>,
}

/// §21.1.3.17 `a:tcPr`. Only the text-placement attributes are modelled; see
/// [`TableCellProperties`] for what is left out and why.
#[derive(Debug, Deserialize, Default)]
struct TcPrXml {
    // §21.1.3.17 margins are `ST_Coordinate32` (signed), same as `a:bodyPr`'s
    // insets, so they take the same lenient signed deserializer and are not
    // clamped.
    #[serde(
        rename = "@marL",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_l: Option<Dimension<Emu>>,
    #[serde(
        rename = "@marT",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_t: Option<Dimension<Emu>>,
    #[serde(
        rename = "@marR",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_r: Option<Dimension<Emu>>,
    #[serde(
        rename = "@marB",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    mar_b: Option<Dimension<Emu>>,
    #[serde(
        rename = "@anchor",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    anchor: Option<StTextAnchoringType>,
    #[serde(
        rename = "@vert",
        default,
        deserialize_with = "crate::docx::parse::primitives::lenient::opt_attr"
    )]
    vert: Option<StTextVerticalType>,
}

impl From<TcPrXml> for TableCellProperties {
    fn from(x: TcPrXml) -> Self {
        Self {
            left_margin: x.mar_l,
            top_margin: x.mar_t,
            right_margin: x.mar_r,
            bottom_margin: x.mar_b,
            anchor: x.anchor.map(Into::into),
            vert: x.vert.map(Into::into),
        }
    }
}

// ── Lowering ─────────────────────────────────────────────────────────────────

fn lower_children(children: Vec<ShapeTreeChildXml>) -> Vec<Shape> {
    let mut out = Vec::with_capacity(children.len());
    lower_into(children, &mut out);
    out
}

fn lower_into(children: Vec<ShapeTreeChildXml>, out: &mut Vec<Shape>) {
    for child in children {
        match child {
            ShapeTreeChildXml::Sp(sp) => out.push(lower_sp(*sp)),
            ShapeTreeChildXml::Pic(pic) => out.push(lower_pic(*pic)),
            ShapeTreeChildXml::CxnSp(cxn) => out.push(lower_cxn(*cxn)),
            ShapeTreeChildXml::GrpSp(grp) => out.push(lower_grp(*grp)),
            ShapeTreeChildXml::GraphicFrame(gf) => out.push(lower_graphic_frame(*gf)),
            // Splices the surviving branch in at the parent's level, so an
            // `mc:AlternateContent` wrapper never shows up as a shape and never
            // perturbs z-order.
            ShapeTreeChildXml::AlternateContent(ac) => out.extend(ac.into_shapes()),
            ShapeTreeChildXml::Other => {}
        }
    }
}

fn doc_properties(cnv_pr: Option<CNvPrXml>) -> DocProperties {
    match cnv_pr {
        Some(p) => DocProperties {
            id: p.id,
            name: p.name,
            description: p.descr,
            hidden: p.hidden.map(|b| b.0),
            title: p.title,
        },
        None => DocProperties {
            id: 0,
            name: String::new(),
            description: None,
            hidden: None,
            title: None,
        },
    }
}

fn lower_sp(sp: SpXml) -> Shape {
    let (cnv_pr, cnv_sp_pr, nv_pr) = match sp.nv_sp_pr {
        Some(nv) => (Some(nv.cnv_pr), nv.cnv_sp_pr, nv.nv_pr),
        None => (None, None, None),
    };
    let properties: Option<ShapeProperties> = sp.sp_pr.map(Into::into);
    Shape {
        non_visual: doc_properties(cnv_pr),
        placeholder: nv_pr.and_then(|nv| nv.ph).map(PhXml::into_model),
        transform: properties.as_ref().and_then(|p| p.transform),
        transform_inherited: false,
        slide_rect: None,
        style: sp.style.map(Into::into),
        kind: ShapeKind::AutoShape(Box::new(AutoShape {
            properties,
            text: sp.tx_body.map(TextBodyXml::into_model),
            is_text_box: cnv_sp_pr
                .and_then(|c| c.tx_box)
                .map(|b| b.0)
                .unwrap_or(false),
        })),
    }
}

fn lower_pic(pic: PicXml) -> Shape {
    let non_visual: DocProperties = pic.nv_pic_pr.cnv_pr.into();
    let shape_properties: Option<ShapeProperties> = pic.sp_pr.map(Into::into);
    let transform = shape_properties.as_ref().and_then(|p| p.transform);
    Shape {
        non_visual: non_visual.clone(),
        placeholder: pic
            .nv_pic_pr
            .nv_pr
            .and_then(|nv| nv.ph)
            .map(PhXml::into_model),
        transform,
        transform_inherited: false,
        slide_rect: None,
        style: pic.style.map(Into::into),
        kind: ShapeKind::Picture(Box::new(Picture {
            nv_pic_pr: NvPicProperties {
                cnv_pr: non_visual,
                cnv_pic_pr: pic.nv_pic_pr.cnv_pic_pr.map(Into::into),
            },
            blip_fill: pic.blip_fill.into(),
            shape_properties,
        })),
    }
}

fn lower_cxn(cxn: CxnSpXml) -> Shape {
    let properties: Option<ShapeProperties> = cxn.sp_pr.map(Into::into);
    Shape {
        non_visual: doc_properties(cxn.nv_cxn_sp_pr.map(|nv| nv.cnv_pr)),
        placeholder: None,
        transform: properties.as_ref().and_then(|p| p.transform),
        transform_inherited: false,
        slide_rect: None,
        style: cxn.style.map(Into::into),
        kind: ShapeKind::Connector(Box::new(Connector { properties })),
    }
}

fn lower_grp(grp: GrpSpXml) -> Shape {
    let grp_sp_pr = grp.grp_sp_pr.unwrap_or_default();
    let fill = pick_fill(
        grp_sp_pr.no_fill,
        grp_sp_pr.grp_fill,
        grp_sp_pr.solid_fill,
        grp_sp_pr.grad_fill,
        grp_sp_pr.blip_fill,
        grp_sp_pr.patt_fill,
    );
    let xfrm = grp_sp_pr.xfrm;
    let transform = xfrm.as_ref().map(GroupXfrmXml::transform);
    Shape {
        non_visual: doc_properties(grp.nv_grp_sp_pr.map(|nv| nv.cnv_pr)),
        placeholder: None,
        transform,
        transform_inherited: false,
        slide_rect: None,
        // §19.3.1.22: `p:grpSp` has no `style` child in the schema.
        style: None,
        kind: ShapeKind::Group(Box::new(Group {
            fill,
            child_offset: xfrm
                .as_ref()
                .and_then(|x| x.ch_off.as_ref())
                .map(|o| Offset { x: o.x, y: o.y }),
            child_extent: xfrm.as_ref().and_then(|x| x.ch_ext.as_ref()).map(|e| Size {
                width: e.cx,
                height: e.cy,
            }),
            children: lower_children(grp.children),
        })),
    }
}

fn lower_graphic_frame(gf: GraphicFrameXml) -> Shape {
    let data = gf
        .graphic
        .unwrap_or_default()
        .graphic_data
        .unwrap_or_default();
    let payload = match (data.tbl, data.rel_ids.and_then(|r| r.dm)) {
        (Some(tbl), _) => GraphicFramePayload::Table(Box::new(lower_table(tbl))),
        (None, Some(dm)) if !dm.is_empty() => GraphicFramePayload::Diagram { data_rel: dm },
        _ => GraphicFramePayload::Unsupported { uri: data.uri },
    };
    Shape {
        non_visual: doc_properties(gf.nv_graphic_frame_pr.map(|nv| nv.cnv_pr)),
        placeholder: None,
        transform: gf.xfrm.map(Into::into),
        transform_inherited: false,
        slide_rect: None,
        // §19.3.1.21: `p:graphicFrame` has no `style` child in the schema.
        style: None,
        kind: ShapeKind::GraphicFrame(Box::new(GraphicFrame { payload })),
    }
}

fn lower_table(tbl: TblXml) -> Table {
    let pr = tbl.tbl_pr.unwrap_or_default();
    let flag = |b: Option<crate::docx::parse::primitives::toggles::AttrBool>| {
        b.map(|b| b.0).unwrap_or(false)
    };
    Table {
        grid: tbl
            .tbl_grid
            .unwrap_or_default()
            .cols
            .into_iter()
            .map(|c| c.w)
            .collect(),
        rows: tbl
            .rows
            .into_iter()
            .map(|r| TableRow {
                height: r.h,
                cells: r
                    .cells
                    .into_iter()
                    .map(|c| TableCell {
                        text: c.tx_body.map(TextBodyXml::into_model),
                        // §21.1.3.16: absent means 1, and a declared 0 is
                        // meaningless — coerce it rather than emit a cell that
                        // occupies no columns.
                        grid_span: c.grid_span.unwrap_or(1).max(1),
                        row_span: c.row_span.unwrap_or(1).max(1),
                        h_merge: flag(c.h_merge),
                        v_merge: flag(c.v_merge),
                        properties: c.tc_pr.map(Into::into).unwrap_or_default(),
                    })
                    .collect(),
            })
            .collect(),
        first_row: flag(pr.first_row),
        first_col: flag(pr.first_col),
        band_row: flag(pr.band_row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SchemeColorVal;

    /// Wraps a shape-tree fragment in a namespace-declaring slide part, which
    /// is what `parse_shape_tree` expects — prefixes must resolve in the
    /// document's own scope, not the parent's.
    fn tree(inner: &str) -> Vec<Shape> {
        let xml = format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                      xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"
                      xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">
                 <p:cSld><p:spTree>
                   <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                   <p:grpSpPr/>
                   {inner}
                 </p:spTree></p:cSld>
               </p:sld>"#
        );
        parse_shape_tree(xml.as_bytes()).expect("parses")
    }

    /// A whole part (not just a shape tree), for the elements that hang off
    /// `p:sldMaster`/`p:sld` rather than off `p:cSld`.
    fn part(inner: &str) -> SlidePart {
        let xml = format!(
            r#"<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
                 <p:cSld><p:spTree/></p:cSld>
                 {inner}
               </p:sldMaster>"#
        );
        parse_slide_part(xml.as_bytes()).expect("parses")
    }

    /// A master can swap `bg2`/`tx2`, which is the whole reason this
    /// element cannot be inferred.
    #[test]
    fn swapped_clr_map_is_read() {
        let p = part(
            r#"<p:clrMap bg1="lt1" tx1="dk1" bg2="dk2" tx2="lt2" accent1="accent1"
                         accent2="accent2" accent3="accent3" accent4="accent4"
                         accent5="accent5" accent6="accent6" hlink="hlink"
                         folHlink="folHlink"/>"#,
        );
        let map = p.color_map.expect("stated");
        assert_eq!(map.tx2, SchemeColorVal::Lt2);
        assert_eq!(map.bg2, SchemeColorVal::Dk2);
        // Untouched slots still map the default way.
        assert_eq!(map.tx1, SchemeColorVal::Dk1);
    }

    /// `a:masterClrMapping` *is* "inherit", so it must parse to `None` — a
    /// default map here would shadow the master's real one on every slide
    /// that carries the element.
    #[test]
    fn master_clr_mapping_override_is_inherit_not_default() {
        let p = part(r#"<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr>"#);
        assert_eq!(p.color_map, None);
    }

    #[test]
    fn override_clr_mapping_states_a_map() {
        let p = part(
            r#"<p:clrMapOvr><a:overrideClrMapping bg1="dk1" tx1="lt1" bg2="lt2" tx2="dk2"
                 accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4"
                 accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/>
               </p:clrMapOvr>"#,
        );
        let map = p.color_map.expect("stated");
        assert_eq!(map.tx1, SchemeColorVal::Lt1);
        assert_eq!(map.bg1, SchemeColorVal::Dk1);
    }

    fn sp(inner: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                 {inner}
               </p:sp>"#
        )
    }

    #[test]
    fn sp_tree_scaffolding_is_not_mistaken_for_a_shape() {
        // `nvGrpSpPr` and `grpSpPr` are `$value` siblings of the real members.
        // Without the `#[serde(other)]` arm they would either fail the part or
        // show up as phantom shapes.
        assert!(tree("").is_empty());
    }

    #[test]
    fn document_order_is_preserved_across_kinds() {
        let shapes = tree(&format!(
            r#"{}
               <p:cxnSp><p:nvCxnSpPr><p:cNvPr id="3" name="c"/></p:nvCxnSpPr><p:spPr/></p:cxnSp>
               {}"#,
            sp("<p:spPr/>"),
            sp("<p:spPr/>")
        ));
        assert_eq!(shapes.len(), 3);
        assert!(matches!(shapes[0].kind, ShapeKind::AutoShape(_)));
        assert!(matches!(shapes[1].kind, ShapeKind::Connector(_)));
        assert!(matches!(shapes[2].kind, ShapeKind::AutoShape(_)));
    }

    /// No xfrm means `None`, not a rectangle at the origin. Inventing (0,0)
    /// here would put every unresolved placeholder in the top-left corner
    /// and look like a layout bug rather than a missing cascade.
    #[test]
    fn missing_xfrm_is_none_not_origin() {
        let shapes = tree(&sp(r#"<p:spPr/><p:txBody><a:p/></p:txBody>"#));
        assert!(shapes[0].transform.is_none());
        assert!(shapes[0].needs_inherited_geometry());
    }

    #[test]
    fn explicit_xfrm_is_read_through_sp_pr() {
        let shapes = tree(&sp(
            r#"<p:spPr><a:xfrm><a:off x="914400" y="457200"/><a:ext cx="1828800" cy="685800"/></a:xfrm></p:spPr>"#,
        ));
        let t = shapes[0].transform.expect("transform present");
        assert_eq!(t.offset.expect("offset").x.raw(), 914400);
        assert_eq!(t.extent.expect("extent").width.raw(), 1828800);
        assert!(!shapes[0].needs_inherited_geometry());
    }

    /// §19.3.1.46. All four references, with the `phClr` substitute each one
    /// carries — the substitute is the load-bearing half, since theme matrix
    /// entries are routinely written in terms of `phClr` and resolve to
    /// black without it.
    #[test]
    fn shape_style_lowers_all_four_references() {
        let shapes = tree(&sp(r#"<p:spPr/><p:style>
                 <a:lnRef idx="2"><a:schemeClr val="accent1"><a:shade val="50000"/></a:schemeClr></a:lnRef>
                 <a:fillRef idx="1"><a:schemeClr val="accent1"/></a:fillRef>
                 <a:effectRef idx="0"><a:schemeClr val="accent1"/></a:effectRef>
                 <a:fontRef idx="minor"><a:schemeClr val="lt1"/></a:fontRef>
               </p:style>"#));
        let style = shapes[0].style.as_ref().expect("p:style lowered");
        let ln = style.line_ref.as_ref().expect("lnRef");
        assert_eq!(ln.idx, 2);
        assert!(ln.color.is_some(), "the phClr substitute must survive");
        assert_eq!(style.fill_ref.as_ref().expect("fillRef").idx, 1);
        // §20.1.4.2.19: 0 is the no-reference sentinel and is kept as
        // declared, not normalised away — the resolver needs to see it.
        assert_eq!(style.effect_ref.as_ref().expect("effectRef").idx, 0);
        assert!(style.font_ref.is_some(), "parsed, though unused for now");
    }

    /// A shape with no `p:style` gets `None`, not a default-constructed set of
    /// references — an all-zero `ShapeStyle` would read as "asked the theme and
    /// got nothing" rather than "never asked".
    #[test]
    fn shape_without_style_has_no_references() {
        let shapes = tree(&sp("<p:spPr/>"));
        assert!(shapes[0].style.is_none());
    }

    /// `p:ph@idx` can legally reach `u32::MAX`. A narrower integer would
    /// wrap or hard-fail on it.
    #[test]
    fn placeholder_idx_holds_u32_max() {
        let shapes = tree(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph type="body" idx="4294967295"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#,
        );
        assert_eq!(shapes[0].placeholder.as_ref().unwrap().idx, u32::MAX);
    }

    /// §19.7.10 defaults. Both attributes are routinely absent, and the
    /// cascade matches on them, so leaving them as `Option` would push the
    /// same defaulting into every consumer.
    #[test]
    fn placeholder_defaults_to_body_index_zero() {
        let shapes = tree(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#,
        );
        let ph = shapes[0].placeholder.as_ref().expect("placeholder");
        assert_eq!(ph.kind, PlaceholderKind::Body);
        assert_eq!(ph.idx, 0);
    }

    /// An unrecognised `@type` must not silently become `Body`: that would make
    /// a typo indistinguishable from a real body placeholder and let it match
    /// the wrong master placeholder.
    #[test]
    fn unknown_placeholder_type_is_distinguishable_from_body() {
        let shapes = tree(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph type="bogus"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#,
        );
        assert_eq!(
            shapes[0].placeholder.as_ref().unwrap().kind,
            PlaceholderKind::Unknown
        );
    }

    #[test]
    fn master_collapse_map_matches_python_pptx() {
        use PlaceholderKind::*;
        assert_eq!(CtrTitle.collapsed_for_master(), Title);
        assert_eq!(Title.collapsed_for_master(), Title);
        for body_ish in [
            SubTitle, Obj, Chart, Tbl, ClipArt, Dgm, Media, Pic, SldImg, Body,
        ] {
            assert_eq!(body_ish.collapsed_for_master(), Body);
        }
        for self_mapped in [Dt, Ftr, SldNum] {
            assert_eq!(self_mapped.collapsed_for_master(), self_mapped);
        }
    }

    /// A graphic frame positions itself with `<p:xfrm>` as a direct child,
    /// not through `spPr`. Reading it via `SpPrXml` finds nothing and
    /// stacks every table at the origin.
    #[test]
    fn graphic_frame_transform_comes_from_p_xfrm() {
        let shapes = tree(
            r#"<p:graphicFrame>
                 <p:nvGraphicFramePr><p:cNvPr id="4" name="t"/></p:nvGraphicFramePr>
                 <p:xfrm><a:off x="100" y="200"/><a:ext cx="300" cy="400"/></p:xfrm>
                 <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
                   <a:tbl><a:tblGrid><a:gridCol w="300"/></a:tblGrid></a:tbl>
                 </a:graphicData></a:graphic>
               </p:graphicFrame>"#,
        );
        let t = shapes[0].transform.expect("transform present");
        assert_eq!(t.offset.expect("offset").y.raw(), 200);
    }

    #[test]
    fn table_merges_keep_origin_spans_and_absorbed_flags() {
        let shapes = tree(
            r#"<p:graphicFrame>
                 <p:nvGraphicFramePr><p:cNvPr id="4" name="t"/></p:nvGraphicFramePr>
                 <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
                   <a:tbl>
                     <a:tblPr firstRow="1"/>
                     <a:tblGrid><a:gridCol w="100"/><a:gridCol w="100"/></a:tblGrid>
                     <a:tr h="370840">
                       <a:tc gridSpan="2"><a:txBody><a:p><a:r><a:t>wide</a:t></a:r></a:p></a:txBody></a:tc>
                       <a:tc hMerge="1"><a:txBody><a:p/></a:txBody></a:tc>
                     </a:tr>
                   </a:tbl>
                 </a:graphicData></a:graphic>
               </p:graphicFrame>"#,
        );
        let ShapeKind::GraphicFrame(frame) = &shapes[0].kind else {
            panic!("expected a graphic frame");
        };
        let GraphicFramePayload::Table(table) = &frame.payload else {
            panic!("expected a table payload");
        };
        assert!(table.first_row);
        assert_eq!(table.grid.len(), 2);
        assert_eq!(table.rows[0].height.expect("height").raw(), 370840);
        assert_eq!(table.rows[0].cells[0].grid_span, 2);
        assert!(!table.rows[0].cells[0].is_absorbed());
        // The absorbed cell is *present* in PPTX, unlike an HTML occupancy
        // grid where it is missing. A consumer that does not drop it emits a
        // phantom column.
        assert!(table.rows[0].cells[1].is_absorbed());
    }

    /// `a:tcPr` is where a cell's text placement lives, and it is not a
    /// rare element: non-default margins and non-top anchors are common.
    #[test]
    fn cell_properties_are_read_off_tc_pr() {
        let shapes = tree(
            r#"<p:graphicFrame>
                 <p:nvGraphicFramePr><p:cNvPr id="3" name="t"/></p:nvGraphicFramePr>
                 <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/table">
                   <a:tbl>
                     <a:tblGrid><a:gridCol w="100"/></a:tblGrid>
                     <a:tr h="200">
                       <a:tc><a:txBody><a:p/></a:txBody>
                         <a:tcPr marL="0" marR="12700" marT="1" marB="2" anchor="ctr">
                           <a:lnL/><a:solidFill/>
                         </a:tcPr></a:tc>
                     </a:tr>
                     <a:tr h="200"><a:tc><a:txBody><a:p/></a:txBody></a:tc></a:tr>
                   </a:tbl>
                 </a:graphicData></a:graphic>
               </p:graphicFrame>"#,
        );
        let ShapeKind::GraphicFrame(frame) = &shapes[0].kind else {
            panic!("expected a graphic frame");
        };
        let GraphicFramePayload::Table(table) = &frame.payload else {
            panic!("expected a table payload");
        };

        let pr = &table.rows[0].cells[0].properties;
        // A declared 0 is a real value, not an absent attribute: reading it as
        // `None` would put the spec's 7.2pt back and indent the text.
        assert_eq!(pr.left_margin.expect("marL").raw(), 0);
        assert_eq!(pr.right_margin.expect("marR").raw(), 12700);
        assert_eq!(pr.anchor, Some(TextAnchoringType::Center));
        assert_eq!(pr.vert, None);

        // No `a:tcPr` at all resolves to the same spec defaults as an empty
        // one, so the two need no distinction downstream.
        let bare = &table.rows[1].cells[0].properties;
        assert_eq!(*bare, TableCellProperties::default());
        assert_eq!(bare.text_body_properties().left_inset, None);
    }

    /// The mapping onto `a:bodyPr` is only sound because the two elements share
    /// their inset defaults, so an absent attribute can pass through as `None`.
    /// If either default ever diverges, this is where it shows.
    #[test]
    fn cell_properties_map_onto_body_properties() {
        let pr = TableCellProperties {
            left_margin: Some(Dimension::new(1)),
            top_margin: Some(Dimension::new(2)),
            right_margin: Some(Dimension::new(3)),
            bottom_margin: Some(Dimension::new(4)),
            anchor: Some(TextAnchoringType::Bottom),
            vert: None,
        };
        let bp = pr.text_body_properties();
        assert_eq!(bp.left_inset.unwrap().raw(), 1);
        assert_eq!(bp.top_inset.unwrap().raw(), 2);
        assert_eq!(bp.right_inset.unwrap().raw(), 3);
        assert_eq!(bp.bottom_inset.unwrap().raw(), 4);
        assert_eq!(bp.anchor, Some(TextAnchoringType::Bottom));
        // A cell declares no rotation, wrap or autofit — these are absent
        // because the element cannot carry them, not because they were dropped.
        assert!(bp.rotation.is_none() && bp.wrap.is_none() && bp.auto_fit.is_none());
    }

    /// SmartArt/OLE/chart frames are unsupported payloads. Typing them as
    /// `Unsupported` rather than dropping them keeps the gap countable.
    #[test]
    fn unsupported_graphic_frame_payload_keeps_its_uri() {
        let shapes = tree(
            r#"<p:graphicFrame>
                 <p:nvGraphicFramePr><p:cNvPr id="4" name="d"/></p:nvGraphicFramePr>
                 <a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/diagram"/></a:graphic>
               </p:graphicFrame>"#,
        );
        let ShapeKind::GraphicFrame(frame) = &shapes[0].kind else {
            panic!("expected a graphic frame");
        };
        match &frame.payload {
            GraphicFramePayload::Unsupported { uri } => assert!(uri.ends_with("diagram")),
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn group_child_coordinate_space_is_captured() {
        let shapes = tree(&format!(
            r#"<p:grpSp>
                 <p:nvGrpSpPr><p:cNvPr id="5" name="g"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                 <p:grpSpPr>
                   <a:xfrm>
                     <a:off x="10" y="20"/><a:ext cx="30" cy="40"/>
                     <a:chOff x="50" y="60"/><a:chExt cx="70" cy="80"/>
                   </a:xfrm>
                 </p:grpSpPr>
                 {}
               </p:grpSp>"#,
            sp("<p:spPr/>")
        ));
        assert_eq!(shapes.len(), 1);
        let ShapeKind::Group(group) = &shapes[0].kind else {
            panic!("expected a group");
        };
        // The group's own frame and its children's frame are different spaces;
        // conflating them is the whole reason chOff/chExt are carried.
        assert_eq!(shapes[0].transform.unwrap().offset.unwrap().x.raw(), 10);
        assert_eq!(group.child_offset.expect("chOff").x.raw(), 50);
        assert_eq!(group.child_extent.expect("chExt").width.raw(), 70);
        assert_eq!(group.children.len(), 1);
    }

    /// §19.3.1.23 — the group's own fill, which is what a member's `a:grpFill`
    /// inherits.
    #[test]
    fn group_fill_is_lowered() {
        let shapes = tree(&format!(
            r#"<p:grpSp>
                 <p:nvGrpSpPr><p:cNvPr id="5" name="g"/></p:nvGrpSpPr>
                 <p:grpSpPr><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></p:grpSpPr>
                 {}
               </p:grpSp>"#,
            sp("<p:spPr><a:grpFill/></p:spPr>")
        ));
        let ShapeKind::Group(group) = &shapes[0].kind else {
            panic!("expected a group");
        };
        assert!(matches!(group.fill, Some(DrawingFill::Solid(_))));
        // The member defers rather than declaring nothing: the distinction is
        // the whole of `grpFill`, and a `None` here would be a shape that
        // silently falls through to its `p:style` instead.
        let ShapeKind::AutoShape(member) = &group.children[0].kind else {
            panic!("expected an autoshape");
        };
        assert!(matches!(
            member.properties.as_ref().and_then(|p| p.fill.as_ref()),
            Some(DrawingFill::Group)
        ));
    }

    /// A group with no fill element is the spec's "inherit nothing" — it must
    /// not be confused with one that names `noFill`, which says the same thing
    /// but says it, nor silently become a fill.
    #[test]
    fn group_without_a_fill_lowers_to_none() {
        let shapes = tree(
            r#"<p:grpSp>
                 <p:nvGrpSpPr><p:cNvPr id="5" name="g"/></p:nvGrpSpPr>
                 <p:grpSpPr><a:xfrm><a:off x="1" y="2"/><a:ext cx="3" cy="4"/></a:xfrm></p:grpSpPr>
               </p:grpSp>"#,
        );
        let ShapeKind::Group(group) = &shapes[0].kind else {
            panic!("expected a group");
        };
        assert!(group.fill.is_none());
        // The `xfrm` still has to survive alongside the new fill fields.
        assert_eq!(shapes[0].transform.unwrap().offset.unwrap().x.raw(), 1);
    }

    #[test]
    fn groups_nest_and_visit_is_preorder() {
        let shapes = tree(&format!(
            r#"<p:grpSp><p:nvGrpSpPr><p:cNvPr id="5" name="outer"/></p:nvGrpSpPr><p:grpSpPr/>
                 <p:grpSp><p:nvGrpSpPr><p:cNvPr id="6" name="inner"/></p:nvGrpSpPr><p:grpSpPr/>
                   {}
                 </p:grpSp>
               </p:grpSp>"#,
            sp("<p:spPr/>")
        ));
        let mut names = Vec::new();
        visit_all(&shapes, &mut |s| names.push(s.non_visual.name.clone()));
        assert_eq!(names, vec!["outer", "inner", "s"]);
    }

    /// `mc:AlternateContent` must splice its surviving branch in at the parent's
    /// level. Emitting it as a shape of its own would perturb z-order, which in
    /// PPTX is nothing but document order.
    #[test]
    fn alternate_content_splices_the_winning_branch_in_place() {
        let shapes = tree(&format!(
            r#"{}
               <mc:AlternateContent>
                 <mc:Choice Requires="p14"><p:contentPart r:id="rId9"/></mc:Choice>
                 <mc:Fallback>{}</mc:Fallback>
               </mc:AlternateContent>
               {}"#,
            sp("<p:spPr/>"),
            sp(r#"<p:spPr/><p:txBody><a:p><a:r><a:t>fallback</a:t></a:r></a:p></p:txBody>"#),
            sp("<p:spPr/>")
        ));
        assert_eq!(shapes.len(), 3, "one branch contributes exactly one shape");
        assert_eq!(
            shapes[1].text().expect("text body").plain_text(),
            "fallback",
            "an ink Choice lowers to nothing, so the picture fallback wins"
        );
    }

    /// The case that forced the rule: a `Requires="a14"` Choice holds the
    /// equation's real text runs, its Fallback holds a rasterized picture
    /// of the same equation. MCE says take the Fallback; that silently
    /// drops the text.
    #[test]
    fn alternate_content_prefers_the_branch_with_more_text() {
        let shapes = tree(&format!(
            r#"<mc:AlternateContent>
                 <mc:Choice Requires="a14">{}</mc:Choice>
                 <mc:Fallback><p:pic>
                   <p:nvPicPr><p:cNvPr id="9" name="eq"/><p:cNvPicPr/></p:nvPicPr>
                   <p:blipFill><a:blip r:embed="rId2"/></p:blipFill><p:spPr/>
                 </p:pic></mc:Fallback>
               </mc:AlternateContent>"#,
            sp(r#"<p:spPr/><p:txBody><a:p><a:r><a:t> = </a:t></a:r></a:p></p:txBody>"#)
        ));
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].text().expect("text body").plain_text(), " = ");
    }

    /// With no Fallback, take the Choice rather than dropping the element.
    #[test]
    fn alternate_content_without_fallback_takes_the_choice() {
        let shapes = tree(&format!(
            r#"<mc:AlternateContent>
                 <mc:Choice Requires="a14">{}</mc:Choice>
               </mc:AlternateContent>"#,
            sp(r#"<p:spPr/><p:txBody><a:p><a:r><a:t>choice</a:t></a:r></a:p></p:txBody>"#)
        ));
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].text().expect("text body").plain_text(), "choice");
    }

    /// Ties go to `Choice` — it is the higher-fidelity representation, so a
    /// fallback that merely matches it must not displace it.
    #[test]
    fn alternate_content_tie_goes_to_the_choice() {
        let shapes = tree(&format!(
            r#"<mc:AlternateContent>
                 <mc:Choice Requires="a14">{}</mc:Choice>
                 <mc:Fallback>{}</mc:Fallback>
               </mc:AlternateContent>"#,
            sp(r#"<p:spPr/><p:txBody><a:p><a:r><a:t>same</a:t></a:r></a:p></p:txBody>"#),
            sp(r#"<p:spPr/><p:txBody><a:p><a:r><a:t>same</a:t></a:r></a:p></p:txBody>"#)
        ));
        assert_eq!(shapes.len(), 1, "exactly one branch survives");
    }

    /// A `p:pic` can be a placeholder, and can declare no `xfrm` of its
    /// own, so the cascade is its only source of geometry. Reusing the DOCX
    /// `PictureXml`, which has no `p:nvPr`, drops this silently: the
    /// picture still parses, it just loses its position.
    #[test]
    fn picture_placeholder_survives() {
        let shapes = tree(
            r#"<p:pic>
                 <p:nvPicPr>
                   <p:cNvPr id="7" name="img"/><p:cNvPicPr/>
                   <p:nvPr><p:ph type="pic" idx="13"/></p:nvPr>
                 </p:nvPicPr>
                 <p:blipFill><a:blip r:embed="rId2"/><a:stretch/></p:blipFill>
                 <p:spPr/>
               </p:pic>"#,
        );
        assert!(matches!(shapes[0].kind, ShapeKind::Picture(_)));
        let ph = shapes[0].placeholder.as_ref().expect("placeholder");
        assert_eq!(ph.kind, PlaceholderKind::Pic);
        assert_eq!(ph.idx, 13);
        assert!(shapes[0].needs_inherited_geometry());
    }

    #[test]
    fn picture_blip_relationship_is_kept() {
        let shapes = tree(
            r#"<p:pic>
                 <p:nvPicPr><p:cNvPr id="7" name="img"/><p:cNvPicPr/></p:nvPicPr>
                 <p:blipFill><a:blip r:embed="rId2"/><a:stretch/></p:blipFill>
                 <p:spPr><a:xfrm><a:off x="1" y="2"/><a:ext cx="3" cy="4"/></a:xfrm></p:spPr>
               </p:pic>"#,
        );
        let ShapeKind::Picture(pic) = &shapes[0].kind else {
            panic!("expected a picture");
        };
        assert_eq!(
            pic.blip_fill
                .blip
                .as_ref()
                .and_then(|b| b.embed.as_ref())
                .map(|r| r.as_str()),
            Some("rId2")
        );
        assert_eq!(shapes[0].non_visual.name, "img");
    }

    #[test]
    fn text_box_flag_is_read() {
        let shapes = tree(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr txBox="1"/><p:nvPr/></p:nvSpPr>
                 <p:spPr/></p:sp>"#,
        );
        let ShapeKind::AutoShape(auto) = &shapes[0].kind else {
            panic!("expected an autoshape");
        };
        assert!(auto.is_text_box);
    }

    /// A part with no `p:cSld` is empty, not an error.
    #[test]
    fn part_without_a_shape_tree_is_empty_not_an_error() {
        let xml =
            r#"<p:notes xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"/>"#;
        assert!(parse_shape_tree(xml.as_bytes()).expect("parses").is_empty());
    }

    #[test]
    fn unmodelled_shape_tree_member_does_not_fail_the_part() {
        let shapes = tree(&format!(
            r#"<p:contentPart r:id="rId9"/>{}"#,
            sp("<p:spPr/>")
        ));
        assert_eq!(shapes.len(), 1);
    }

    // ── Background (§19.3.1.1) ───────────────────────────────────────────

    /// Same namespace scaffolding as [`tree`], but with a `p:bg` sibling
    /// ahead of the shape tree, which is where the content model puts it.
    fn part_with_bg(bg: &str) -> SlidePart {
        let xml = format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                      xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
                 <p:cSld>
                   {bg}
                   <p:spTree>
                     <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                     <p:grpSpPr/>
                   </p:spTree>
                 </p:cSld>
               </p:sld>"#
        );
        parse_slide_part(xml.as_bytes()).expect("parses")
    }

    #[test]
    fn bg_pr_solid_fill_is_read() {
        let part = part_with_bg(
            r#"<p:bg><p:bgPr><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill>
                 <a:effectLst/></p:bgPr></p:bg>"#,
        );
        match part.background {
            Some(Background::Properties(DrawingFill::Solid(_))) => {}
            other => panic!("expected a solid bgPr, got {other:?}"),
        }
    }

    /// `bgRef@idx` must survive as declared. 1001 is not a typo and not a
    /// cosmetic index: it selects the theme's *background* fill matrix.
    /// Clamping or defaulting it here would push the bug down into the
    /// resolver.
    #[test]
    fn bg_ref_keeps_its_thousand_offset_index() {
        let part =
            part_with_bg(r#"<p:bg><p:bgRef idx="1001"><a:schemeClr val="bg1"/></p:bgRef></p:bg>"#);
        match part.background {
            Some(Background::Reference(r)) => {
                assert_eq!(r.idx, 1001);
                assert!(r.color.is_some(), "the phClr substitute must survive");
            }
            other => panic!("expected a bgRef, got {other:?}"),
        }
    }

    /// A part that declares no background yields `None`, which the cascade
    /// reads as "look further up" — not as "transparent".
    #[test]
    fn absent_bg_is_none() {
        assert!(part_with_bg("").background.is_none());
    }

    /// A `<p:bgPr>` with no fill child is malformed. It must read as "declares
    /// nothing" so the cascade keeps walking, rather than as an opaque
    /// nothing that would mask the master's real background.
    #[test]
    fn bg_pr_without_a_fill_declares_nothing() {
        let part = part_with_bg(r#"<p:bg><p:bgPr><a:effectLst/></p:bgPr></p:bg>"#);
        assert!(part.background.is_none());
    }

    /// `<a:noFill>` is a real declaration — an explicitly transparent
    /// background — and must be distinguishable from the absent case above.
    #[test]
    fn bg_pr_no_fill_is_a_declaration_not_an_absence() {
        let part = part_with_bg(r#"<p:bg><p:bgPr><a:noFill/></p:bgPr></p:bg>"#);
        assert!(matches!(
            part.background,
            Some(Background::Properties(DrawingFill::None))
        ));
    }

    /// The background lives beside the shape tree, so reading one must not
    /// cost the other.
    #[test]
    fn background_and_shapes_come_from_one_parse() {
        let xml = r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                            xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
             <p:cSld>
               <p:bg><p:bgPr><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></p:bgPr></p:bg>
               <p:spTree>
                 <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                 <p:grpSpPr/>
                 <p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr>
                   <p:spPr/></p:sp>
               </p:spTree>
             </p:cSld></p:sld>"#;
        let part = parse_slide_part(xml.as_bytes()).expect("parses");
        assert_eq!(part.shapes.len(), 1);
        assert!(part.background.is_some());
    }

    fn show_inherited(root_attrs: &str) -> bool {
        let xml = format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                      xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                      {root_attrs}>
                 <p:cSld><p:spTree>
                   <p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
                   <p:grpSpPr/>
                 </p:spTree></p:cSld>
               </p:sld>"#
        );
        let full = parse_slide_part(xml.as_bytes())
            .expect("parses")
            .show_inherited_shapes;
        // Every case below fences the second reader, not just the one test
        // written for it: `shows_inherited_shapes` exists only while it cannot
        // drift from the parse it shortcuts.
        assert_eq!(
            full,
            shows_inherited_shapes(xml.as_bytes()),
            "cheap reader disagreed on {root_attrs:?}"
        );
        full
    }

    /// The two readers must agree on the whole `AttrBool` true-family and on
    /// the values outside it, which is where a hand-rolled attribute scan is
    /// most likely to go its own way.
    #[test]
    fn cheap_reader_agrees_with_the_full_parse() {
        for attrs in [
            "",
            r#"showMasterSp="0""#,
            r#"showMasterSp="1""#,
            r#"showMasterSp="true""#,
            r#"showMasterSp="false""#,
            r#"showMasterSp="on""#,
            r#"showMasterSp="off""#,
            // Not a value the schema allows. Both sides must still land on the
            // same answer, whatever it is.
            r#"showMasterSp="yes""#,
            // A sibling attribute, to catch a scan that matches by prefix.
            r#"preserve="1""#,
        ] {
            show_inherited(attrs);
        }
    }

    /// The attribute is on the **root**, and a `p:sp` deeper in the part may
    /// carry unrelated attributes. A reader that took the first match anywhere
    /// would read the wrong element.
    #[test]
    fn cheap_reader_reads_the_root_not_a_descendant() {
        let xml = r#"<p:sldLayout xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                                  showMasterSp="0">
                       <p:cSld><p:spTree><p:sp showMasterSp="1"/></p:spTree></p:cSld>
                     </p:sldLayout>"#;
        assert!(!shows_inherited_shapes(xml.as_bytes()));
    }

    /// The schema default is `true`, and it is the *absence* of the attribute
    /// that carries it — a part that says nothing inherits the shapes above it.
    #[test]
    fn show_master_sp_defaults_to_true() {
        assert!(show_inherited(""));
    }

    #[test]
    fn show_master_sp_is_read_when_stated() {
        assert!(!show_inherited(r#"showMasterSp="0""#));
        assert!(show_inherited(r#"showMasterSp="1""#));
    }
}
