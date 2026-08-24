//! SpreadsheetML DrawingML: the pictures floating over a sheet's grid.
//!
//! A worksheet references at most one drawing part (`<drawing r:id>`); the
//! part holds a list of *anchors* (ECMA-376 §20.5), each placing one object —
//! a picture, a shape, a chart frame, or a group — against the grid. This
//! module reads **pictures only**: the corpus census (1,248 workbooks) found
//! 2,276 `xdr:pic` against 386 chart frames and 28,745 shapes, and a chart
//! has no image bytes to extract while a shape's text is a separate content
//! gap, recorded in the plan doc rather than half-solved here.
//!
//! Census-driven decisions:
//!
//! * **All three anchor kinds are read.** `oneCellAnchor` is the majority
//!   (19,228 against 9,157 `twoCellAnchor`), so treating the two-cell form as
//!   canonical would misplace most of the population. `absoluteAnchor` holds
//!   exactly 1 corpus picture but costs ~10 lines.
//! * **A grouped picture inherits its group's anchor.** 161 of 2,276 pictures
//!   sit inside `xdr:grpSp`, whose children carry their own EMU offsets
//!   relative to a child coordinate space. Composing those matrices is the
//!   PPTX group-geometry problem again; here every picture found anywhere in
//!   an anchor's subtree is placed at the anchor's box. The bytes are exact,
//!   the box is the group's — an approximation the geometry pass records.
//! * **`r:embed` resolves against the drawing part's own rels** — the same
//!   per-part scoping rule every other OOXML reader in this crate has had to
//!   learn. When a `blip` also carries an SVG extension, `r:embed` is the
//!   raster fallback and is what we take; an SVG-only blip has no `r:embed`
//!   and yields nothing, matching the PPTX figure policy.

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
}

/// Parse one `xl/drawings/drawingN.xml` part into its placed pictures.
///
/// Fail-open like the rest of the reader: a malformed drawing part costs its
/// pictures, never the workbook.
pub(crate) fn parse_drawing(data: &[u8]) -> Result<Vec<RawPic>> {
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    let mut out = Vec::new();

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
    let mut pending: Vec<(Option<String>, Option<String>)> = Vec::new(); // (name, rel_id)

    // Corner currently being filled and the element text being accumulated.
    let mut corner: Option<bool> = None; // true = from, false = to
    let mut text_target: Option<&'static str> = None;
    let mut text = String::new();

    loop {
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
                        pending.clear();
                    }
                    b"pic" | b"sp" | b"grpSp" | b"graphicFrame" | b"cxnSp"
                        if anchor_kind.is_some() =>
                    {
                        if name == b"pic" {
                            pending.push((None, None));
                            if !empty {
                                pic_depth += 1;
                            }
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
                    // own (the first at its depth) is its display name.
                    b"cNvPr" if pic_depth > 0 => {
                        if let Some(last) = pending.last_mut()
                            && last.0.is_none()
                        {
                            last.0 = attr(e, b"name").filter(|n| !n.is_empty());
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
                        for (name, rel_id) in pending.drain(..) {
                            if let Some(rel_id) = rel_id {
                                out.push(RawPic {
                                    anchor,
                                    name: name.clone(),
                                    rel_id,
                                });
                            }
                        }
                    }
                }
                b"pic" if pic_depth > 0 => {
                    pic_depth -= 1;
                    object_depth = object_depth.saturating_sub(1);
                }
                b"sp" | b"grpSp" | b"graphicFrame" | b"cxnSp" => {
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
    Ok(out)
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
        let pics = parse_drawing(xml.as_bytes()).unwrap();
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
        let pics = parse_drawing(xml.as_bytes()).unwrap();
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
        let pics = parse_drawing(xml.as_bytes()).unwrap();
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
        assert!(parse_drawing(xml.as_bytes()).unwrap().is_empty());
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
        let pics = parse_drawing(xml.as_bytes()).unwrap();
        assert_eq!(
            pics[0].anchor,
            PicAnchor::Absolute {
                pos_emu: (100000, 200000),
                ext_emu: (300000, 400000)
            }
        );
    }

    /// A chart frame is not a picture and yields nothing — the census's 386
    /// `graphicFrame`s are recorded as out of scope, not silently absorbed.
    #[test]
    fn a_chart_frame_yields_no_picture() {
        let xml = format!(
            r#"<xdr:wsDr {NS}><xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>5</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>10</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>
                 <xdr:graphicFrame><a:graphic><a:graphicData uri="chart"/></a:graphic></xdr:graphicFrame>
               <xdr:clientData/></xdr:twoCellAnchor></xdr:wsDr>"#
        );
        assert!(parse_drawing(xml.as_bytes()).unwrap().is_empty());
    }
}
