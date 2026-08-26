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
//! * **Value-less cells are dropped from [`Row::cells`].** `<c r="A1" s="7"/>`
//!   carries a style and no content — usually a bordered-but-blank region, and
//!   in some producers an entire pre-formatted sheet. They are counted in
//!   [`SheetStats`] and, when they carry a style at all, kept in the
//!   *paint-only* side-channel [`Row::styled_blanks`] — never in `cells`,
//!   where they would change what the emitter, the block slicer and the
//!   geometry pass all read as "a row / column that exists".
//!
//! # The paint side-channel
//!
//! `xlsx_unvalued_paint_census` over the 1,248-workbook corpus: **57.6% of all
//! declared paint reaches the file and not a reader built on valued cells
//! alone**, in three carriers — value-less styled cells, `<row customFormat>`
//! and `<col style>`. [`Row::styled_blanks`], [`Row::style`] and
//! [`ColInfo::style`] are those three, and they exist for the raster; nothing
//! that reads text sees them.
//!
//! `styled_blanks` is pruned at `</row>`: a row with no valued cell is not a
//! row any consumer of this reader emits, so its blanks are unreachable ink
//! and are dropped rather than stored. That prune is also the memory bound —
//! it is what keeps the pre-formatted-region sheets (thousands of blank rows
//! past the data, class C of the census, 41.7% of all paint) from being held
//! at all.

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

/// A value-less `<c s=…>`: the style it carries and the column it carries it
/// on. Paint only — see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StyledBlank {
    pub col: u32,
    /// The `s=` index. A blank with no `s=` is not recorded: it declares
    /// nothing to paint.
    pub style: u32,
}

/// A row that exists in the file, with its own metrics.
#[derive(Clone, Debug, Default)]
pub struct Row {
    /// Zero-based.
    pub index: u32,
    /// `ht=` in points, when the row states its own height.
    pub height: Option<f64>,
    pub hidden: bool,
    /// `<row s=… customFormat="1">`: a format for every cell of the row that
    /// states none of its own. Recorded **only** under `customFormat`, which
    /// is the attribute that says the `s=` is meant rather than inherited
    /// (§18.3.1.73); 33,562 rows in the corpus declare one, carrying 482,334
    /// cells of paint.
    pub style: Option<u32>,
    /// Cells in written order, which is ascending column order in every
    /// producer seen. Sparse: absent columns are absent.
    pub cells: Vec<Cell>,
    /// Value-less styled cells of this row, in written order. A paint-only
    /// side-channel: it is deliberately *not* part of `cells`, because every
    /// consumer derives "this row has content" and "this column is used" from
    /// that field. Empty for a row with no valued cell (see the module docs).
    pub styled_blanks: Vec<StyledBlank>,
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
    /// `<col style=…>`: a format for every cell of the span that states none
    /// of its own, and none through its row. Paint only, like
    /// [`Row::styled_blanks`]: 8,372 corpus spans declare one, carrying
    /// 163,350 cells of ink.
    pub style: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Hyperlink {
    pub at: RangeRef,
    /// The `r:id` to resolve against the sheet's own relationships. Absent for
    /// an in-workbook link, which carries only a `location`.
    pub rel_id: Option<String>,
    /// The external URL [`rel_id`](Self::rel_id) resolves to. Filled by
    /// [`crate::xlsx::read`], which holds the sheet's relationships part; this
    /// parser alone cannot resolve it and leaves `None`.
    pub url: Option<String>,
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
    /// `<sheetView showGridLines=…>`, from the first view only. `None` means
    /// the file did not say, which is Excel's default of *visible* — read it
    /// through [`Sheet::gridlines_visible`] rather than unwrapping to
    /// `false`. It matters to the raster and nothing else: 12.1% of corpus
    /// sheets declare neither a fill nor a border anywhere, so gridlines are
    /// the only ink holding their numbers in a grid.
    pub show_gridlines: Option<bool>,
    /// `<drawing r:id>`: the sheet's DrawingML part, scoped to the sheet's
    /// own relationships. At most one per §18.3.1.36.
    pub drawing_rel_id: Option<String>,
    /// Pictures placed over the grid, resolved by [`crate::xlsx::read`] from
    /// the drawing part — empty until then, and empty for the 69% of corpus
    /// workbooks whose sheets draw nothing.
    pub pics: Vec<crate::xlsx::drawings::SheetPic>,
    /// Visible text-bearing shapes floating over the grid, from the same
    /// drawing part as `pics`. Titles, form labels, instructions — content
    /// no cell holds.
    pub shapes: Vec<crate::xlsx::drawings::SheetShape>,
    /// The drawing part's *paint* channel: every top-level `sp`/`grpSp`/
    /// `cxnSp` as the shared DrawingML shape tree, fills and outlines and
    /// group child spaces intact. Parallel to `shapes` — a text-bearing
    /// shape appears in both, one carrying its words, the other its box's
    /// ink — and never feeds items or markdown.
    pub ink: Vec<crate::xlsx::drawings::SheetInk>,
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

    /// Are the sheet's gridlines drawn? Undeclared means yes (§18.3.1.87).
    pub fn gridlines_visible(&self) -> bool {
        self.show_gridlines.unwrap_or(true)
    }

    /// The format a `<col>` span declares for the columns it covers, if any.
    /// Paint only — the emitter never asks.
    pub fn col_style(&self, col: u32) -> Option<u32> {
        self.cols
            .iter()
            .find(|c| col >= c.min && col <= c.max)
            .and_then(|c| c.style)
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
                    b"sheetView" => {
                        // First view only, for the reason `<pane>` below is
                        // gated: later `<sheetView>` elements describe other
                        // windows onto the same sheet, and a
                        // `<customSheetView>` a saved one.
                        if sheet.show_gridlines.is_none() {
                            sheet.show_gridlines = Some(attr_bool(e, b"showGridLines", true));
                        }
                        in_sheet_view = !empty;
                    }
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
                            style: attr_parse(e, b"style"),
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
                            // `s=` without `customFormat="1"` is the row's
                            // *inherited* format written out; taking it would
                            // paint every default row with style 0's fill.
                            style: attr_bool(e, b"customFormat", false)
                                .then(|| attr_parse(e, b"s"))
                                .flatten(),
                            cells: Vec::new(),
                            styled_blanks: Vec::new(),
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
                            record_styled_blank(pending.at.col, pending.style, &mut row);
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
                                url: None,
                                location: attr(e, b"location"),
                                tooltip: attr(e, b"tooltip"),
                            });
                        }
                    }
                    b"autoFilter" => {
                        sheet.auto_filter = attr(e, b"ref").as_deref().and_then(parse_range);
                    }
                    b"drawing" => {
                        sheet.drawing_rel_id = attr(e, b"r:id");
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
                        sheet.rows.push(prune_blanks(r));
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
        sheet.rows.push(prune_blanks(r));
    }
    Ok(sheet)
}

/// Drop a row's paint side-channel when the row carries no value: no consumer
/// emits such a row, so the ink has nowhere to land. This is what bounds the
/// reader's memory on a pre-formatted sheet — see the module docs.
fn prune_blanks(mut row: Row) -> Row {
    if row.cells.is_empty() && !row.styled_blanks.is_empty() {
        row.styled_blanks = Vec::new();
    }
    row
}

/// Send a value-less `<c s=…>` to the paint side-channel. A blank outside any
/// `<row>` is dropped: the row `finish_cell` would invent for it has no valued
/// cell, so the prune at `</row>` would discard it anyway.
fn record_styled_blank(col: u32, style: Option<u32>, row: &mut Option<Row>) {
    let (Some(style), Some(r)) = (style, row.as_mut()) else {
        return;
    };
    r.styled_blanks.push(StyledBlank { col, style });
}

struct PendingCell {
    at: CellRef,
    style: Option<u32>,
    kind: String,
    has_formula: bool,
    text: Option<RichText>,
}

fn finish_cell(pending: PendingCell, raw: &str, row: &mut Option<Row>, sheet: &mut Sheet) {
    // Read before the match moves `pending.text`; the blank path below needs
    // only these two.
    let blank = (pending.at.col, pending.style);
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
        record_styled_blank(blank.0, blank.1, row);
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

    /// The paint side-channel: the blank keeps its style, `cells` does not
    /// grow, and the stats keep counting it.
    #[test]
    fn styled_blanks_reach_the_paint_side_channel_only() {
        let s = sheet();
        assert_eq!(s.rows[0].cells.len(), 1, "B1 is still not a cell");
        assert_eq!(
            s.rows[0].styled_blanks,
            vec![StyledBlank { col: 1, style: 2 }]
        );
        assert_eq!(s.stats.empty_styled_cells, 1);
    }

    /// A blank with no `s=` declares nothing to paint and is not recorded.
    #[test]
    fn an_unstyled_blank_is_not_recorded() {
        let xml = r#"<worksheet><sheetData><row r="1">
            <c r="A1"/><c r="B1"><v>1</v></c>
        </row></sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert!(s.rows[0].styled_blanks.is_empty());
        assert_eq!(s.stats.empty_styled_cells, 1);
    }

    /// The memory bound and the class-C rule in one: a row with no value is
    /// emitted by nobody, so its blanks are dropped rather than stored — for
    /// the row written `<c/>`-style and for the one written `<c></c>`.
    #[test]
    fn a_valueless_row_keeps_its_metrics_and_drops_its_blanks() {
        let xml = r#"<worksheet><sheetData>
            <row r="1" ht="30"><c r="A1" s="4"/><c r="B1" s="4"></c></row>
            <row r="2"><c r="A2" s="5"/><c r="B2"><v>1</v></c></row>
        </sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.rows[0].height, Some(30.0), "the row itself survives");
        assert!(s.rows[0].styled_blanks.is_empty());
        assert_eq!(
            s.rows[1].styled_blanks,
            vec![StyledBlank { col: 0, style: 5 }],
            "a row with a value keeps its blanks"
        );
        assert_eq!(s.stats.empty_styled_cells, 3);
    }

    /// `<row s=>` counts only under `customFormat="1"`; `<col style=>` always
    /// does.
    #[test]
    fn row_and_column_formats_are_read_for_paint() {
        let xml = r#"<worksheet>
            <cols><col min="1" max="2" width="9" style="6"/><col min="3" max="3" width="9"/></cols>
            <sheetData>
              <row r="1" s="8" customFormat="1"><c r="A1"><v>1</v></c></row>
              <row r="2" s="9"><c r="A2"><v>2</v></c></row>
            </sheetData></worksheet>"#;
        let s = parse("s", true, xml.as_bytes()).unwrap();
        assert_eq!(s.rows[0].style, Some(8));
        assert_eq!(s.rows[1].style, None, "no customFormat, no claim");
        assert_eq!(s.col_style(0), Some(6));
        assert_eq!(s.col_style(1), Some(6));
        assert_eq!(s.col_style(2), None);
        assert_eq!(s.col_style(9), None, "outside every span");
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
