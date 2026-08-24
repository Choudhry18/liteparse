//! Native SpreadsheetML (`.xlsx`) reader.
//!
//! Shares the OOXML packaging and relationships layers with [`crate::docx`] and
//! [`crate::pptx`] — see `ATTRIBUTION.md` for what is vendored and what is ours.
//! Unlike those two, nothing here is vendored: SpreadsheetML has no DOCX
//! counterpart in dxpdf, so the whole reader is new code against ECMA-376 §18.
//!
//! # What this replaces
//!
//! Today an `.xlsx` goes through LibreOffice → PDF → grid projection, and the
//! measured cost of that is not subtle. Across 218 real workbooks (63 research
//! + 150 finance), that path emits **zero markdown table rows** and clips every
//! cell at its rendered column width — `"Mosaic tree and shrub (>50%) …"`
//! becomes `"Mosaic tr"`, destroyed before projection ever sees it, with a
//! cell-text recall of 0.77 macro / 0.66–0.73 micro as the ceiling.
//!
//! A workbook states its grid explicitly. There is nothing to reverse-engineer.
//!
//! # Shape
//!
//! ```no_run
//! let bytes = std::fs::read("report.xlsx")?;
//! let wb = liteparse_ooxml::xlsx::read(&bytes)?;
//! for sheet in &wb.sheets {
//!     for row in &sheet.rows {
//!         for cell in &row.cells {
//!             // The raw value and its format code, kept apart …
//!             let text = wb.cell_text(cell);
//!             // … and joined on demand: `0.155` + `0.0%` → `15.5%`.
//!             let shown = wb.display_text(cell);
//!         }
//!     }
//! }
//! # Ok::<(), liteparse_ooxml::Error>(())
//! ```
//!
//! # What this layer does *not* do
//!
//! * **Geometry.** Column widths stay in Excel's character unit and row
//!   heights in points; converting to a page needs the Normal font's digit
//!   width. That is the geometry pass.
//! * **Emission.** No markdown, no `Block`s. That is `liteparse`'s job, and
//!   keeping it out of here is what lets this module be tested against a
//!   corpus with no renderer in the loop.

pub mod numfmt;
pub mod package;
pub mod refs;
pub mod sheet;
pub mod styles;
pub mod text;
mod xml;

pub use package::{SheetEntry, WorkbookPackage, walk};
pub use refs::{CellRef, RangeRef, column_label, parse_cell, parse_column, parse_range};
pub use sheet::{Cell, CellValue, ColInfo, Hyperlink, Row, Sheet, SheetStats};
pub use styles::{Alignment, CellXf, Font, HorizontalAlign, Styles};
pub use text::{RichText, RunProps, TextRun, VertAlign, parse_shared_strings};

use crate::docx::error::Result;

/// A workbook read end to end.
pub struct Workbook {
    /// Sheets in tab order, including hidden ones (see [`Sheet::visible`]) and
    /// excluding chartsheets, which carry no cell grid.
    pub sheets: Vec<Sheet>,
    /// The shared-string table, addressed by index from
    /// [`CellValue::SharedString`]. Not folded into the cells: it exists so a
    /// string used 40,000 times is stored once, and resolving eagerly would
    /// undo that.
    pub shared_strings: Vec<RichText>,
    pub styles: Styles,
    /// Serial dates count from 1904-01-01 rather than 1899-12-30.
    pub date1904: bool,
    /// Sheets named in `<sheets>` that hold no cell grid — chartsheets and
    /// dialogsheets. Reported so a caller can tell an empty workbook from one
    /// whose content this reader does not model.
    pub non_worksheet_sheets: Vec<String>,
}

/// Read an `.xlsx` from raw bytes.
///
/// Fails only when the container is unreadable or the workbook part is absent.
/// Everything below that degrades: an unparseable sheet is skipped with a
/// warning, a missing styles part means every cell is `General`, and a missing
/// shared-string table means string cells resolve to nothing. A workbook is
/// almost never wholly broken, and refusing the whole file because one sheet is
/// malformed is the fail-closed behaviour this vendor has already had to
/// retire repeatedly.
pub fn read(data: &[u8]) -> Result<Workbook> {
    let pkg = walk(data)?;

    let shared_strings = match pkg.shared_strings_xml.as_deref() {
        Some(xml) => parse_shared_strings(xml).unwrap_or_else(|e| {
            // Losing the table costs every `t="s"` cell its text, which is
            // most of the workbook's words — but the numbers, the structure
            // and the inline strings all survive.
            log::warn!("unreadable shared-string table: {e}");
            Vec::new()
        }),
        None => Vec::new(),
    };
    let styles = match pkg.styles_xml.as_deref() {
        Some(xml) => Styles::parse(xml).unwrap_or_else(|e| {
            log::warn!("unreadable styles part: {e}");
            Styles::default()
        }),
        None => Styles::default(),
    };

    let mut sheets = Vec::new();
    let mut non_worksheet_sheets = Vec::new();
    for entry in &pkg.sheets {
        if !entry.is_worksheet {
            non_worksheet_sheets.push(entry.name.clone());
            continue;
        }
        let Some(xml) = pkg.package.get_part(&entry.path) else {
            continue;
        };
        match sheet::parse(&entry.name, entry.visible, xml) {
            Ok(s) => sheets.push(s),
            Err(e) => log::warn!("sheet {:?} failed to parse: {e}", entry.name),
        }
    }

    Ok(Workbook {
        sheets,
        shared_strings,
        styles,
        date1904: pkg.date1904,
        non_worksheet_sheets,
    })
}

impl Workbook {
    /// The text of a cell, resolving a shared-string index against the table.
    ///
    /// Returns `None` for a cell that holds no text — a number, a boolean, an
    /// error — which is the caller's cue that the value needs formatting
    /// rather than printing.
    pub fn cell_text<'a>(&'a self, cell: &'a Cell) -> Option<&'a RichText> {
        match &cell.value {
            CellValue::Text(t) => Some(t),
            CellValue::SharedString(i) => match self.shared_strings.get(*i as usize) {
                Some(t) => Some(t),
                None => {
                    // An index past the end of the table is corruption, and
                    // silently rendering an empty cell would hide it.
                    log::warn!(
                        "shared string {i} out of range ({} entries)",
                        self.shared_strings.len()
                    );
                    None
                }
            },
            _ => None,
        }
    }

    /// The number-format code governing a cell — the seam the number-format
    /// interpreter plugs into.
    pub fn format_code(&self, cell: &Cell) -> &str {
        self.styles.format_code(cell.style)
    }

    /// Total cells carrying a value, across every sheet.
    pub fn cell_count(&self) -> usize {
        self.sheets.iter().map(|s| s.cell_count()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal but real `.xlsx` container in memory. Written by hand
    /// rather than fixtured so the test states exactly which parts the reader
    /// is being asked to join.
    fn build(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, body) in parts {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    const ROOT_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

    const WB_RELS: &str = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>
  <Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
  <Relationship Id="rId4" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet" Target="chartsheets/sheet1.xml"/>
</Relationships>"#;

    const WORKBOOK: &str = r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="Data" sheetId="1" r:id="rId1"/>
    <sheet name="Chart" sheetId="2" r:id="rId4"/>
  </sheets>
</workbook>"#;

    const SST: &str = r#"<sst count="2" uniqueCount="2">
  <si><t>Total assets</t></si>
  <si><r><rPr><b/></rPr><t>Q3</t></r></si>
</sst>"#;

    const STYLES: &str = r#"<styleSheet>
  <numFmts><numFmt numFmtId="164" formatCode="0.0%"/></numFmts>
  <cellXfs count="2"><xf numFmtId="0" fontId="0"/><xf numFmtId="164" fontId="0"/></cellXfs>
</styleSheet>"#;

    const SHEET1: &str = r#"<worksheet><sheetData>
  <row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row>
  <row r="2"><c r="A2"><v>1234.5</v></c><c r="B2" s="1"><v>0.155</v></c></row>
</sheetData></worksheet>"#;

    fn workbook() -> Workbook {
        read(&build(&[
            ("[Content_Types].xml", "<Types/>"),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WB_RELS),
            ("xl/sharedStrings.xml", SST),
            ("xl/styles.xml", STYLES),
            ("xl/worksheets/sheet1.xml", SHEET1),
            ("xl/chartsheets/sheet1.xml", "<chartsheet/>"),
        ]))
        .unwrap()
    }

    #[test]
    fn a_workbook_joins_its_parts() {
        let wb = workbook();
        assert_eq!(wb.sheets.len(), 1);
        assert_eq!(wb.sheets[0].name, "Data");
        assert_eq!(wb.cell_count(), 4);
    }

    #[test]
    fn shared_strings_resolve_through_the_workbook() {
        let wb = workbook();
        let cell = &wb.sheets[0].rows[0].cells[0];
        assert_eq!(wb.cell_text(cell).unwrap().plain(), "Total assets");
        // Rich formatting survives the round trip through the table.
        let q3 = &wb.sheets[0].rows[0].cells[1];
        assert!(wb.cell_text(q3).unwrap().runs[0].props.bold);
    }

    #[test]
    fn numbers_carry_a_format_code_rather_than_a_rendered_string() {
        let wb = workbook();
        let plain = &wb.sheets[0].rows[1].cells[0];
        let pct = &wb.sheets[0].rows[1].cells[1];
        assert_eq!(plain.value, CellValue::Number(1234.5));
        assert_eq!(wb.format_code(plain), styles::GENERAL);
        // The seam: `0.155` plus `0.0%` is what the interpreter turns into
        // `15.5%`. Neither half is discarded here.
        assert_eq!(pct.value, CellValue::Number(0.155));
        assert_eq!(wb.format_code(pct), "0.0%");
        assert!(wb.cell_text(pct).is_none());
        // …and joining them is `display_text`, not the reader's business.
        assert_eq!(wb.display_text(pct).unwrap(), "15.5%");
        assert_eq!(wb.display_text(plain).unwrap(), "1234.5");
    }

    /// A shared string reaches the formatter through the table, so the text
    /// path cannot be exercised by [`numfmt::render`] alone.
    #[test]
    fn display_text_resolves_a_shared_string_before_formatting_it() {
        let wb = workbook();
        let cell = &wb.sheets[0].rows[0].cells[0];
        assert_eq!(wb.display_text(cell).unwrap(), "Total assets");
    }

    /// A chartsheet is declared in `<sheets>` exactly like a worksheet. Parsing
    /// it as one would add an empty sheet to the output.
    #[test]
    fn chartsheets_are_reported_not_parsed_as_worksheets() {
        let wb = workbook();
        assert_eq!(wb.non_worksheet_sheets, vec!["Chart"]);
    }

    /// The failure the packaging layer exists to prevent: the part name is
    /// conventional, so a workbook that puts its sheet somewhere else must
    /// still resolve through relationships.
    #[test]
    fn sheets_resolve_through_rels_not_by_convention() {
        let rels = WB_RELS.replace("worksheets/sheet1.xml", "sheets/data.xml");
        let wb = read(&build(&[
            ("[Content_Types].xml", "<Types/>"),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", &rels),
            ("xl/sheets/data.xml", SHEET1),
        ]))
        .unwrap();
        assert_eq!(wb.cell_count(), 4);
    }

    #[test]
    fn a_workbook_with_no_styles_or_strings_still_reads() {
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let wb = read(&build(&[
            ("[Content_Types].xml", "<Types/>"),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/worksheets/sheet1.xml", SHEET1),
        ]))
        .unwrap();
        assert_eq!(wb.sheets.len(), 1);
        // String cells have nothing to resolve against, but numbers survive.
        assert!(wb.cell_text(&wb.sheets[0].rows[0].cells[0]).is_none());
        assert_eq!(
            wb.sheets[0].rows[1].cells[0].value,
            CellValue::Number(1234.5)
        );
    }

    #[test]
    fn a_missing_workbook_part_is_the_one_fatal_error() {
        let data = build(&[
            ("[Content_Types].xml", "<Types/>"),
            ("_rels/.rels", ROOT_RELS),
        ]);
        assert!(read(&data).is_err());
    }

    #[test]
    fn a_non_zip_input_is_rejected() {
        assert!(read(b"<html>not a workbook</html>").is_err());
    }
}
