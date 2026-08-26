//! SpreadsheetML DrawingML: the pictures and text shapes floating over a
//! sheet's grid.
//!
//! A worksheet references at most one drawing part (`<drawing r:id>`); the
//! part holds a list of *anchors* (ECMA-376 §20.5), each placing one object —
//! a picture, a shape, a chart frame, or a group — against the grid. This
//! module reads **pictures** and **text-bearing shapes**. Charts have no
//! image bytes to extract and stay out of scope.
//!
//! Decisions for shapes:
//!
//! * **`hidden="1"` shapes are skipped.** These are almost always legacy
//!   form controls (`name="Check Box N"`, zero-extent, stacked option labels
//!   like `YES`/`NO`/`UNKNOWN`) that Excel itself never renders.
//! * **`mc:Fallback` subtrees are skipped** — the same prefer-Choice rule
//!   used by the PPTX shape walk. Reading both branches double-places any
//!   object that appears in each.
//! * **`cxnSp` text and `a:hlinkClick`/`a:fld` resolution are not read** —
//!   both are schema-legal but effectively unused in practice.
//!
//! Decisions for pictures:
//!
//! * **All three anchor kinds are read.** `oneCellAnchor` is the majority
//!   form in practice, so treating the two-cell form as canonical would
//!   misplace most pictures. `absoluteAnchor` is rare but cheap to support.
//! * **A grouped picture inherits its group's anchor.** A picture inside
//!   `xdr:grpSp` carries its own EMU offset relative to a child coordinate
//!   space. Composing those matrices is the same problem as PPTX group
//!   geometry; every picture found anywhere in an anchor's subtree is placed
//!   at the anchor's box by default, refined to a fraction of that box when
//!   [`compose_grouped_pics`] can resolve the group's transform.
//! * **`r:embed` resolves against the drawing part's own rels** — the
//!   per-part scoping rule every OOXML reader in this crate follows. When a
//!   `blip` also carries an SVG extension, `r:embed` is the raster fallback
//!   and is what we take; an SVG-only blip has no `r:embed` and yields
//!   nothing, matching the PPTX figure policy.

use std::sync::Arc;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::docx::error::Result;
use crate::model::ImageFormat;
use crate::xlsx::xml::{attr, local_name};

/// One corner of a cell anchor: a grid position plus an EMU offset into it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAnchor {
    /// Zero-based column.
    pub col: u32,
    pub col_off_emu: i64,
    /// Zero-based row.
    pub row: u32,
    pub row_off_emu: i64,
}

/// Where an anchor places its object, in the three forms §20.5.2 allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PicAnchor {
    /// `xdr:oneCellAnchor`: a top-left cell plus an explicit extent.
    OneCell {
        from: CellAnchor,
        ext_emu: (i64, i64),
    },
    /// `xdr:twoCellAnchor`: a top-left and bottom-right cell.
    TwoCell { from: CellAnchor, to: CellAnchor },
    /// `xdr:absoluteAnchor`: a canvas position, ignoring the grid.
    Absolute {
        pos_emu: (i64, i64),
        ext_emu: (i64, i64),
    },
}

impl PicAnchor {
    /// The anchor's top-left grid corner, `None` for the absolute form.
    /// Sorting and page assignment key on this.
    pub fn from_cell(&self) -> Option<CellAnchor> {
        match self {
            PicAnchor::OneCell { from, .. } | PicAnchor::TwoCell { from, .. } => Some(*from),
            PicAnchor::Absolute { .. } => None,
        }
    }
}

/// One picture placed on a sheet, bytes in hand.
#[derive(Clone, Debug)]
pub struct SheetPic {
    pub anchor: PicAnchor,
    /// For a picture inside a `xdr:grpSp`: its composed box as fractions
    /// `[x, y, w, h]` of the anchor's own box. The anchor places the whole
    /// group; the fraction is the pic's `a:xfrm` folded through every
    /// enclosing group's child space by the shared geometry pass — the same
    /// composition, over the same root bounding box, that places the group's
    /// ink, so the two cannot drift apart. `None` for a top-level picture
    /// (the anchor box *is* its box) and for the fail-open cases — a tree the
    /// shared model cannot parse, a degenerate root box — which keep the
    /// group-box approximation this field replaced.
    pub frac: Option<[f64; 4]>,
    /// `xdr:cNvPr@name`, when the producer wrote one.
    pub name: Option<String>,
    /// Package path of the media part — the dedup key across placements.
    pub media_path: String,
    pub format: ImageFormat,
    /// Shared with every other placement of the same media part.
    pub bytes: Arc<Vec<u8>>,
}

/// A picture as the drawing part states it, before media resolution.
pub(crate) struct RawPic {
    pub(crate) anchor: PicAnchor,
    pub(crate) name: Option<String>,
    pub(crate) rel_id: String,
    /// See [`SheetPic::frac`]. Filled by [`compose_grouped_pics`].
    pub(crate) frac: Option<[f64; 4]>,
    /// Index into the part's anchors, the join key to the ink slice the
    /// composed box is derived from.
    anchor_idx: usize,
    /// The pic sat inside at least one `xdr:grpSp`.
    grouped: bool,
}

/// One visible text-bearing shape placed on a sheet. Unlike a picture there
/// is nothing to resolve — the text is complete in the drawing part — so
/// this is the final form, not a `Raw*` intermediate.
#[derive(Clone, Debug)]
pub struct SheetShape {
    pub anchor: PicAnchor,
    /// `xdr:cNvPr@name`, when the producer wrote one.
    pub name: Option<String>,
    /// The shape's `xdr:txBody`, lowered through the shared DrawingML text
    /// model — the same `CT_TextBody` a PPTX shape carries.
    pub body: crate::pptx::text::TextBody,
}

/// One top-level drawing object on the *paint* channel of a drawing part —
/// parallel to `shapes` (text) and `pics` (images) and feeding neither.
///
/// Holds the object's verbatim XML rather than a parsed tree: a plain parse
/// never paints, and parsing 25,948 corpus shape trees it will never look at
/// measured at +67% on the most shape-heavy workbook. [`SheetInk::shape`]
/// parses on demand — the painter is the only caller.
#[derive(Clone, Debug)]
pub struct SheetInk {
    pub anchor: PicAnchor,
    /// Index into the part's anchors — how [`compose_grouped_pics`] joins a
    /// grouped picture back to the slice its group's transforms live in
    /// (anchors can repeat a box, so the anchor value itself is not a key).
    anchor_idx: usize,
    /// The `sp`/`grpSp`/`cxnSp` element, sliced verbatim out of the part.
    xml: Vec<u8>,
}

impl SheetInk {
    /// The object as the shared DrawingML shape tree — groups, child spaces,
    /// MCE and all. `None` for an object the shared model cannot parse
    /// (fail-open: it costs its ink, nothing else).
    ///
    /// A *tree* rather than a flattened list because the placement of a
    /// grouped child only exists relative to its group's child space
    /// (`chOff`/`chExt`), and composing that is the geometry pass's job
    /// ([`crate::pptx::apply_slide_geometry`]), not the reader's. Transforms
    /// are as declared, in the drawing's EMU space; the anchor is what
    /// places the tree's outer box on the page.
    pub fn shape(&self) -> Option<crate::pptx::shapes::Shape> {
        crate::pptx::shapes::parse_single_object(&self.xml)
            .ok()
            .flatten()
    }
}

/// Everything one drawing part yields.
#[derive(Default)]
pub(crate) struct DrawingContent {
    pub(crate) pics: Vec<RawPic>,
    pub(crate) shapes: Vec<SheetShape>,
    pub(crate) ink: Vec<SheetInk>,
}

/// An `xdr:sp` currently open in the walk, before its anchor is known.
#[derive(Default)]
struct OpenShape {
    name: Option<String>,
    hidden: bool,
    body: Option<crate::pptx::text::TextBody>,
    seen_cnvpr: bool,
}

/// Flat text rescue for OMML equation bodies.
///
/// A shape may hold its text in `a14:m` math — `m:oMath` trees whose glyphs
/// sit in `m:t` runs the structured text model does not know, so the body
/// lowers to empty. This collects every `t`-element's text in document order
/// into one run per `a:p` (`m:t` and `a:t` both strip to `t` under
/// `local_name`). Math *structure* — fractions, sub/superscripts — flattens
/// to its glyph sequence; recorded, not hidden. Returns `None` when no text
/// is found at all.
fn flat_math_body(txbody: &[u8]) -> Option<crate::pptx::text::TextBody> {
    use crate::model::{Inline, RunElement, TextRun};
    use crate::pptx::text::{TextBody, TextParagraph};

    let mut reader = Reader::from_reader(txbody);
    let mut buf = Vec::new();
    let mut paras: Vec<String> = Vec::new();
    let mut in_t = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) => return None,
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => match local_name(e.name().as_ref()) {
                b"p" => paras.push(String::new()),
                b"t" => in_t = true,
                _ => {}
            },
            Ok(Event::Text(ref t)) if in_t => {
                if let (Some(last), Ok(txt)) = (paras.last_mut(), t.decode()) {
                    last.push_str(&txt);
                }
            }
            Ok(Event::End(ref e)) => {
                if local_name(e.name().as_ref()) == b"t" {
                    in_t = false;
                }
            }
            Ok(_) => {}
        }
        buf.clear();
    }
    if paras.iter().all(|p| p.trim().is_empty()) {
        return None;
    }
    Some(TextBody {
        body_pr: None,
        list_style: Default::default(),
        paragraphs: paras
            .into_iter()
            .map(|text| TextParagraph {
                properties: Default::default(),
                content: vec![Inline::TextRun(Box::new(TextRun {
                    style_id: None,
                    properties: Default::default(),
                    content: vec![RunElement::Text(text)],
                    rsids: Default::default(),
                }))],
                end_run_properties: None,
            })
            .collect(),
    })
}

/// Parse one `xl/drawings/drawingN.xml` part into its placed pictures and
/// text shapes.
///
/// Fail-open like the rest of the reader: a malformed drawing part costs its
/// pictures and shapes, never the workbook.
pub(crate) fn parse_drawing(data: &[u8]) -> Result<DrawingContent> {
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    let mut skip_buf = Vec::new();
    let mut out = DrawingContent::default();

    // State for the anchor currently open. `object_depth` counts how far
    // inside a placed object (pic/sp/grpSp/graphicFrame/cxnSp) the cursor is:
    // the anchor's own `from`/`to`/`ext`/`pos` children are only read at
    // depth 0, because `ext` also names `a:ext` inside every shape transform
    // and `local_name` cannot tell the prefixes apart.
    let mut anchor_kind: Option<&'static str> = None;
    let mut from = CellAnchor::default();
    let mut to = CellAnchor::default();
    let mut has_to = false;
    let mut ext: Option<(i64, i64)> = None;
    let mut pos: Option<(i64, i64)> = None;
    let mut object_depth: u32 = 0;
    let mut pic_depth: u32 = 0;
    // Open `xdr:grpSp` elements above the cursor. A pic that opens while this
    // is non-zero is a *grouped* pic: its box is a fraction of the anchor's,
    // composed later by `compose_grouped_pics`.
    let mut group_depth: u32 = 0;
    // Every anchor's resolved placement, in document order — the join key the
    // ink pass uses to place its object slices.
    let mut anchors: Vec<PicAnchor> = Vec::new();
    let mut pending: Vec<(Option<String>, Option<String>, bool)> = Vec::new(); // (name, rel_id, grouped)

    // The sp currently open, if any. `xdr:sp` cannot nest (only groups
    // nest), so one slot suffices; like `pending`, its result waits for the
    // anchor's End event, where the placement is finally known.
    let mut cur_sp: Option<OpenShape> = None;
    let mut pending_shapes: Vec<(Option<String>, crate::pptx::text::TextBody)> = Vec::new();

    // Corner currently being filled and the element text being accumulated.
    let mut corner: Option<bool> = None; // true = from, false = to
    let mut text_target: Option<&'static str> = None;
    let mut text = String::new();

    loop {
        // Byte offset where the next event's own bytes begin — whitespace
        // between tags is its own Text event, so after any event this is
        // exactly the start of the following tag. Used to slice a txBody
        // element out of the part verbatim.
        let tag_start = reader.buffer_position() as usize;
        let event = reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?;
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let qname = e.name();
                let name = local_name(qname.as_ref());
                let empty = matches!(event, Event::Empty(_));
                match name {
                    // MCE: keep the Choice branch, skip the Fallback.
                    // Reading both double-places any object that appears in
                    // each branch.
                    b"Fallback" if !empty => {
                        let end = e.to_end().into_owned();
                        reader
                            .read_to_end_into(end.name(), &mut skip_buf)
                            .map_err(quick_xml::DeError::from)?;
                    }
                    b"oneCellAnchor" | b"twoCellAnchor" | b"absoluteAnchor" if !empty => {
                        anchor_kind = Some(match name {
                            b"oneCellAnchor" => "one",
                            b"twoCellAnchor" => "two",
                            _ => "abs",
                        });
                        from = CellAnchor::default();
                        to = CellAnchor::default();
                        has_to = false;
                        ext = None;
                        pos = None;
                        object_depth = 0;
                        pic_depth = 0;
                        group_depth = 0;
                        pending.clear();
                        pending_shapes.clear();
                        cur_sp = None;
                    }
                    b"pic" | b"sp" | b"grpSp" | b"graphicFrame" | b"cxnSp"
                        if anchor_kind.is_some() =>
                    {
                        if name == b"pic" {
                            pending.push((None, None, group_depth > 0));
                            if !empty {
                                pic_depth += 1;
                            }
                        }
                        if name == b"sp" && !empty {
                            cur_sp = Some(OpenShape::default());
                        }
                        if name == b"grpSp" && !empty {
                            group_depth += 1;
                        }
                        if !empty {
                            object_depth += 1;
                        }
                    }
                    b"from" if anchor_kind.is_some() && object_depth == 0 && !empty => {
                        corner = Some(true);
                    }
                    b"to" if anchor_kind.is_some() && object_depth == 0 && !empty => {
                        corner = Some(false);
                        has_to = true;
                    }
                    b"col" | b"colOff" | b"row" | b"rowOff" if corner.is_some() && !empty => {
                        text_target = Some(match name {
                            b"col" => "col",
                            b"colOff" => "colOff",
                            b"row" => "row",
                            _ => "rowOff",
                        });
                        text.clear();
                    }
                    b"ext" if anchor_kind.is_some() && object_depth == 0 => {
                        let cx = attr(e, b"cx").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let cy = attr(e, b"cy").and_then(|v| v.parse().ok()).unwrap_or(0);
                        ext = Some((cx, cy));
                    }
                    b"pos" if anchor_kind.is_some() && object_depth == 0 => {
                        let x = attr(e, b"x").and_then(|v| v.parse().ok()).unwrap_or(0);
                        let y = attr(e, b"y").and_then(|v| v.parse().ok()).unwrap_or(0);
                        pos = Some((x, y));
                    }
                    // `cNvPr` also names shapes and groups; only a picture's
                    // or shape's own (the first at its depth) is its display
                    // name. Order matters: an sp closes before a sibling pic
                    // opens, so at most one of the two branches is live.
                    b"cNvPr" if pic_depth > 0 => {
                        if let Some(last) = pending.last_mut()
                            && last.0.is_none()
                        {
                            last.0 = attr(e, b"name").filter(|n| !n.is_empty());
                        }
                    }
                    b"cNvPr" => {
                        if let Some(sh) = cur_sp.as_mut()
                            && !sh.seen_cnvpr
                        {
                            sh.seen_cnvpr = true;
                            sh.name = attr(e, b"name").filter(|n| !n.is_empty());
                            sh.hidden = attr(e, b"hidden").as_deref() == Some("1");
                        }
                    }
                    // The shape's text body, sliced out verbatim and lowered
                    // through the shared DrawingML text model. quick-xml's
                    // serde layer matches local names with prefixes stripped,
                    // so the `xdr:txBody` slice parses without namespace
                    // re-declaration. A body that fails to parse costs its
                    // shape's text, nothing else.
                    b"txBody" if cur_sp.is_some() && pic_depth == 0 && !empty => {
                        let end = e.to_end().into_owned();
                        reader
                            .read_to_end_into(end.name(), &mut skip_buf)
                            .map_err(quick_xml::DeError::from)?;
                        let tag_end = reader.buffer_position() as usize;
                        let slice = &data[tag_start..tag_end];
                        if let Some(sh) = cur_sp.as_mut()
                            && let Ok(body) = crate::pptx::text::parse_text_body(slice)
                        {
                            // An OMML equation body (`a14:m` → `m:t` runs) is
                            // invisible to the structured text model and
                            // lowers to empty; rescue its glyphs flat.
                            sh.body = if body.plain_text().trim().is_empty() {
                                flat_math_body(slice).or(Some(body))
                            } else {
                                Some(body)
                            };
                        }
                    }
                    // The blip inside `xdr:blipFill`. A shape's background
                    // fill also uses `a:blip`, so only capture inside a pic.
                    b"blip" if pic_depth > 0 => {
                        if let Some(last) = pending.last_mut()
                            && last.1.is_none()
                        {
                            last.1 = attr(e, b"r:embed");
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(ref t) if text_target.is_some() => {
                text.push_str(&t.decode().map_err(quick_xml::DeError::from)?);
            }
            Event::End(ref e) => match local_name(e.name().as_ref()) {
                b"oneCellAnchor" | b"twoCellAnchor" | b"absoluteAnchor" => {
                    if let Some(kind) = anchor_kind.take() {
                        let anchor = match kind {
                            "one" => PicAnchor::OneCell {
                                from,
                                ext_emu: ext.unwrap_or((0, 0)),
                            },
                            "two" => {
                                if has_to {
                                    PicAnchor::TwoCell { from, to }
                                } else {
                                    // A malformed two-cell anchor with no
                                    // `to` degrades to a zero-extent one-cell.
                                    PicAnchor::OneCell {
                                        from,
                                        ext_emu: ext.unwrap_or((0, 0)),
                                    }
                                }
                            }
                            _ => PicAnchor::Absolute {
                                pos_emu: pos.unwrap_or((0, 0)),
                                ext_emu: ext.unwrap_or((0, 0)),
                            },
                        };
                        for (name, rel_id, grouped) in pending.drain(..) {
                            if let Some(rel_id) = rel_id {
                                out.pics.push(RawPic {
                                    anchor,
                                    name,
                                    rel_id,
                                    frac: None,
                                    anchor_idx: anchors.len(),
                                    grouped,
                                });
                            }
                        }
                        for (name, body) in pending_shapes.drain(..) {
                            out.shapes.push(SheetShape { anchor, name, body });
                        }
                        anchors.push(anchor);
                    }
                }
                b"pic" if pic_depth > 0 => {
                    pic_depth -= 1;
                    object_depth = object_depth.saturating_sub(1);
                }
                b"sp" => {
                    object_depth = object_depth.saturating_sub(1);
                    if let Some(sh) = cur_sp.take() {
                        // Hidden shapes are legacy form controls Excel never
                        // renders; whitespace-only bodies carry no content.
                        // Both censused, both dropped here.
                        if !sh.hidden
                            && let Some(body) = sh.body
                            && !body.plain_text().trim().is_empty()
                        {
                            pending_shapes.push((sh.name, body));
                        }
                    }
                }
                b"grpSp" => {
                    object_depth = object_depth.saturating_sub(1);
                    group_depth = group_depth.saturating_sub(1);
                }
                b"graphicFrame" | b"cxnSp" => {
                    object_depth = object_depth.saturating_sub(1);
                }
                b"from" | b"to" => corner = None,
                b"col" | b"colOff" | b"row" | b"rowOff" => {
                    if let (Some(target), Some(is_from)) = (text_target.take(), corner) {
                        let c = if is_from { &mut from } else { &mut to };
                        let v = text.trim();
                        match target {
                            "col" => c.col = v.parse().unwrap_or(0),
                            "colOff" => c.col_off_emu = v.parse().unwrap_or(0),
                            "row" => c.row = v.parse().unwrap_or(0),
                            _ => c.row_off_emu = v.parse().unwrap_or(0),
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    out.ink = extract_ink(data, &anchors);
    compose_grouped_pics(&mut out);
    Ok(out)
}

/// Fill [`RawPic::frac`] for every grouped picture whose composed box the
/// shared geometry pass can produce.
///
/// A grouped pic's outermost group is a top-level object, so its verbatim XML
/// is exactly the anchor's ink slice. Parsing that one slice through the
/// shared shape tree and running [`crate::pptx::apply_slide_geometry`] yields
/// each descendant pic's box in the drawing's EMU space — the same
/// composition, over the same root bounding box, that places the group's ink,
/// so pictures and their group's fills land in one frame by construction.
/// The fail-open path — parse failure, a count or `r:embed` sequence
/// disagreement between this tree and the streaming walk, a degenerate root
/// box — falls back to the group-box approximation (the anchor's box, not a
/// fraction of it).
fn compose_grouped_pics(out: &mut DrawingContent) {
    let mut composed: Vec<(usize, [f64; 4])> = Vec::new();
    for ink in &out.ink {
        let targets: Vec<usize> = out
            .pics
            .iter()
            .enumerate()
            .filter(|(_, p)| p.anchor_idx == ink.anchor_idx && p.grouped)
            .map(|(i, _)| i)
            .collect();
        if targets.is_empty() {
            continue;
        }
        let Some(mut root) = ink.shape() else {
            continue;
        };
        crate::pptx::apply_slide_geometry(std::slice::from_mut(&mut root));
        let Some(base) = root.slide_rect.as_ref().map(|sr| sr.bounding_box()) else {
            continue;
        };
        let (bx, by) = (base.origin.x.raw() as f64, base.origin.y.raw() as f64);
        let (bw, bh) = (base.size.width.raw() as f64, base.size.height.raw() as f64);
        if bw <= 0.0 || bh <= 0.0 {
            continue;
        }
        // Document order on both sides, filtered by the same rule (a pic
        // without `r:embed` never became a `RawPic`), verified by the rel
        // sequence itself.
        let mut found: Vec<(String, Option<[f64; 4]>)> = Vec::new();
        collect_tree_pics(&root, &mut found);
        if found.len() != targets.len()
            || found
                .iter()
                .zip(&targets)
                .any(|((rel, _), &pi)| rel != &out.pics[pi].rel_id)
        {
            continue;
        }
        for ((_, bb), pi) in found.into_iter().zip(targets) {
            let Some([x, y, w, h]) = bb else { continue };
            composed.push((pi, [(x - bx) / bw, (y - by) / bh, w / bw, h / bh]));
        }
    }
    for (pi, frac) in composed {
        out.pics[pi].frac = Some(frac);
    }
}

/// Every pic in the tree that carries an `r:embed`, pre-order, with its
/// composed bounding box (`None` when the geometry pass left it unplaced).
fn collect_tree_pics(
    shape: &crate::pptx::shapes::Shape,
    out: &mut Vec<(String, Option<[f64; 4]>)>,
) {
    use crate::pptx::shapes::ShapeKind;
    match &shape.kind {
        ShapeKind::Picture(p) => {
            if let Some(rel) = p.blip_fill.blip.as_ref().and_then(|b| b.embed.as_ref()) {
                let bb = shape.slide_rect.as_ref().map(|sr| {
                    let r = sr.bounding_box();
                    [
                        r.origin.x.raw() as f64,
                        r.origin.y.raw() as f64,
                        r.size.width.raw() as f64,
                        r.size.height.raw() as f64,
                    ]
                });
                out.push((rel.as_str().to_string(), bb));
            }
        }
        ShapeKind::Group(g) => {
            for ch in &g.children {
                collect_tree_pics(ch, out);
            }
        }
        _ => {}
    }
}

/// The paint channel: slice every top-level `sp`/`grpSp`/`cxnSp` out of the
/// part verbatim and parse each through the shared DrawingML shape tree.
///
/// A second pass over the same bytes rather than a branch of the main walk,
/// because the main walk *descends* into an `sp` for its text while this pass
/// consumes the whole subtree in one slice — one cursor cannot do both. The
/// two stay aligned on anchors by construction: both count the same
/// non-empty anchor Start/End events and both skip `mc:Fallback` (the
/// prefer-Choice rule), so anchor *i* here is `anchors[i]` there.
///
/// Fail-open at every level: an object that fails to parse costs its ink,
/// never its sibling's, and never the part's text or pictures.
fn extract_ink(data: &[u8], anchors: &[PicAnchor]) -> Vec<SheetInk> {
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    let mut skip_buf = Vec::new();
    let mut out = Vec::new();
    let mut anchor_idx = 0usize;
    loop {
        let tag_start = reader.buffer_position() as usize;
        let event = match reader.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(_) => return out,
        };
        match event {
            Event::Eof => break,
            Event::Start(ref e) => match local_name(e.name().as_ref()) {
                b"Fallback" => {
                    let end = e.to_end().into_owned();
                    if reader.read_to_end_into(end.name(), &mut skip_buf).is_err() {
                        return out;
                    }
                }
                // Any of these seen here is top-level by construction:
                // nested ones are inside a subtree this arm already consumed.
                b"sp" | b"grpSp" | b"cxnSp" => {
                    let end = e.to_end().into_owned();
                    if reader.read_to_end_into(end.name(), &mut skip_buf).is_err() {
                        return out;
                    }
                    let tag_end = reader.buffer_position() as usize;
                    if let Some(anchor) = anchors.get(anchor_idx) {
                        out.push(SheetInk {
                            anchor: *anchor,
                            anchor_idx,
                            xml: data[tag_start..tag_end].to_vec(),
                        });
                    }
                }
                _ => {}
            },
            Event::End(ref e) => {
                if matches!(
                    local_name(e.name().as_ref()),
                    b"oneCellAnchor" | b"twoCellAnchor" | b"absoluteAnchor"
                ) {
                    anchor_idx += 1;
                }
            }
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = r#"xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

    fn pic(rel: &str, name: &str) -> String {
        format!(
            r#"<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="{name}"/><xdr:cNvPicPr/></xdr:nvPicPr>
               <xdr:blipFill><a:blip r:embed="{rel}"/></xdr:blipFill>
               <xdr:spPr><a:xfrm><a:off x="1" y="2"/><a:ext cx="999" cy="888"/></a:xfrm></xdr:spPr></xdr:pic>"#
        )
    }

    #[test]
    fn a_two_cell_anchor_reads_both_corners() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>1</xdr:col><xdr:colOff>9525</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>19050</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>7</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 {}
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            pic("rId1", "Logo")
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].rel_id, "rId1");
        assert_eq!(pics[0].name.as_deref(), Some("Logo"));
        assert_eq!(
            pics[0].anchor,
            PicAnchor::TwoCell {
                from: CellAnchor {
                    col: 1,
                    col_off_emu: 9525,
                    row: 2,
                    row_off_emu: 19050
                },
                to: CellAnchor {
                    col: 4,
                    col_off_emu: 0,
                    row: 7,
                    row_off_emu: 0
                },
            }
        );
    }

    /// The trap this parser's depth counter exists for: the pic's own
    /// `a:ext cx="999"` must not overwrite the anchor's `xdr:ext`, because
    /// `local_name` strips both prefixes to `ext`.
    #[test]
    fn a_one_cell_anchor_takes_the_anchor_ext_not_the_shape_transform() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>
                 {}
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#,
            pic("rId2", "P")
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(
            pics[0].anchor,
            PicAnchor::OneCell {
                from: CellAnchor::default(),
                ext_emu: (914400, 457200)
            }
        );
    }

    /// Pictures inside a group inherit the group's anchor; the shapes around
    /// them contribute nothing.
    #[test]
    fn grouped_pictures_take_the_group_anchor() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>9</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="Group 1"/></xdr:nvGrpSpPr>
                   <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="Box"/></xdr:nvSpPr><xdr:spPr><a:xfrm><a:ext cx="7" cy="8"/></a:xfrm></xdr:spPr></xdr:sp>
                   {}{}
                 </xdr:grpSp>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            pic("rId3", "A"),
            pic("rId4", "B")
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(pics.len(), 2);
        assert_eq!(pics[0].rel_id, "rId3");
        assert_eq!(pics[0].name.as_deref(), Some("A"));
        assert_eq!(pics[1].rel_id, "rId4");
        let expect = PicAnchor::TwoCell {
            from: CellAnchor {
                col: 3,
                row: 4,
                ..Default::default()
            },
            to: CellAnchor {
                col: 6,
                row: 9,
                ..Default::default()
            },
        };
        assert_eq!(pics[0].anchor, expect);
        assert_eq!(pics[1].anchor, expect);
        // This group declares no `grpSpPr` xfrm, so the composer cannot map
        // its child space and fails open: the anchor box stays the pics' box.
        assert_eq!(pics[0].frac, None);
        assert_eq!(pics[1].frac, None);
    }

    /// A grouped pic with a specific `a:xfrm`, for composition tests.
    fn pic_at(rel: &str, off: (i64, i64), ext: (i64, i64)) -> String {
        format!(
            r#"<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="P"/><xdr:cNvPicPr/></xdr:nvPicPr>
               <xdr:blipFill><a:blip r:embed="{rel}"/></xdr:blipFill>
               <xdr:spPr><a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm></xdr:spPr></xdr:pic>"#,
            off.0, off.1, ext.0, ext.1
        )
    }

    /// The whole point of `compose_grouped_pics`: a pic's `a:xfrm`, folded
    /// through its group's child space, lands as a fraction of the anchor box
    /// instead of filling it.
    #[test]
    fn a_grouped_pic_composes_to_a_fraction_of_the_anchor() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="G"/><xdr:cNvGrpSpPr/></xdr:nvGrpSpPr>
                   <xdr:grpSpPr><a:xfrm><a:off x="1000" y="2000"/><a:ext cx="4000" cy="6000"/>
                     <a:chOff x="0" y="0"/><a:chExt cx="2000" cy="3000"/></a:xfrm></xdr:grpSpPr>
                   {}
                 </xdr:grpSp>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            pic_at("rId1", (500, 750), (1000, 1500))
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(pics.len(), 1);
        let [fx, fy, fw, fh] = pics[0].frac.expect("composed");
        assert!((fx - 0.25).abs() < 1e-9, "fx {fx}");
        assert!((fy - 0.25).abs() < 1e-9, "fy {fy}");
        assert!((fw - 0.5).abs() < 1e-9, "fw {fw}");
        assert!((fh - 0.5).abs() < 1e-9, "fh {fh}");
    }

    /// Nested groups: both child spaces fold.
    #[test]
    fn nested_groups_compose_through_both_child_spaces() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="914400"/>
                 <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="Outer"/><xdr:cNvGrpSpPr/></xdr:nvGrpSpPr>
                   <xdr:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="1000" cy="1000"/>
                     <a:chOff x="0" y="0"/><a:chExt cx="100" cy="100"/></a:xfrm></xdr:grpSpPr>
                   <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="2" name="Inner"/><xdr:cNvGrpSpPr/></xdr:nvGrpSpPr>
                     <xdr:grpSpPr><a:xfrm><a:off x="10" y="10"/><a:ext cx="50" cy="50"/>
                       <a:chOff x="0" y="0"/><a:chExt cx="25" cy="25"/></a:xfrm></xdr:grpSpPr>
                     {}
                   </xdr:grpSp>
                 </xdr:grpSp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#,
            pic_at("rId1", (5, 5), (10, 10))
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(pics.len(), 1);
        let [fx, fy, fw, fh] = pics[0].frac.expect("composed");
        for (got, want) in [(fx, 0.2), (fy, 0.2), (fw, 0.2), (fh, 0.2)] {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }
    }

    /// A pic with no `r:embed` never becomes a `RawPic`; the tree walk
    /// filters by the same rule, so the sibling that does embed still aligns
    /// with its composed box.
    #[test]
    fn a_relless_sibling_does_not_break_the_alignment() {
        let no_rel = r#"<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="9" name="Svg"/><xdr:cNvPicPr/></xdr:nvPicPr>
               <xdr:blipFill><a:blip/></xdr:blipFill>
               <xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="2000" cy="3000"/></a:xfrm></xdr:spPr></xdr:pic>"#;
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="G"/><xdr:cNvGrpSpPr/></xdr:nvGrpSpPr>
                   <xdr:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="4000" cy="6000"/>
                     <a:chOff x="0" y="0"/><a:chExt cx="2000" cy="3000"/></a:xfrm></xdr:grpSpPr>
                   {no_rel}
                   {}
                 </xdr:grpSp>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            pic_at("rId1", (1000, 1500), (1000, 1500))
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(pics.len(), 1, "the rel-less pic is not a RawPic");
        let [fx, fy, fw, fh] = pics[0].frac.expect("composed");
        for (got, want) in [(fx, 0.5), (fy, 0.5), (fw, 0.5), (fh, 0.5)] {
            assert!((got - want).abs() < 1e-9, "{got} vs {want}");
        }
    }

    /// A shape's `a:blip` background fill is not a picture.
    #[test]
    fn a_shape_fill_blip_is_not_captured() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 <xdr:sp><xdr:spPr><a:blipFill><a:blip r:embed="rId9"/></a:blipFill></xdr:spPr></xdr:sp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert!(content.pics.is_empty());
        assert!(content.shapes.is_empty());
    }

    #[test]
    fn an_absolute_anchor_reads_pos_and_ext() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:absoluteAnchor>
                 <xdr:pos x="100000" y="200000"/><xdr:ext cx="300000" cy="400000"/>
                 {}
               <xdr:clientData/></xdr:absoluteAnchor></xdr:wsDr>"#,
            pic("rId5", "Abs")
        );
        let pics = parse_drawing(xml.as_bytes()).unwrap().pics;
        assert_eq!(
            pics[0].anchor,
            PicAnchor::Absolute {
                pos_emu: (100000, 200000),
                ext_emu: (300000, 400000)
            }
        );
    }

    /// A chart frame is not a picture and yields nothing.
    #[test]
    fn a_chart_frame_yields_no_picture() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>5</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>10</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:graphicFrame><a:graphic><a:graphicData uri="chart"/></a:graphic></xdr:graphicFrame>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert!(content.pics.is_empty());
        assert!(content.shapes.is_empty());
    }

    fn text_sp(body: &str) -> String {
        format!(
            r#"<xdr:sp><xdr:nvSpPr><xdr:cNvPr id="7" name="TextBox 1"/><xdr:cNvSpPr txBox="1"/></xdr:nvSpPr>
               <xdr:spPr><a:xfrm><a:off x="1" y="2"/><a:ext cx="999" cy="888"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr>
               <xdr:txBody><a:bodyPr/><a:lstStyle/>{body}</xdr:txBody></xdr:sp>"#
        )
    }

    /// A text shape parses into a lowered body carrying the anchor that was
    /// only known after the shape closed.
    #[test]
    fn a_text_shape_takes_its_anchor_and_lowers_its_body() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>2</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>3</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>5</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>6</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 {}
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            text_sp(
                r#"<a:p><a:r><a:rPr lang="en-US" sz="1100"/><a:t>Section title</a:t></a:r></a:p><a:p><a:r><a:t>second line</a:t></a:r></a:p>"#
            )
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert!(content.pics.is_empty());
        assert_eq!(content.shapes.len(), 1);
        let sh = &content.shapes[0];
        assert_eq!(sh.name.as_deref(), Some("TextBox 1"));
        assert_eq!(
            sh.body.plain_text(),
            "Section title
second line"
        );
        assert_eq!(
            sh.anchor.from_cell(),
            Some(CellAnchor {
                col: 2,
                row: 3,
                ..Default::default()
            })
        );
    }

    /// `hidden="1"` marks a legacy form control (checkbox option labels and
    /// friends) that Excel never renders.
    #[test]
    fn a_hidden_shape_is_a_form_control_and_is_skipped() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="9" name="Check Box 15" hidden="1"/></xdr:nvSpPr>
                   <xdr:txBody><a:bodyPr/><a:p><a:r><a:t>UNKNOWN</a:t></a:r></a:p></xdr:txBody></xdr:sp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        assert!(parse_drawing(xml.as_bytes()).unwrap().shapes.is_empty());
    }

    /// A whitespace-only body carries no content.
    #[test]
    fn a_whitespace_only_body_is_skipped() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 {}
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#,
            text_sp(r#"<a:p><a:r><a:t>  </a:t></a:r></a:p>"#)
        );
        assert!(parse_drawing(xml.as_bytes()).unwrap().shapes.is_empty());
    }

    /// A grouped text shape inherits the group's anchor, like a grouped
    /// picture — and its cNvPr is its own, not the group's.
    #[test]
    fn a_grouped_text_shape_takes_the_group_anchor_and_its_own_name() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>6</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>9</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:grpSp><xdr:nvGrpSpPr><xdr:cNvPr id="1" name="Group 1"/></xdr:nvGrpSpPr>
                   {}
                 </xdr:grpSp>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            text_sp(r#"<a:p><a:r><a:t>label</a:t></a:r></a:p>"#)
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert_eq!(content.shapes.len(), 1);
        assert_eq!(content.shapes[0].name.as_deref(), Some("TextBox 1"));
        assert_eq!(
            content.shapes[0].anchor.from_cell(),
            Some(CellAnchor {
                col: 3,
                row: 4,
                ..Default::default()
            })
        );
    }

    /// MCE: the Choice branch wins, the Fallback is skipped — an object in
    /// both branches yields ONE placement, not two.
    #[test]
    fn mce_prefers_choice_and_never_reads_both_branches() {
        let xml = format!(
            r#"<xdr:wsDr {NS} xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>4</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <mc:AlternateContent>
                   <mc:Choice Requires="a14">{}{}</mc:Choice>
                   <mc:Fallback>{}{}</mc:Fallback>
                 </mc:AlternateContent>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#,
            text_sp(r#"<a:p><a:r><a:t>choice text</a:t></a:r></a:p>"#),
            pic("rId1", "P"),
            text_sp(r#"<a:p><a:r><a:t>fallback text</a:t></a:r></a:p>"#),
            pic("rId1", "P")
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert_eq!(content.pics.len(), 1);
        assert_eq!(content.shapes.len(), 1);
        assert_eq!(content.shapes[0].body.plain_text(), "choice text");
    }

    /// An OMML equation shape keeps its glyphs: the structured model sees an
    /// empty paragraph, and the flat rescue reads the `m:t` runs instead.
    #[test]
    fn an_omml_equation_body_flattens_to_its_glyphs() {
        let xml = format!(
            r#"<xdr:wsDr {NS} xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="4" name="Eq"/></xdr:nvSpPr><xdr:spPr/>
                   <xdr:txBody><a:bodyPr/><a:p><a:pPr/><a14:m><m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:r><m:t>𝑛=</m:t></m:r><m:r><m:t>𝑑1/6</m:t></m:r></m:oMath></a14:m></a:p></xdr:txBody></xdr:sp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert_eq!(content.shapes.len(), 1);
        assert_eq!(content.shapes[0].body.plain_text(), "𝑛=𝑑1/6");
    }

    /// A connector's text is schema-legal but not read here, and its
    /// presence does not disturb a sibling text shape.
    #[test]
    fn a_connector_yields_no_text_shape() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 <xdr:cxnSp><xdr:nvCxnSpPr><xdr:cNvPr id="3" name="Line 1"/></xdr:nvCxnSpPr><xdr:spPr/></xdr:cxnSp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        assert!(parse_drawing(xml.as_bytes()).unwrap().shapes.is_empty());
    }

    // ── the ink channel ──────────────────────────────────────────────────

    use crate::pptx::shapes::ShapeKind;

    /// A textless filled rectangle — dropped by the text channel — survives
    /// on the ink channel, fill and geometry intact, under the anchor it was
    /// placed by.
    #[test]
    fn a_textless_filled_shape_reaches_ink_but_not_shapes() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:oneCellAnchor>
                 <xdr:from><xdr:col>2</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>3</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>
                 <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="Box"/></xdr:nvSpPr>
                   <xdr:spPr><a:prstGeom prst="rect"/><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></xdr:spPr></xdr:sp>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert!(content.shapes.is_empty());
        assert_eq!(content.ink.len(), 1);
        assert_eq!(content.ink[0].anchor.from_cell().unwrap().col, 2);
        let shape = content.ink[0].shape().unwrap();
        let ShapeKind::AutoShape(auto) = &shape.kind else {
            panic!("expected an AutoShape");
        };
        let props = auto.properties.as_ref().unwrap();
        assert!(props.fill.is_some());
        assert!(props.geometry.is_some());
    }

    /// A group keeps its child space and its members — the placement facts
    /// `apply_slide_geometry` composes with — and a connector reaches ink
    /// as a Connector.
    #[test]
    fn a_group_keeps_its_child_space_and_a_connector_reaches_ink() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>4</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:grpSp>
                   <xdr:nvGrpSpPr><xdr:cNvPr id="1" name="G"/></xdr:nvGrpSpPr>
                   <xdr:grpSpPr><a:xfrm><a:off x="100" y="200"/><a:ext cx="1000" cy="2000"/>
                     <a:chOff x="10" y="20"/><a:chExt cx="100" cy="200"/></a:xfrm></xdr:grpSpPr>
                   <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="2" name="A"/></xdr:nvSpPr>
                     <xdr:spPr><a:xfrm><a:off x="10" y="20"/><a:ext cx="50" cy="100"/></a:xfrm>
                       <a:prstGeom prst="ellipse"/><a:solidFill><a:schemeClr val="accent1"/></a:solidFill></xdr:spPr></xdr:sp>
                   <xdr:cxnSp><xdr:nvCxnSpPr><xdr:cNvPr id="3" name="L"/></xdr:nvCxnSpPr>
                     <xdr:spPr><a:prstGeom prst="line"/><a:ln w="9525"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln></xdr:spPr></xdr:cxnSp>
                 </xdr:grpSp>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#
        );
        let content = parse_drawing(xml.as_bytes()).unwrap();
        assert_eq!(content.ink.len(), 1);
        let shape = content.ink[0].shape().unwrap();
        let ShapeKind::Group(group) = &shape.kind else {
            panic!("expected a Group");
        };
        assert_eq!(group.child_offset.unwrap().x.raw(), 10);
        assert_eq!(group.child_extent.unwrap().width.raw(), 100);
        assert_eq!(group.children.len(), 2);
        assert!(matches!(group.children[0].kind, ShapeKind::AutoShape(_)));
        assert!(matches!(group.children[1].kind, ShapeKind::Connector(_)));
    }

    /// Two anchors, two objects: each ink entry carries its own anchor —
    /// the alignment the two-pass design must not lose.
    #[test]
    fn ink_objects_keep_their_own_anchors() {
        let sp = |id: u32| {
            format!(
                r#"<xdr:sp><xdr:nvSpPr><xdr:cNvPr id="{id}" name="S{id}"/></xdr:nvSpPr>
                   <xdr:spPr><a:prstGeom prst="rect"/><a:noFill/></xdr:spPr></xdr:sp>"#
            )
        };
        let xml = format!(
            r#"<xdr:wsDr {NS}>
               <xdr:oneCellAnchor>
                 <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>{}<xdr:clientData/></xdr:oneCellAnchor>
               <xdr:oneCellAnchor>
                 <xdr:from><xdr:col>7</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>9</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>{}<xdr:clientData/></xdr:oneCellAnchor>
               </xdr:wsDr>"#,
            sp(1),
            sp(2)
        );
        let ink = parse_drawing(xml.as_bytes()).unwrap().ink;
        assert_eq!(ink.len(), 2);
        assert_eq!(ink[0].anchor.from_cell().unwrap().col, 1);
        assert_eq!(ink[1].anchor.from_cell().unwrap().col, 7);
        assert_eq!(ink[1].shape().unwrap().non_visual.name, "S2");
    }

    /// MCE on the ink channel follows the reader's prefer-Choice rule: a
    /// `Fallback` object never paints, so nothing is placed twice.
    #[test]
    fn ink_skips_mce_fallback_objects() {
        let xml = format!(
            r#"<xdr:wsDr {NS} xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">
               <xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="1" cy="1"/>
                 <mc:AlternateContent>
                   <mc:Choice Requires="x"><xdr:sp><xdr:nvSpPr><xdr:cNvPr id="1" name="Live"/></xdr:nvSpPr>
                     <xdr:spPr><a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></xdr:spPr></xdr:sp></mc:Choice>
                   <mc:Fallback><xdr:sp><xdr:nvSpPr><xdr:cNvPr id="2" name="Dead"/></xdr:nvSpPr>
                     <xdr:spPr><a:solidFill><a:srgbClr val="0000FF"/></a:solidFill></xdr:spPr></xdr:sp></mc:Fallback>
                 </mc:AlternateContent>
               <xdr:clientData/></xdr:oneCellAnchor></xdr:wsDr>"#
        );
        let ink = parse_drawing(xml.as_bytes()).unwrap().ink;
        assert_eq!(ink.len(), 1);
        assert_eq!(ink[0].shape().unwrap().non_visual.name, "Live");
    }
}
