//! `xl/styles.xml` (§18.8): the indirection from a cell's `s=` to the things a
//! renderer needs — the number-format code, the font, the alignment.
//!
//! The chain is `<c s="7">` → `<cellXfs>` entry 7 → its `numFmtId` → either a
//! builtin code (§18.8.30) or the workbook's own `<numFmt>`.
//!
//! **Two containers in this part hold identically-named children, and reading
//! the wrong one fails silently rather than loudly**, which is why every
//! collector below is gated on its enclosing element:
//!
//! * `<cellStyleXfs>` also holds `<xf>` elements. Only `<cellXfs>` is indexed
//!   by a cell's `s=`. Reading the other array yields plausible-looking
//!   formats attached to the wrong cells.
//! * `<dxfs>` (differential formats, used by conditional formatting) also
//!   holds `<font>` elements. Appending those to the font list shifts every
//!   `fontId` above them.

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::docx::error::Result;
use crate::xlsx::xml::{attr, attr_bool, attr_parse, local_name};

/// The format code applied when a cell names no style at all.
pub const GENERAL: &str = "General";

/// Custom format ids start here (§18.8.30); anything below with no builtin
/// entry is reserved and Excel does not write it.
pub const FIRST_CUSTOM_FORMAT_ID: u32 = 164;

/// ECMA-376 §18.8.30 builtin number formats.
///
/// Ids 23–36 and 41–44 are absent on purpose: the spec reserves them or makes
/// them locale-dependent, Excel does not write them, and a reader that invents
/// codes for them is guessing at the user's locale.
const BUILTIN_FORMATS: &[(u32, &str)] = &[
    (0, "General"),
    (1, "0"),
    (2, "0.00"),
    (3, "#,##0"),
    (4, "#,##0.00"),
    (9, "0%"),
    (10, "0.00%"),
    (11, "0.00E+00"),
    (12, "# ?/?"),
    (13, "# ??/??"),
    (14, "mm-dd-yy"),
    (15, "d-mmm-yy"),
    (16, "d-mmm"),
    (17, "mmm-yy"),
    (18, "h:mm AM/PM"),
    (19, "h:mm:ss AM/PM"),
    (20, "h:mm"),
    (21, "h:mm:ss"),
    (22, "m/d/yy h:mm"),
    (37, "#,##0 ;(#,##0)"),
    (38, "#,##0 ;[Red](#,##0)"),
    (39, "#,##0.00;(#,##0.00)"),
    (40, "#,##0.00;[Red](#,##0.00)"),
    (45, "mm:ss"),
    (46, "[h]:mm:ss"),
    (47, "mmss.0"),
    (48, "##0.0E+0"),
    (49, "@"),
];

/// Look up a builtin format code by id.
pub fn builtin_format(id: u32) -> Option<&'static str> {
    BUILTIN_FORMATS
        .iter()
        .find_map(|&(k, code)| (k == id).then_some(code))
}

/// Character formatting from `<fonts>`. Mirrors
/// [`RunProps`](crate::xlsx::text::RunProps), which is the run-level version of
/// the same properties — a cell takes this one, and a rich run overrides it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Font {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub size: Option<f32>,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HorizontalAlign {
    /// §18.18.40: numbers right, text left. The default, and the reason a
    /// spreadsheet reads as a table without any explicit alignment at all.
    #[default]
    General,
    Left,
    Center,
    Right,
    Fill,
    Justify,
    CenterContinuous,
    Distributed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Alignment {
    pub horizontal: HorizontalAlign,
    pub wrap_text: bool,
    /// Indent steps, each 3 spaces' worth in Excel's rendering. Load-bearing
    /// for structure: indented labels are how spreadsheets express hierarchy.
    pub indent: u32,
    /// Degrees counter-clockwise, or 255 for stacked/vertical text (§18.8.1).
    pub text_rotation: Option<u32>,
}

/// One `<cellXfs>` entry — the record a cell's `s=` points at.
#[derive(Clone, Debug, Default)]
pub struct CellXf {
    pub num_fmt_id: u32,
    pub font_id: u32,
    pub alignment: Alignment,
    /// `quotePrefix="1"`: the user typed a leading apostrophe to force text.
    /// The apostrophe is not in the cell value, so it must not be emitted, but
    /// it does mean the value is text no matter what it looks like.
    pub quote_prefix: bool,
}

/// The parsed `xl/styles.xml`.
#[derive(Clone, Debug, Default)]
pub struct Styles {
    /// Workbook-defined codes from `<numFmts>`, keyed by id.
    ///
    /// A custom id may legally redefine a builtin one, and may carry the code
    /// `General`; both are in the corpus, so custom entries are consulted
    /// first and never assumed to be "not General".
    custom_formats: HashMap<u32, String>,
    cell_xfs: Vec<CellXf>,
    fonts: Vec<Font>,
}

impl Styles {
    /// Parse `xl/styles.xml`.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut out = Styles::default();
        let mut reader = Reader::from_reader(data);
        let mut buf = Vec::new();
        // Which array we are inside. `<xf>` and `<font>` each appear under
        // more than one parent; see the module docs.
        let mut in_cell_xfs = false;
        let mut in_fonts = false;
        let mut current_xf: Option<CellXf> = None;
        let mut current_font: Option<Font> = None;

        loop {
            let event = reader
                .read_event_into(&mut buf)
                .map_err(quick_xml::DeError::from)?;
            let (start, empty) = match event {
                Event::Eof => break,
                Event::Start(ref e) => (Some(e), false),
                Event::Empty(ref e) => (Some(e), true),
                Event::End(ref e) => {
                    match local_name(e.name().as_ref()) {
                        b"cellXfs" => in_cell_xfs = false,
                        b"fonts" => in_fonts = false,
                        b"xf" => {
                            if let Some(xf) = current_xf.take() {
                                out.cell_xfs.push(xf);
                            }
                        }
                        b"font" => {
                            if let Some(font) = current_font.take() {
                                out.fonts.push(font);
                            }
                        }
                        _ => {}
                    }
                    buf.clear();
                    continue;
                }
                _ => {
                    buf.clear();
                    continue;
                }
            };
            let Some(e) = start else { unreachable!() };

            match local_name(e.name().as_ref()) {
                b"cellXfs" => in_cell_xfs = !empty,
                b"fonts" => in_fonts = !empty,
                b"numFmt" => {
                    // `<numFmt>` appears both in the workbook-level `<numFmts>`
                    // and inside `<dxf>`. Collecting both is harmless — they
                    // share one id space and a dxf restates rather than
                    // redefines — so this one is not container-gated.
                    if let (Some(id), Some(code)) =
                        (attr_parse(e, b"numFmtId"), attr(e, b"formatCode"))
                    {
                        out.custom_formats.insert(id, code);
                    }
                }
                b"xf" if in_cell_xfs => {
                    let xf = CellXf {
                        num_fmt_id: attr_parse(e, b"numFmtId").unwrap_or(0),
                        font_id: attr_parse(e, b"fontId").unwrap_or(0),
                        alignment: Alignment::default(),
                        quote_prefix: attr_bool(e, b"quotePrefix", false),
                    };
                    if empty {
                        out.cell_xfs.push(xf);
                    } else {
                        // A non-empty `<xf>` still has an `<alignment>` child
                        // to read before it can be pushed.
                        current_xf = Some(xf);
                    }
                }
                b"alignment" => {
                    if let Some(xf) = current_xf.as_mut() {
                        xf.alignment = Alignment {
                            horizontal: match attr(e, b"horizontal").as_deref() {
                                Some("left") => HorizontalAlign::Left,
                                Some("center") => HorizontalAlign::Center,
                                Some("right") => HorizontalAlign::Right,
                                Some("fill") => HorizontalAlign::Fill,
                                Some("justify") => HorizontalAlign::Justify,
                                Some("centerContinuous") => HorizontalAlign::CenterContinuous,
                                Some("distributed") => HorizontalAlign::Distributed,
                                _ => HorizontalAlign::General,
                            },
                            wrap_text: attr_bool(e, b"wrapText", false),
                            indent: attr_parse(e, b"indent").unwrap_or(0),
                            text_rotation: attr_parse::<u32>(e, b"textRotation")
                                .filter(|&r| r != 0),
                        };
                    }
                }
                b"font" if in_fonts => {
                    let font = Font::default();
                    if empty {
                        out.fonts.push(font);
                    } else {
                        current_font = Some(font);
                    }
                }
                name if current_font.is_some() => {
                    let font = current_font.as_mut().expect("checked by the guard");
                    // Same CT_BooleanProperty rule as run properties: an
                    // explicit `val="0"` turns the property off.
                    let on = || !matches!(attr(e, b"val").as_deref(), Some("0" | "false"));
                    match name {
                        b"b" => font.bold = on(),
                        b"i" => font.italic = on(),
                        b"strike" => font.strike = on(),
                        b"u" => {
                            font.underline = !matches!(attr(e, b"val").as_deref(), Some("none"))
                        }
                        b"sz" => font.size = attr_parse(e, b"val"),
                        b"name" => font.name = attr(e, b"val"),
                        _ => {}
                    }
                }
                _ => {}
            }
            buf.clear();
        }
        Ok(out)
    }

    /// The number-format code for a cell's `s=` index.
    ///
    /// Absent `s=`, an out-of-range index, and an unmapped `numFmtId` all
    /// resolve to `General` — the same answer Excel gives, and the reason most
    /// cells in most workbooks need no lookup at all.
    ///
    /// `applyNumberFormat="0"` is deliberately ignored. In the spec it means
    /// "take the format from the linked `<cellStyleXfs>` entry instead", but
    /// every producer in the corpus writes the effective `numFmtId` on the
    /// `<cellXfs>` entry regardless, and honouring the flag would replace a
    /// correct answer with an inherited one.
    pub fn format_code(&self, style_index: Option<u32>) -> &str {
        let Some(xf) = self.cell_xf(style_index) else {
            return GENERAL;
        };
        self.format_code_for_id(xf.num_fmt_id)
    }

    /// The code registered for a `numFmtId`, custom entries winning over
    /// builtins.
    pub fn format_code_for_id(&self, id: u32) -> &str {
        if let Some(code) = self.custom_formats.get(&id) {
            return code;
        }
        builtin_format(id).unwrap_or(GENERAL)
    }

    pub fn cell_xf(&self, style_index: Option<u32>) -> Option<&CellXf> {
        self.cell_xfs.get(style_index.unwrap_or(0) as usize)
    }

    /// The font for a cell's `s=` index, or the default font when anything in
    /// the chain is missing.
    pub fn font(&self, style_index: Option<u32>) -> Font {
        self.cell_xf(style_index)
            .and_then(|xf| self.fonts.get(xf.font_id as usize))
            .cloned()
            .unwrap_or_default()
    }

    pub fn alignment(&self, style_index: Option<u32>) -> Alignment {
        self.cell_xf(style_index)
            .map(|xf| xf.alignment.clone())
            .unwrap_or_default()
    }

    /// Every distinct format code the workbook can reach, for censuses.
    pub fn custom_formats(&self) -> &HashMap<u32, String> {
        &self.custom_formats
    }

    pub fn cell_xf_count(&self) -> usize {
        self.cell_xfs.len()
    }

    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: &str = r#"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="2">
    <numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/>
    <numFmt numFmtId="165" formatCode="General"/>
  </numFmts>
  <fonts count="3">
    <font><sz val="11"/><name val="Calibri"/></font>
    <font><b/><sz val="14"/><name val="Arial"/></font>
    <font><b val="0"/><i/></font>
  </fonts>
  <cellStyleXfs count="1">
    <xf numFmtId="9" fontId="1"/>
  </cellStyleXfs>
  <cellXfs count="4">
    <xf numFmtId="0" fontId="0"/>
    <xf numFmtId="164" fontId="1" applyNumberFormat="1"/>
    <xf numFmtId="3" fontId="2" quotePrefix="1"/>
    <xf numFmtId="0" fontId="0"><alignment horizontal="center" wrapText="1" indent="2"/></xf>
  </cellXfs>
  <dxfs count="1"><dxf><font><b/></font></dxf></dxfs>
</styleSheet>"#;

    fn styles() -> Styles {
        Styles::parse(STYLES.as_bytes()).unwrap()
    }

    /// The headline trap. `<cellStyleXfs>` holds an `<xf numFmtId="9">` (a
    /// percent format) directly above `<cellXfs>`. If it leaked in, every
    /// `s=` index would be shifted by one and cell styles would silently
    /// resolve to their neighbours'.
    #[test]
    fn cell_style_xfs_do_not_leak_into_cell_xfs() {
        let s = styles();
        assert_eq!(s.cell_xf_count(), 4);
        assert_eq!(s.format_code(Some(0)), "General");
        assert_eq!(s.format_code(Some(1)), "\"$\"#,##0.00");
        assert_eq!(s.format_code(Some(2)), "#,##0");
        // The percent code from cellStyleXfs must appear nowhere.
        assert!((0..4).all(|i| s.format_code(Some(i)) != "0%"));
    }

    /// `<dxfs>` holds `<font>` elements too; counting them shifts every
    /// `fontId` above the insertion point.
    #[test]
    fn dxf_fonts_do_not_shift_the_font_ids() {
        let s = styles();
        assert_eq!(s.font_count(), 3);
        assert_eq!(s.font(Some(1)).name.as_deref(), Some("Arial"));
        assert!(s.font(Some(1)).bold);
    }

    #[test]
    fn absent_and_out_of_range_style_indices_are_general() {
        let s = styles();
        assert_eq!(s.format_code(None), GENERAL);
        assert_eq!(s.format_code(Some(999)), GENERAL);
        assert_eq!(s.font(Some(999)), Font::default());
    }

    /// A custom id may carry the code `General` — a reader that assumed
    /// "id >= 164 means non-General" would format these cells wrongly.
    #[test]
    fn a_custom_format_may_be_general() {
        assert_eq!(styles().format_code_for_id(165), "General");
    }

    #[test]
    fn format_code_attributes_are_unescaped() {
        // `"$"#,##0.00` arrives as `&quot;$&quot;#,##0.00`; leaving the
        // entities in would hand the formatter a code it cannot parse.
        assert!(styles().format_code_for_id(164).starts_with('"'));
    }

    #[test]
    fn font_boolean_off_switch_is_honoured() {
        let f = styles().font(Some(2));
        assert!(!f.bold, "b val=0 must turn bold off");
        assert!(f.italic);
    }

    #[test]
    fn alignment_is_read_from_the_xf_child() {
        let a = styles().alignment(Some(3));
        assert_eq!(a.horizontal, HorizontalAlign::Center);
        assert!(a.wrap_text);
        assert_eq!(a.indent, 2);
        // An xf with no <alignment> child gets the default, not the previous
        // xf's — the classic streaming-parser carry-over bug.
        assert_eq!(styles().alignment(Some(0)), Alignment::default());
    }

    #[test]
    fn quote_prefix_is_captured() {
        assert!(styles().cell_xf(Some(2)).unwrap().quote_prefix);
        assert!(!styles().cell_xf(Some(0)).unwrap().quote_prefix);
    }

    #[test]
    fn a_workbook_with_no_styles_part_still_resolves() {
        let s = Styles::default();
        assert_eq!(s.format_code(Some(7)), GENERAL);
        assert_eq!(s.font(Some(7)), Font::default());
    }

    #[test]
    fn builtin_table_omits_reserved_ids() {
        assert_eq!(builtin_format(14), Some("mm-dd-yy"));
        assert_eq!(builtin_format(49), Some("@"));
        // Reserved / locale-dependent: no code may be invented for these.
        assert_eq!(builtin_format(23), None);
        assert_eq!(builtin_format(42), None);
    }
}
