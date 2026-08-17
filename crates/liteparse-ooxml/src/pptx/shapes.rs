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
//! ## What the corpus says this has to handle
//!
//! Census over the 45-deck corpus, all 2,980 shape trees (slides, layouts,
//! masters, notes slides, notes masters):
//!
//! | child | count | note |
//! |---|---|---|
//! | `p:sp` | 15,906 | 14,957 carry a text body |
//! | `p:pic` | 2,418 | never carries text |
//! | `p:cxnSp` | 845 | never carries text — connectors are geometry only |
//! | `p:grpSp` | 695 | nests to **depth 5** |
//! | `p:graphicFrame` | 102 | diagram 53, table 36, ole 11, chartex 3 |
//! | `mc:AlternateContent` | 10 | see below |
//!
//! Two invariants worth asserting rather than assuming, both measured:
//!
//! - **Every shape lacking an `xfrm` carries a `p:ph`** — 2,422 `p:sp` and 3
//!   `p:pic`, with no counterexample of any kind. So the placeholder cascade is
//!   exactly the missing-geometry case, with no second fallback needed.
//!   [`Shape::needs_inherited_geometry`] names it, and `pptx_shape_probe`
//!   fails if a deck ever breaks it. Note the 3 pictures: they are why this
//!   module models `p:pic` itself instead of reusing the DOCX `PictureXml`,
//!   which has no slot for a placeholder — see [`PicXml`].
//! - **`p:graphicFrame` positions itself with `<p:xfrm>`, not `<a:xfrm>` —
//!   102 of 102.** Different namespace and a different parent element, so it
//!   does *not* arrive through [`SpPrXml`] like every other shape's transform.
//!   Missing this yields graphic frames silently stacked at the origin.

use serde::Deserialize;

use crate::model::dimension::{Dimension, Emu};
use crate::model::geometry::{Offset, Size};
use crate::model::{DocProperties, NvPicProperties, Picture, ShapeProperties, Transform2D};

use crate::docx::error::Result;
use crate::docx::parse::drawing::schema::fill::BlipFillXml;
use crate::docx::parse::drawing::schema::picture::{CNvPicPrXml, CNvPrXml};
use crate::docx::parse::drawing::schema::shape::{ExtXml, OffXml, SpPrXml, XfrmXml};
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
    pub kind: ShapeKind,
}

impl Shape {
    /// True when this shape declares no transform of its own and therefore
    /// depends on the placeholder cascade for its position and size.
    ///
    /// On the corpus this is true for 2,422 shapes and every one of them is a
    /// placeholder, which is what makes the cascade a total function over this
    /// set rather than a best-effort one.
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
    /// §19.3.1.19 `p:cxnSp` — a connector. Carries no text in any of the 845
    /// corpus instances; kept because it is real geometry a layout pass and a
    /// screenshot both need.
    Connector(Box<Connector>),
    /// §19.3.1.22 `p:grpSp`. Boxed: `Group` is by far the largest variant
    /// (~880 B), and there are 16.5k shapes on the corpus.
    Group(Box<Group>),
    /// §19.3.1.21 `p:graphicFrame`.
    GraphicFrame(Box<GraphicFrame>),
}

/// §19.3.1.43 `p:sp`.
#[derive(Clone, Debug, Default)]
pub struct AutoShape {
    pub properties: Option<ShapeProperties>,
    /// `p:txBody`. Absent on 949 of 15,906 corpus shapes — a shape with no
    /// text body is not the same as one with an empty body.
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
    pub properties: Option<ShapeProperties>,
    /// `a:chOff` — the origin of the child coordinate space.
    pub child_offset: Option<Offset<Emu>>,
    /// `a:chExt` — the extent of the child coordinate space.
    pub child_extent: Option<Size<Emu>>,
    /// In document order, which is z-order. Nests to depth 5 on the corpus.
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
    /// A DrawingML table (`a:tbl`) — 36 of 102 corpus frames.
    Table(Box<Table>),
    /// SmartArt (53), embedded OLE (11) or a chart (2).
    ///
    /// Kept as a typed variant rather than dropped so that a caller can emit a
    /// placeholder and a probe can *count* what is being lost. Silently
    /// discarding them would make the gap invisible, which is the failure mode
    /// this vendor has been bitten by repeatedly.
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
    /// only header signal PPTX gives; 15 of 36 corpus tables set it.
    pub first_row: bool,
    pub first_col: bool,
    pub band_row: bool,
}

/// §21.1.3.18 `a:tr`.
#[derive(Clone, Debug, Default)]
pub struct TableRow {
    /// `@h` — declared row height. Present on 221 of 221 corpus rows, and it
    /// is a *minimum*: the rendered row grows to fit its content.
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
}

impl TableCell {
    /// True when this cell is covered by a neighbour's merge and must not
    /// occupy a slot in an occupancy grid.
    pub fn is_absorbed(&self) -> bool {
        self.h_merge || self.v_merge
    }
}

/// §19.3.1.36 `p:ph` — the placeholder a shape fills.
///
/// Both attributes are optional in the file and both have spec defaults that
/// the cascade depends on, so they are materialized here rather than left as
/// `Option` for every consumer to re-derive. On the corpus `@type` is absent
/// 639 times and `@idx` 1,866 times, so neither default is hypothetical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    /// `@type`, defaulting to `body` per §19.7.10.
    pub kind: PlaceholderKind,
    /// `@idx`, defaulting to 0 per §19.7.10.
    ///
    /// **`u32`, not a narrower integer.** The corpus contains `idx` values of
    /// exactly 4294967295 (`u32::MAX`) — 137 of them across 13 of 45 decks.
    /// It is a legal `xsd:unsignedInt`, so anything narrower silently wraps or
    /// hard-fails on more than a quarter of real decks.
    pub idx: u32,
}

/// §19.7.10 ST_PlaceholderType.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PlaceholderKind {
    Title,
    /// The `@type` default, and the most common value on the corpus.
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
    /// The three rungs of the cascade do **not** match the same way: slide →
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
pub fn parse_shape_tree(data: &[u8]) -> Result<Vec<Shape>> {
    let part: SlidePartXml = serde_xml::from_xml(data)?;
    let Some(tree) = part.c_sld.and_then(|c| c.sp_tree) else {
        return Ok(Vec::new());
    };
    Ok(lower_children(tree.children))
}

// ── Schema ───────────────────────────────────────────────────────────────────
//
// As in `pptx::text`, coverage is driven by a census over the 45-deck corpus
// rather than by reading §19.3 front to back: every element and attribute below
// appears in at least one real deck.

#[derive(Deserialize)]
struct SlidePartXml {
    #[serde(rename = "cSld", default)]
    c_sld: Option<CSldXml>,
}

#[derive(Deserialize)]
struct CSldXml {
    #[serde(rename = "spTree", default)]
    sp_tree: Option<SpTreeXml>,
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
/// its `@Requires`, else fall through to `Fallback`. We understand none of the
/// four the corpus uses (`v`, `p14`, `cx1`, `a14`), so that rule always picks
/// `Fallback`. It is the right rule for a *renderer* — the fallback is what an
/// old viewer is meant to draw — and the wrong one for a text extractor,
/// because the fallback is routinely a **rasterized picture of the content**.
///
/// The corpus case that caught it: a slide equation is written as
/// `mc:Choice Requires="a14"` holding a `p:sp` whose paragraph interleaves
/// `<a14:m><m:oMath>` blocks with real `<a:r>` runs, beside an `mc:Fallback`
/// holding a `p:pic` of the same equation. Preferring the fallback dropped the
/// two `<a:t> = </a:t>` runs between the math blocks — a 2-character corpus
/// recall miss, invisible until OMML was split out of the denominator.
///
/// Comparing text also handles the opposite case without a second rule:
/// `mc:Choice` holding a `p:contentPart` (ink, 5 on the corpus) lowers to
/// nothing, so its `p:pic` fallback wins on its own merits.
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
/// Mirrors the shared `PictureXml` field for field — same `CT_Picture`, and the
/// corpus confirms `nvPicPr` and `blipFill` are present on 2,437 of 2,437 — but
/// adds the one thing PresentationML has and WordprocessingML does not: a
/// picture can be a **placeholder**.
///
/// Reusing `PictureXml` directly costs the `p:ph` on 55 corpus pictures, 3 of
/// which declare no `xfrm` at all and therefore have no geometry from any other
/// source. Those 3 are the entire reason the "every shape without an `xfrm` is a
/// placeholder" invariant appeared to break when this walk first ran.
#[derive(Debug, Deserialize)]
struct PicXml {
    #[serde(rename = "nvPicPr")]
    nv_pic_pr: PicNvPrXml,
    #[serde(rename = "blipFill")]
    blip_fill: BlipFillXml,
    #[serde(rename = "spPr", default)]
    sp_pr: Option<SpPrXml>,
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

#[derive(Debug, Deserialize, Default)]
struct GrpSpPrXml {
    #[serde(rename = "xfrm", default)]
    xfrm: Option<GroupXfrmXml>,
    #[serde(rename = "@bwMode", default)]
    _bw_mode: Option<String>,
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
        kind: ShapeKind::Connector(Box::new(Connector { properties })),
    }
}

fn lower_grp(grp: GrpSpXml) -> Shape {
    let xfrm = grp.grp_sp_pr.and_then(|p| p.xfrm);
    let transform = xfrm.as_ref().map(GroupXfrmXml::transform);
    Shape {
        non_visual: doc_properties(grp.nv_grp_sp_pr.map(|nv| nv.cnv_pr)),
        placeholder: None,
        transform,
        transform_inherited: false,
        slide_rect: None,
        kind: ShapeKind::Group(Box::new(Group {
            properties: None,
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
    let payload = match data.tbl {
        Some(tbl) => GraphicFramePayload::Table(Box::new(lower_table(tbl))),
        None => GraphicFramePayload::Unsupported { uri: data.uri },
    };
    Shape {
        non_visual: doc_properties(gf.nv_graphic_frame_pr.map(|nv| nv.cnv_pr)),
        placeholder: None,
        transform: gf.xfrm.map(Into::into),
        transform_inherited: false,
        slide_rect: None,
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

    /// The 2422/2422 corpus invariant, in miniature: no xfrm means `None`, not
    /// a rectangle at the origin. Inventing (0,0) here would put every
    /// unresolved placeholder in the top-left corner and look like a layout
    /// bug rather than a missing cascade.
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

    /// `p:ph@idx` reaches exactly `u32::MAX` in 13 of the 45 corpus decks.
    /// A narrower integer wraps or hard-fails on all of them.
    #[test]
    fn placeholder_idx_holds_u32_max() {
        let shapes = tree(&format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph type="body" idx="4294967295"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#
        ));
        assert_eq!(shapes[0].placeholder.as_ref().unwrap().idx, u32::MAX);
    }

    /// §19.7.10 defaults. Both are absent on thousands of corpus shapes, and
    /// the cascade matches on them, so leaving them as `Option` would push the
    /// same defaulting into every consumer.
    #[test]
    fn placeholder_defaults_to_body_index_zero() {
        let shapes = tree(&format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#
        ));
        let ph = shapes[0].placeholder.as_ref().expect("placeholder");
        assert_eq!(ph.kind, PlaceholderKind::Body);
        assert_eq!(ph.idx, 0);
    }

    /// An unrecognised `@type` must not silently become `Body`: that would make
    /// a typo indistinguishable from a real body placeholder and let it match
    /// the wrong master rung.
    #[test]
    fn unknown_placeholder_type_is_distinguishable_from_body() {
        let shapes = tree(&format!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="s"/><p:cNvSpPr/>
                 <p:nvPr><p:ph type="bogus"/></p:nvPr></p:nvSpPr><p:spPr/></p:sp>"#
        ));
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

    /// A graphic frame positions itself with `<p:xfrm>` as a direct child, not
    /// through `spPr` — 102/102 on the corpus. Reading it via `SpPrXml` finds
    /// nothing and stacks every table at the origin.
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

    /// SmartArt/OLE/chart frames are 66 of 102 on the corpus. Typing them as
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

    /// The corpus case that forced the rule: a `Requires="a14"` Choice holds
    /// the equation's real text runs, its Fallback holds a rasterized picture
    /// of the same equation. MCE says take the Fallback; that silently drops
    /// the text.
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

    /// A `p:pic` can be a placeholder — 55 on the corpus — and 3 of them
    /// declare no `xfrm`, so the cascade is their only source of geometry.
    /// Reusing the DOCX `PictureXml`, which has no `p:nvPr`, drops this
    /// silently: the picture still parses, it just loses its position.
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

    /// A part with no `p:cSld` is empty, not an error — the fail-open posture
    /// this vendor has settled on four times over.
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
}
