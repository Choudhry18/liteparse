//! A worksheet part (`xl/worksheets/sheetN.xml`, §18.3): the cell grid, the
//! row/column metrics the grid is drawn on, and the merges that break it.
//!
//! Streamed rather than deserialized. Sheets are the bulk of a workbook — 50.3M
//! cells across the 1,089-workbook finance corpus, single sheets past 32 MB —
//! and this is the one part where the reader's memory profile is decided.
//!
//! Two allocation decisions follow from that and are load-bearing:
//!
//! * **Shared strings are not resolved here.** A cell keeps its index into the
//!   shared-string table. The table exists precisely so a string used 40,000
//!   times is stored once; resolving eagerly would undo that. See
//!   [`crate::xlsx::Workbook::cell_text`].
//! * **Value-less cells are dropped.** `<c r="A1" s="7"/>` carries a style and
//!   no content — usually a bordered-but-blank region, and in some producers
//!   an entire pre-formatted sheet. They are counted in [`SheetStats`] rather
//!   than stored, because a workbook can hold millions of them and none of
//!   them carries text.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::docx::error::Result;
use crate::xlsx::refs::{CellRef, RangeRef, parse_cell, parse_range};
use crate::xlsx::text::{RichText, TextRun, read_rich_text};
use crate::xlsx::xml::{
    attr, attr_bool, attr_parse, decode_cdata, decode_general_ref, decode_text, local_name,
};

/// A cell's value, as the file states it.
#[derive(Clone, Debug, PartialEq)]
pub enum CellValue {
    /// `t="n"` or, far more often, no `t=` at all — the default and the single
    /// most common cell in any workbook. Dates live here too: Excel stores
    /// them as serial numbers and only the format code says otherwise.
    Number(f64),
    /// `t="s"`: an index into the workbook's shared-string table.
    SharedString(u32),
    /// `t="inlineStr"` (`<is>`) or `t="str"` (a formula's cached string
    /// result). Both carry their text in the cell.
    Text(RichText),
    /// `t="b"`, written as `0` / `1`.
    Bool(bool),
    /// `t="e"`: `#DIV/0!`, `#N/A`, `#REF!` and friends, kept verbatim because
    /// the error text is what a reader of the sheet sees.
    Error(String),
    /// `t="d"`: an ISO 8601 date, written literally rather than as a serial.
    /// Only ECMA-376 Strict and a few non-Excel producers emit this.
    Date(String),
}

/// One cell that carries a value.
#[derive(Clone, Debug)]
pub struct Cell {
    pub at: CellRef,
    /// The `s=` index into `<cellXfs>`; `None` means style 0 / General.
    pub style: Option<u32>,
    pub value: CellValue,
    /// The cell holds an `<f>`. The formula text itself is not kept — the
    /// cached `<v>` is what renders, and 49.3% of corpus workbooks have
    /// formulas, so storing their source would be pure weight.
    pub has_formula: bool,
}

/// A row that exists in the file, with its own metrics.
#[derive(Clone, Debug, Default)]
pub struct Row {
    /// Zero-based.
    pub index: u32,
    /// `ht=` in points, when the row states its own height.
    pub height: Option<f64>,
    pub hidden: bool,
    /// Cells in written order, which is ascending column order in every
    /// producer seen. Sparse: absent columns are absent.
    pub cells: Vec<Cell>,
}

/// A `<col>` span's metrics. One entry covers columns `min..=max`, both
/// zero-based here (the file writes them one-based).
#[derive(Clone, Debug)]
pub struct ColInfo {
    pub min: u32,
    pub max: u32,
    /// Excel's column width unit: the count of `0` glyphs of the Normal font
    /// that fit in the column. Converting it to points needs the font's digit
    /// width, so it is kept raw here and converted by the geometry pass.
    pub width: Option<f64>,
    pub hidden: bool,
    pub custom_width: bool,
}

#[derive(Clone, Debug)]
pub struct Hyperlink {
    pub at: RangeRef,
    /// The `r:id` to resolve against the sheet's own relationships. Absent for
    /// an in-workbook link, which carries only a `location`.
    pub rel_id: Option<String>,
    /// `location=`: a defined name or `Sheet2!A1` target within the workbook.
    pub location: Option<String>,
    pub tooltip: Option<String>,
}

/// Counts kept for the census and for the emitter's routing decisions —
/// things that were seen and deliberately not stored.
#[derive(Clone, Copy, Debug, Default)]
pub struct SheetStats {
    /// Cells with a style and no value. See the module docs for why they are
    /// dropped.
    pub empty_styled_cells: u32,
    /// Cells whose `r=` was absent or unparseable and whose position was
    /// inferred from document order. Non-zero means a producer that is not
    /// Excel; worth knowing before trusting the geometry.
    pub inferred_positions: u32,
    /// `<c t="s">` whose `<v>` is not a usable index into the shared-string
    /// table — unparseable here, out of range when resolved. Always a bug in
    /// the producer or in this reader, never normal.
    pub dangling_shared_strings: u32,
}

/// One parsed worksheet.
#[derive(Clone, Debug, Default)]
pub struct Sheet {
    /// The tab name from `<sheet name=…>` in the workbook part, not from the
    /// sheet itself — a worksheet part does not know what it is called.
    pub name: String,
    pub visible: bool,
    /// `<dimension ref=…>`: the producer's claim about the used range. A
    /// claim, not a guarantee — some producers write `A1` regardless — so it
    /// is exposed for cross-checking rather than used to size anything.
    pub dimension: Option<RangeRef>,
    pub default_col_width: Option<f64>,
    pub default_row_height: Option<f64>,
    pub cols: Vec<ColInfo>,
    pub rows: Vec<Row>,
    /// Present in 80.7% of real workbooks, which is why they are read by the
    /// first version of this reader rather than deferred.
    pub merges: Vec<RangeRef>,
    pub hyperlinks: Vec<Hyperlink>,
    /// `<pane ySplit=… state="frozen">`: rows frozen at the top. A direct
    /// statement that those rows are a header, which is otherwise the hardest
    /// thing to infer about a spreadsheet.
    pub frozen_rows: u32,
    pub frozen_cols: u32,
    /// `<autoFilter ref=…>`, the other explicit header signal.
    pub auto_filter: Option<RangeRef>,
    pub stats: SheetStats,
}

impl Sheet {
    pub fn cell_count(&self) -> usize {
        self.rows.iter().map(|r| r.cells.len()).sum()
    }

    /// The merge whose top-left corner is `at`, if any. The anchor is the only
    /// cell of a merged region that carries a value; Excel writes the rest as
    /// value-less cells, which this reader has already dropped.
    pub fn merge_anchored_at(&self, at: CellRef) -> Option<&RangeRef> {
        self.merges.iter().find(|m| m.start == at)
    }

    /// The merge covering `at`, anchor or not.
    pub fn merge_covering(&self, at: CellRef) -> Option<&RangeRef> {
        self.merges.iter().find(|m| m.contains(at))
    }

    /// The declared width of a column, walking the `<col>` spans.
    pub fn col_width(&self, col: u32) -> Option<f64> {
        self.cols
            .iter()
            .find(|c| col >= c.min && col <= c.max)
            .and_then(|c| c.width)
            .or(self.default_col_width)
    }
}

/// Parse one worksheet part.
pub fn parse(name: &str, visible: bool, data: &[u8]) -> Result<Sheet> {
    let mut sheet = Sheet {
        name: name.to_string(),
        visible,
        ..Sheet::default()
    };
    let mut reader = Reader::from_reader(data);
    let mut buf = Vec::new();
    // A separate buffer for nested reads: `buf` is borrowed by the event
    // currently being matched, so `<is>` cannot recurse into it.
    let mut nested_buf = Vec::new();

    let mut row: Option<Row> = None;
    let mut cell: Option<PendingCell> = None;
    // The running position used when a producer omits `r=`. §18.3.1.4 makes
    // the attribute optional, in which case position is document order.
    let mut next_row: u32 = 0;
    let mut next_col: u32 = 0;
    let mut in_value = false;
    let mut value = String::new();
    // `<pane>` also appears under `<customSheetView>`, where it describes a
    // saved view rather than the sheet as opened. Only the first `<sheetView>`
    // one is the frozen-header signal.
    let mut in_sheet_view = false;

    loop {
        let event = reader
            .read_event_into(&mut buf)
            .map_err(quick_xml::DeError::from)?;
        match event {
            Event::Eof => break,
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                match local_name(e.name().as_ref()) {
                    b"dimension" => {
                        sheet.dimension = attr(e, b"ref").as_deref().and_then(parse_range);
                    }
                    b"sheetFormatPr" => {
                        sheet.default_col_width = attr_parse(e, b"defaultColWidth");
                        sheet.default_row_height = attr_parse(e, b"defaultRowHeight");
                    }
                    b"sheetView" => in_sheet_view = !empty,
                    b"pane" if in_sheet_view => {
                        // `state="split"` is a draggable divider, not a frozen
                        // header; only frozen panes state that rows above the
                        // split are repeated context.
                        if matches!(attr(e, b"state").as_deref(), Some("frozen" | "frozenSplit")) {
                            sheet.frozen_rows = attr_parse(e, b"ySplit").unwrap_or(0);
                            sheet.frozen_cols = attr_parse(e, b"xSplit").unwrap_or(0);
                        }
                    }
                    b"col" => {
                        // One-based and inclusive in the file.
                        let min: u32 = attr_parse(e, b"min").unwrap_or(1);
                        let max: u32 = attr_parse(e, b"max").unwrap_or(min);
                        sheet.cols.push(ColInfo {
                            min: min.saturating_sub(1),
                            max: max.saturating_sub(1),
                            width: attr_parse(e, b"width"),
                            hidden: attr_bool(e, b"hidden", false),
                            custom_width: attr_bool(e, b"customWidth", false),
                        });
                    }
                    b"row" => {
                        let index = match attr_parse::<u32>(e, b"r") {
                            Some(r) if r >= 1 => r - 1,
                            _ => next_row,
                        };
                        next_row = index + 1;
                        next_col = 0;
                        let started = Row {
                            index,
                            height: attr_parse(e, b"ht"),
                            hidden: attr_bool(e, b"hidden", false),
                            cells: Vec::new(),
                        };
                        // An empty `<row/>` still carries height and hidden
                        // state, which the geometry pass needs.
                        if empty {
                            sheet.rows.push(started);
                        } else {
                            row = Some(started);
                        }
                    }
                    b"c" => {
                        let at = match attr(e, b"r").as_deref().and_then(parse_cell) {
                            Some(at) => at,
                            None => {
                                sheet.stats.inferred_positions += 1;
                                CellRef {
                                    row: row.as_ref().map_or(next_row, |r| r.index),
                                    col: next_col,
                                }
                            }
                        };
                        next_col = at.col + 1;
                        let pending = PendingCell {
                            at,
                            style: attr_parse(e, b"s"),
                            kind: attr(e, b"t").unwrap_or_else(|| "n".to_string()),
                            has_formula: false,
                            text: None,
                        };
                        if empty {
                            // No children means no value: styled-but-blank.
                            sheet.stats.empty_styled_cells += 1;
                        } else {
                            cell = Some(pending);
                        }
                    }
                    b"v" if cell.is_some() => {
                        in_value = !empty;
                        value.clear();
                    }
                    b"f" if cell.is_some() => {
                        if let Some(c) = cell.as_mut() {
                            c.has_formula = true;
                        }
                    }
                    b"is" if cell.is_some() && !empty => {
                        let text = read_rich_text(&mut reader, &mut nested_buf, b"is")?;
                        if let Some(c) = cell.as_mut() {
                            c.text = Some(text);
                        }
                    }
                    b"mergeCell" => {
                        if let Some(r) = attr(e, b"ref").as_deref().and_then(parse_range) {
                            sheet.merges.push(r);
                        }
                    }
                    b"hyperlink" => {
                        if let Some(at) = attr(e, b"ref").as_deref().and_then(parse_range) {
                            sheet.hyperlinks.push(Hyperlink {
                                at,
                                rel_id: attr(e, b"r:id"),
                                location: attr(e, b"location"),
                                tooltip: attr(e, b"tooltip"),
                            });
                        }
                    }
                    b"autoFilter" => {
                        sheet.auto_filter = attr(e, b"ref").as_deref().and_then(parse_range);
                    }
                    _ => {}
                }
            }
            Event::Text(ref t) if in_value => value.push_str(&decode_text(t)?),
            Event::CData(ref c) if in_value => value.push_str(&decode_cdata(c)?),
            // A `t="str"` cell's cached text arrives entity-split, same as a
            // shared string; see `xml::decode_general_ref`.
            Event::GeneralRef(ref r) if in_value => value.push_str(&decode_general_ref(r)?),
            Event::End(ref e) => match local_name(e.name().as_ref()) {
                b"sheetView" => in_sheet_view = false,
                b"v" => in_value = false,
                b"c" => {
                    if let Some(pending) = cell.take() {
                        finish_cell(pending, &value, &mut row, &mut sheet);
                        value.clear();
                    }
                }
                b"row" => {
                    if let Some(r) = row.take() {
                        sheet.rows.push(r);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }

    // A `</row>` or `</c>` lost to a truncated file still yields what was read.
    if let Some(pending) = cell.take() {
        finish_cell(pending, &value, &mut row, &mut sheet);
    }
    if let Some(r) = row.take() {
        sheet.rows.push(r);
    }
    Ok(sheet)
}

struct PendingCell {
    at: CellRef,
    style: Option<u32>,
    kind: String,
    has_formula: bool,
    text: Option<RichText>,
}

fn finish_cell(pending: PendingCell, raw: &str, row: &mut Option<Row>, sheet: &mut Sheet) {
    let value = match pending.kind.as_str() {
        "inlineStr" => pending.text.map(CellValue::Text),
        // An empty body is a styled-but-blank cell that the producer happened
        // to write as `<c …></c>` rather than `<c …/>`. Not corruption, and
        // counting it as a dangling index would report 480 fake anomalies on
        // one corpus workbook — which is exactly what it did before this
        // distinction existed.
        "s" if raw.trim().is_empty() => None,
        "s" => match raw.trim().parse::<u32>() {
            Ok(i) => Some(CellValue::SharedString(i)),
            Err(_) => {
                sheet.stats.dangling_shared_strings += 1;
                None
            }
        },
        // A formula's cached string result. Plain text with no runs — the
        // `<is>` grammar does not apply here.
        "str" => (!raw.is_empty()).then(|| {
            CellValue::Text(RichText {
                runs: vec![TextRun {
                    text: raw.to_string(),
                    props: Default::default(),
                }],
            })
        }),
        "b" => Some(CellValue::Bool(raw.trim() != "0")),
        "e" => (!raw.is_empty()).then(|| CellValue::Error(raw.trim().to_string())),
        "d" => (!raw.is_empty()).then(|| CellValue::Date(raw.trim().to_string())),
        // "n" and anything unrecognised. A number that will not parse is not
        // a number: dropping it is right, because emitting `NaN` would put a
        // value in a cell the spreadsheet shows as blank.
        _ => raw.trim().parse::<f64>().ok().map(CellValue::Number),
    };

    let Some(value) = value else {
        sheet.stats.empty_styled_cells += 1;
        return;
    };
    let cell = Cell {
        at: pending.at,
        style: pending.style,
        value,
        has_formula: pending.has_formula,
    };
    // A `<c>` outside any `<row>` is malformed but recoverable: give it a row
    // of its own rather than losing the value.
    match row.as_mut() {
        Some(r) => r.cells.push(cell),
        None => sheet.rows.push(Row {
            index: cell.at.row,
            cells: vec![cell],
            ..Row::default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHEET: &str = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
           xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <dimension ref="A1:C4"/>
  <sheetViews><sheetView><pane xSplit="1" ySplit="2" topLeftCell="B3" state="frozen"/></sheetView></sheetViews>
  <sheetFormatPr defaultRowHeight="15" defaultColWidth="8.43"/>
  <cols>
    <col min="1" max="1" width="24.5" customWidth="1"/>
    <col min="2" max="3" width="12" hidden="1"/>
  </cols>
  <sheetData>
    <row r="1" ht="30" customHeight="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" s="2"/>
    </row>
    <row r="2">
      <c r="A2"><v>1234.5</v></c>
      <c r="B2" t="b"><v>1</v></c>
      <c r="C2" t="e"><v>#DIV/0!</v></c>
    </row>
    <row r="4" hidden="1">
      <c r="A4" t="inlineStr"><is><r><rPr><b/></rPr><t>inline</t></r></is></c>
      <c r="B4" t="str"><f>CONCAT(A1)</f><v>computed</v></c>
    </row>
  </sheetData>
  <autoFilter ref="A1:C4"/>
  <mergeCells count="1"><mergeCell ref="A1:C1"/></mergeCells>
  <hyperlinks><hyperlink ref="A2" r:id="rId1" tooltip="go"/></hyperlinks>
</worksheet>"#;

    fn sheet() -> Sheet {
        parse("Sheet1", true, SHEET.as_bytes()).unwrap()
    }

    #[test]
    fn rows_keep_their_declared_index_not_their_position() {
        let s = sheet();
        // Row 3 is absent from the file; the fourth row must stay row 4
        // (index 3), or every metric below it shifts up one.
        assert_eq!(
            s.rows.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![0, 1, 3]
        );
    }

    #[test]
    fn cell_values_are_typed_by_their_t_attribute() {
        let s = sheet();
        assert_eq!(s.rows[0].cells[0].value, CellValue::SharedString(0));
        assert_eq!(s.rows[1].cells[0].value, CellValue::Number(1234.5));
        assert_eq!(s.rows[1].cells[1].value, CellValue::Bool(true));
        assert_eq!(
            s.rows[1].cells[2].value,
            CellValue::Error("#DIV/0!".to_string())
        );
    }

    #[test]
    fn a_cell_with_no_t_attribute_is_a_number() {
        // The single most common cell in any workbook carries no type marker.
        assert_eq!(sheet().rows[1].cells[0].value, CellValue::Number(1234.5));
    }

    #[test]
    fn inline_strings_keep_their_runs() {
        let s = sheet();
        let CellValue::Text(ref t) = s.rows[2].cells[0].value else {
            panic!("expected inline text");
        };
        assert_eq!(t.plain(), "inline");
        assert!(t.runs[0].props.bold);
    }

    /// `<is>` is consumed by a nested reader loop. If it did not resume
    /// cleanly, the rest of the row — here the `t="str"` cell after it —
    /// would be lost.
    #[test]
    fn parsing_resumes_after_an_inline_string() {
        let s = sheet();
        assert_eq!(s.rows[2].cells.len(), 2);
        let CellValue::Text(ref t) = s.rows[2].cells[1].value else {
            panic!("expected the formula's cached string");
        };
        assert_eq!(t.plain(), "computed");
        assert!(s.rows[2].cells[1].has_formula);
    }

    #[test]
    fn styled_but_valueless_cells_are_counted_not_stored() {
        let s = sheet();
        assert_eq!(s.rows[0].cells.len(), 1, "B1 has a style and no value");
        assert_eq!(s.stats.empty_styled_cells, 1);
    }

    #[test]
    fn merges_and_filters_and_frozen_panes_are_read() {
        let s = sheet();
        assert_eq!(s.merges.len(), 1);
        assert_eq!(s.merges[0], parse_range("A1:C1").unwrap());
        assert_eq!(s.auto_filter, Some(parse_range("A1:C4").unwrap()));
        assert_eq!(s.frozen_rows, 2);
        assert_eq!(s.frozen_cols, 1);
    }

    /// `state="split"` is a draggable divider, not a header declaration.
    #[test]
    fn split_panes_are_not_frozen_rows() {
        let xml = r#"<worksheet><sheetViews><sheetView>
            <pane xSplit="1" ySplit="2" state="split"/>
        </sheetView></sheetViews><sheetData/></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.frozen_rows, 0);
    }

    #[test]
    fn col_spans_are_zero_based_and_inclusive() {
        let s = sheet();
        assert_eq!(s.col_width(0), Some(24.5));
        assert_eq!(s.col_width(1), Some(12.0));
        assert_eq!(s.col_width(2), Some(12.0), "max is inclusive");
        assert_eq!(s.col_width(3), Some(8.43), "falls back to the default");
        assert!(s.cols[1].hidden);
    }

    #[test]
    fn row_metrics_survive() {
        let s = sheet();
        assert_eq!(s.rows[0].height, Some(30.0));
        assert!(!s.rows[0].hidden);
        assert!(s.rows[2].hidden);
    }

    #[test]
    fn hyperlinks_carry_their_rel_id() {
        let s = sheet();
        assert_eq!(s.hyperlinks[0].rel_id.as_deref(), Some("rId1"));
        assert_eq!(s.hyperlinks[0].tooltip.as_deref(), Some("go"));
    }

    /// §18.3.1.4 makes `r=` optional on both `<row>` and `<c>`; a handful of
    /// non-Excel producers omit it and position is then document order.
    #[test]
    fn positions_are_inferred_when_r_is_absent() {
        let xml = r#"<worksheet><sheetData>
            <row><c><v>1</v></c><c><v>2</v></c></row>
            <row><c><v>3</v></c></row>
        </sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.rows[0].cells[0].at, CellRef { row: 0, col: 0 });
        assert_eq!(s.rows[0].cells[1].at, CellRef { row: 0, col: 1 });
        assert_eq!(s.rows[1].cells[0].at, CellRef { row: 1, col: 0 });
        assert_eq!(s.stats.inferred_positions, 3);
    }

    /// A partial `r=` run must not shift the cells that do declare one.
    #[test]
    fn an_explicit_ref_resets_the_running_column() {
        let xml = r#"<worksheet><sheetData>
            <row r="1"><c><v>1</v></c><c r="E1"><v>2</v></c><c><v>3</v></c></row>
        </sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        let cols: Vec<u32> = s.rows[0].cells.iter().map(|c| c.at.col).collect();
        assert_eq!(cols, vec![0, 4, 5]);
    }

    /// `<c t="s"></c>` — a blank cell a producer wrote with an explicit close
    /// tag instead of self-closing. It is styled-but-blank, not a broken
    /// index; one corpus workbook has 480 of them.
    #[test]
    fn an_empty_shared_string_cell_is_blank_not_dangling() {
        let xml = r#"<worksheet><sheetData><row r="1">
            <c r="A1" s="3" t="s"></c><c r="B1" t="s"><v>0</v></c>
        </row></sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.cell_count(), 1);
        assert_eq!(s.stats.dangling_shared_strings, 0);
        assert_eq!(s.stats.empty_styled_cells, 1);
    }

    /// A `t="s"` cell whose `<v>` is not an index *is* corruption, and must
    /// stay visible in the stats rather than being folded into the blanks.
    #[test]
    fn an_unparseable_shared_string_index_is_reported() {
        let xml = r#"<worksheet><sheetData><row r="1">
            <c r="A1" t="s"><v>seven</v></c>
        </row></sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.stats.dangling_shared_strings, 1);
    }

    #[test]
    fn a_number_that_will_not_parse_is_dropped_not_nan() {
        let xml = r#"<worksheet><sheetData><row r="1">
            <c r="A1"><v>#N/A</v></c><c r="B1"><v>7</v></c>
        </row></sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.rows[0].cells.len(), 1);
        assert_eq!(s.rows[0].cells[0].value, CellValue::Number(7.0));
    }

    #[test]
    fn a_truncated_sheet_yields_what_was_read() {
        let xml = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>5</v>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.cell_count(), 1);
        assert_eq!(s.rows[0].cells[0].value, CellValue::Number(5.0));
    }

    #[test]
    fn merge_lookup_distinguishes_the_anchor_from_the_covered_cells() {
        let s = sheet();
        let a1 = parse_cell("A1").unwrap();
        let b1 = parse_cell("B1").unwrap();
        assert!(s.merge_anchored_at(a1).is_some());
        assert!(s.merge_anchored_at(b1).is_none());
        assert!(s.merge_covering(b1).is_some());
    }

    #[test]
    fn an_empty_sheet_parses_to_nothing() {
        let s = parse("s", true, b"<worksheet><sheetData/></worksheet>").unwrap();
        assert_eq!(s.cell_count(), 0);
        assert!(s.rows.is_empty());
    }
}
