//! XLSX geometry: the planned grid → [`Page`]s with per-cell [`TextItem`]s.
//!
//! Unlike DOCX (a layout engine) and PPTX (a cascade), XLSX geometry is
//! *stated*: the file declares every column width and row height, so this
//! pass is unit conversion plus pagination — no fonts, no measurement, no
//! host dependence. Every constant traces to `xlsx_geometry_census` over the
//! 1,248-workbook corpus:
//!
//! * **Packed, not canvas.** Item positions honour the emitted grid (rows
//!   the file wrote, columns holding at least one cell), the same sparse
//!   rule as the emitter. Honouring declared positions instead puts one
//!   sheet's canvas at 16,777,274 pt tall (a stray cell on row 1,048,576);
//!   packed and canvas differ by >2× on only 256 of 5,606 sheets.
//! * **Pages are Letter-height, content-width.** At 792 pt, 49% of sheets
//!   paginate (a sheet is not a page: the corpus median workbook is 4 pages,
//!   the largest 6,612) — but 67.5% of sheets are *wider* than Letter, so
//!   width is never split: the page takes the content's width and a
//!   downstream consumer sees an unclipped row, which is the native path's
//!   whole advantage over LibreOffice.
//! * **Geometry inputs are declared, not guessed**: 100% of sheets state
//!   `defaultRowHeight`, 97% of retained columns have a declared width. The
//!   8.43-unit / 15 pt fallbacks carry the remainder.
//! * **A merge cut by a page break** (1.05% of merges) keeps its item on the
//!   anchor's page, clamped to that page's rows — mirroring the block
//!   slicer, where the continuation is an empty spanning cell.
//!
//! An item's rectangle is the **cell box** (inset by the cell padding the
//! pixel formula bakes in), not a measured text box: the file states no
//! glyph positions, and the box the grid declares is what a bbox consumer
//! (extraction join, highlight) wants. The inset also keeps adjacent cells
//! from touching, which is what `native_page_text` uses to put spaces
//! between cells on a line.

use std::ops::Range;

use liteparse_ooxml::xlsx::{CellAnchor, PicAnchor, Sheet, Workbook};

use super::figures::FigureSink;
use super::xlsx::{EmitOptions, SHEET_HEADING_LEVEL, SheetPlan};
use crate::markdown_layout::{Block, escape_inline};
use crate::types::{ExtractedImage, OutlineTarget, Page, Rect, TextItem};

/// The write-side constants from rust_xlsxwriter: Calibri 11's max digit
/// width is 7 px at 96 DPI, plus 5 px of cell padding.
const MDW: f64 = 7.0;
const PAD_PX: f64 = 5.0;
const PX_TO_PT: f64 = 72.0 / 96.0;
/// Excel's fallbacks when the file declares nothing (3% of retained columns,
/// 0% of corpus sheets for row height).
const DEFAULT_COL_WIDTH: f64 = 8.43;
const DEFAULT_ROW_HEIGHT_PT: f64 = 15.0;

/// US Letter portrait height; width is content-driven and never split.
const PAGE_HEIGHT: f32 = 792.0;
const MARGIN: f32 = 36.0;
/// Rows chunk greedily into this much vertical space per page.
const USABLE_HEIGHT: f64 = (PAGE_HEIGHT - 2.0 * MARGIN) as f64;
/// Pages narrower than Letter portrait are padded up to it — a two-column
/// sheet still gets a page-shaped page.
const MIN_PAGE_WIDTH: f32 = 612.0;
/// Horizontal inset of an item inside its cell: half the cell padding the
/// pixel formula already contains, in points.
const TEXT_INSET: f32 = (PAD_PX / 2.0 * PX_TO_PT) as f32;

/// Everything the native XLSX pipeline hands `parser.rs`.
pub struct NativeXlsx {
    /// One or more pages per sheet (every sheet gets at least one, so a page
    /// exists for each heading), page numbers 1-based across the workbook.
    pub pages: Vec<Page>,
    /// Blocks per page, aligned with `pages`: the sheet heading and banners
    /// on the sheet's first page, then the table rows that are actually on
    /// each page (a merge cut by the break leaves an empty spanning cell on
    /// the continuation).
    pub page_blocks: Vec<Vec<Block>>,
    /// Doc-level blocks: each sheet's table *unsplit*, the shape the
    /// markdown deliverable wants.
    pub all_blocks: Vec<Block>,
    /// One level-1 entry per sheet at its first page, matching the emitter's
    /// heading rank.
    pub outline: Vec<OutlineTarget>,
    /// Extracted picture bytes, populated only under `EmitOptions::images`.
    /// Ids are `s{sheet}_{n}` in [`super::xlsx::ordered_pics`] order, the
    /// same numbering the `Block::Figure` refs carry.
    pub images: Vec<ExtractedImage>,
    /// Per-page picture rectangles, aligned with `pages` — the media-rect
    /// input to page complexity, computed whether or not images were asked
    /// for (a rectangle costs no byte copies).
    pub pic_rects: Vec<Vec<Rect>>,
}

/// EMU → points: 914,400 EMU to the inch, 72 points to the inch.
fn emu_pt(v: i64) -> f64 {
    v as f64 / 12_700.0
}

/// The *declared* grid, for measuring picture extents: every column and row
/// the sheet addresses, written or not, at its declared (or default) size.
/// Row sums are arithmetic over a map of explicit heights — never an
/// iteration over the span, which the row-1,048,576 stray cell would turn
/// into a hang.
struct CanvasGrid<'a> {
    sheet: &'a Sheet,
    /// Rows with an explicit `ht`, by index.
    explicit_rows: std::collections::BTreeMap<u32, f64>,
    default_row_h: f64,
}

impl<'a> CanvasGrid<'a> {
    fn build(sheet: &'a Sheet) -> Self {
        CanvasGrid {
            sheet,
            explicit_rows: sheet
                .rows
                .iter()
                .filter_map(|r| r.height.map(|h| (r.index, h.max(0.0))))
                .collect(),
            default_row_h: sheet.default_row_height.unwrap_or(DEFAULT_ROW_HEIGHT_PT),
        }
    }

    /// Canvas width between two anchor corners, in points.
    fn col_span(&self, from: CellAnchor, to: CellAnchor) -> f64 {
        // Columns are bounded at 16,384 (§18.17.2), so direct iteration is
        // safe; a malformed `to` past the bound is clamped.
        let hi = to.col.min(16_384);
        let mut w = 0.0;
        for c in from.col..hi {
            w += col_px(
                self.sheet
                    .col_width(c)
                    .unwrap_or(DEFAULT_COL_WIDTH)
                    .max(0.0),
            ) * PX_TO_PT;
        }
        w - emu_pt(from.col_off_emu).max(0.0) + emu_pt(to.col_off_emu).max(0.0)
    }

    /// Canvas height between two anchor corners, in points.
    fn row_span(&self, from: CellAnchor, to: CellAnchor) -> f64 {
        if to.row <= from.row {
            return emu_pt(to.row_off_emu - from.row_off_emu);
        }
        let (mut sum, mut counted) = (0.0, 0u64);
        for (_, h) in self.explicit_rows.range(from.row..to.row) {
            sum += h;
            counted += 1;
        }
        let n = u64::from(to.row - from.row);
        sum += (n - counted) as f64 * self.default_row_h;
        sum - emu_pt(from.row_off_emu).max(0.0) + emu_pt(to.row_off_emu).max(0.0)
    }
}

/// Excel width units → pixels, the xlsxwriter algorithm: sub-unit widths
/// scale the padded unit instead of adding padding.
fn col_px(w: f64) -> f64 {
    if w < 1.0 {
        (w * (MDW + PAD_PX) + 0.5).floor()
    } else {
        (w * MDW + 0.5).floor() + PAD_PX
    }
}

/// Lay out a whole workbook. Infallible past a successful `xlsx::read`, like
/// the emitter: a sheet with no content still yields one empty page.
pub fn workbook_to_pages(wb: &Workbook, opts: EmitOptions) -> NativeXlsx {
    let mut out = NativeXlsx {
        pages: Vec::new(),
        page_blocks: Vec::new(),
        all_blocks: Vec::new(),
        outline: Vec::new(),
        images: Vec::new(),
        pic_rects: Vec::new(),
    };
    // One sink across the workbook: a logo placed on every sheet dedups to
    // one canonical entry, the same cross-page rule the PPTX path applies
    // across slides.
    let mut sink = FigureSink::default();

    for (si, sheet) in wb.sheets.iter().enumerate() {
        let heading = Block::Heading {
            level: SHEET_HEADING_LEVEL,
            text: escape_inline(&sheet.name),
        };
        out.outline.push(OutlineTarget {
            level: SHEET_HEADING_LEVEL,
            title: escape_inline(&sheet.name),
            page_index: out.pages.len() as i32,
            y_pdf: None,
        });
        out.all_blocks.push(heading.clone());

        let plan = SheetPlan::build(wb, sheet, opts);
        // A sheet with no written cells still needs geometry when it draws
        // pictures — the image-only sheet is exactly the `extract_images`
        // case — so the plan-less path gets an empty grid rather than a
        // bail-out: every anchor collapses to the page origin and the
        // picture's own extent survives.
        let (geo, ranges, row_indices, cols): (SheetGeometry, Vec<Range<usize>>, Vec<u32>, &[u32]) =
            match &plan {
                Some(p) => {
                    let geo = SheetGeometry::build(sheet, p);
                    let ranges = geo.page_ranges();
                    let rows: Vec<u32> = p.rows.iter().map(|(r, _)| r.index).collect();
                    (geo, ranges, rows, &p.cols)
                }
                None => (
                    SheetGeometry {
                        x_off: vec![0.0],
                        y_off: vec![0.0],
                    },
                    vec![0..0],
                    Vec::new(),
                    &[],
                ),
            };

        // Doc-level: the unsplit emission (one full-range slice), then the
        // sheet's figures — the same order `emit_workbook` writes, so the
        // two doc emissions stay byte-identical.
        if let Some(p) = &plan {
            let n = p.rows.len();
            for page in p.page_blocks(wb, &[0..n]) {
                out.all_blocks.extend(page);
            }
        }
        if opts.figures {
            out.all_blocks.extend(super::xlsx::figure_blocks(sheet, si));
        }

        let mut blocks_per_page: Vec<Vec<Block>> = match &plan {
            Some(p) => p.page_blocks(wb, &ranges),
            None => vec![Vec::new()],
        };
        blocks_per_page[0].insert(0, heading);

        // Pictures: page assignment + rect from the packed grid, figure
        // blocks on the page the anchor lands on, bytes into the sink. The
        // local ordinal mirrors `figure_blocks` (both skip media we do not
        // surface), and the sink's id must agree with it by construction.
        let grid_w = *geo.x_off.last().unwrap() as f32;
        let page_width = (grid_w + 2.0 * MARGIN).max(MIN_PAGE_WIDTH);
        let mut rects_per_page: Vec<Vec<Rect>> = vec![Vec::new(); ranges.len()];
        let canvas = CanvasGrid::build(sheet);
        sink.reset_ordinal();
        let mut n_fig = 0u32;
        for pic in super::xlsx::ordered_pics(sheet) {
            let Some(ext) = super::docx_layout::media_extension(pic.format) else {
                continue;
            };
            n_fig += 1;
            let id = format!("s{}_{n_fig}", si + 1);
            let (page_local, rect) = geo.place_pic(
                cols,
                &row_indices,
                &ranges,
                page_width,
                &canvas,
                &pic.anchor,
            );
            rects_per_page[page_local].push(rect.clone());
            if opts.figures {
                blocks_per_page[page_local].push(Block::Figure {
                    id: id.clone(),
                    format: ext.to_string(),
                });
            }
            if opts.images {
                let page_number = (out.pages.len() + 1 + page_local) as u32;
                let placed = sink.place(
                    &pic.bytes,
                    pic.format,
                    &format!("s{}", si + 1),
                    page_number,
                    rect,
                );
                debug_assert_eq!(
                    placed.as_ref().map(|(i, _)| i.as_str()),
                    Some(id.as_str()),
                    "figure ref and image entry numbered apart"
                );
            }
        }

        for (i, blocks) in blocks_per_page.into_iter().enumerate() {
            let page = match &plan {
                Some(p) => geo.build_page(wb, p, ranges[i].clone(), out.pages.len() + 1),
                None => empty_page(out.pages.len() + 1),
            };
            out.pages.push(page);
            out.page_blocks.push(blocks);
            out.pic_rects.push(std::mem::take(&mut rects_per_page[i]));
        }
    }
    out.images = sink.images;
    out
}

fn empty_page(page_number: usize) -> Page {
    Page {
        page_number,
        page_width: MIN_PAGE_WIDTH,
        page_height: PAGE_HEIGHT,
        content_bounds: None,
        text_items: Vec::new(),
        graphics: Vec::new(),
        vector_graphics: None,
        struct_nodes: Vec::new(),
        image_refs: Vec::new(),
        annotations: None,
        form_fields: None,
        structure_tree: None,
    }
}

/// The resolved sizes of one sheet's emitted grid: prefix sums so a rect is
/// two subtractions, in points throughout.
struct SheetGeometry {
    /// `x_off[i]` = left edge of retained column `i` relative to the grid
    /// origin; `x_off[cols.len()]` = grid width.
    x_off: Vec<f64>,
    /// `y_off[i]` = top edge of emitted row `i` from the grid top;
    /// `y_off[rows.len()]` = grid height.
    y_off: Vec<f64>,
}

impl SheetGeometry {
    fn build(sheet: &Sheet, plan: &SheetPlan<'_>) -> Self {
        let default_h = sheet.default_row_height.unwrap_or(DEFAULT_ROW_HEIGHT_PT);
        let mut x_off = Vec::with_capacity(plan.cols.len() + 1);
        let mut acc = 0.0;
        x_off.push(0.0);
        for &c in &plan.cols {
            let w = sheet.col_width(c).unwrap_or(DEFAULT_COL_WIDTH).max(0.0);
            acc += col_px(w) * PX_TO_PT;
            x_off.push(acc);
        }
        let mut y_off = Vec::with_capacity(plan.rows.len() + 1);
        let mut acc = 0.0;
        y_off.push(0.0);
        for (row, _) in &plan.rows {
            // Hidden rows keep their declared height: their text is emitted
            // (the deliberate divergence from what Excel prints), so their
            // geometry must be somewhere real too.
            acc += row.height.unwrap_or(default_h).max(0.0);
            y_off.push(acc);
        }
        SheetGeometry { x_off, y_off }
    }

    fn row_height(&self, i: usize) -> f64 {
        self.y_off[i + 1] - self.y_off[i]
    }

    /// A cell-anchor corner in packed-grid points. A retained column /
    /// emitted row keeps its EMU offset (clamped inside the cell); an anchor
    /// on a compacted column or unwritten row collapses to the boundary
    /// where the packed grid resumes, and the offset — which measures into
    /// space the packed grid does not have — is dropped.
    fn corner_pt(&self, cols: &[u32], row_indices: &[u32], c: CellAnchor) -> (f64, f64) {
        let ci = cols.partition_point(|&x| x < c.col);
        let x = if ci < cols.len() && cols[ci] == c.col {
            let w = self.x_off[ci + 1] - self.x_off[ci];
            self.x_off[ci] + emu_pt(c.col_off_emu).clamp(0.0, w)
        } else {
            self.x_off[ci]
        };
        let ri = row_indices.partition_point(|&r| r < c.row);
        let y = if ri < row_indices.len() && row_indices[ri] == c.row {
            let h = self.y_off[ri + 1] - self.y_off[ri];
            self.y_off[ri] + emu_pt(c.row_off_emu).clamp(0.0, h)
        } else {
            self.y_off[ri]
        };
        (x, y)
    }

    /// Where a picture lands: the local page index within `ranges` plus its
    /// rect on that page, clamped into the page box. The page is the one
    /// whose rows contain the picture's *top* edge — a picture crossing the
    /// break truncates at its anchor page's bottom, the merge-item precedent
    /// — and the page's width stays the text grid's, so a picture hanging
    /// past it truncates at the right edge rather than growing every item's
    /// page.
    ///
    /// Position is packed, **extent is canvas**: a two-cell anchor's corners
    /// are measured over the *declared* grid (`canvas`), because the corpus
    /// says the packed difference is a lie — 818 of 2,126 pictures anchor
    /// across rows or columns the packed grid does not have (a logo floating
    /// right of the data, a photo below the table) and collapse to 1 pt
    /// slivers under a packed reading. The top-left still snaps to the
    /// packed corner so the picture sits next to the text it belongs with.
    fn place_pic(
        &self,
        cols: &[u32],
        row_indices: &[u32],
        ranges: &[Range<usize>],
        page_width: f32,
        canvas: &CanvasGrid<'_>,
        anchor: &PicAnchor,
    ) -> (usize, Rect) {
        let (gx, gy, gw, gh) = match *anchor {
            PicAnchor::OneCell { from, ext_emu } => {
                let (x, y) = self.corner_pt(cols, row_indices, from);
                (x, y, emu_pt(ext_emu.0).max(1.0), emu_pt(ext_emu.1).max(1.0))
            }
            PicAnchor::TwoCell { from, to } => {
                let (x0, y0) = self.corner_pt(cols, row_indices, from);
                (
                    x0,
                    y0,
                    canvas.col_span(from, to).max(1.0),
                    canvas.row_span(from, to).max(1.0),
                )
            }
            PicAnchor::Absolute { pos_emu, ext_emu } => (
                emu_pt(pos_emu.0).max(0.0),
                emu_pt(pos_emu.1).max(0.0),
                emu_pt(ext_emu.0).max(1.0),
                emu_pt(ext_emu.1).max(1.0),
            ),
        };
        let mut page = 0;
        for (i, r) in ranges.iter().enumerate() {
            if self.y_off[r.start] <= gy {
                page = i;
            } else {
                break;
            }
        }
        let y_base = self.y_off[ranges[page].start];
        let usable_w = (page_width - 2.0 * MARGIN) as f64;
        let x_rel = gx.max(0.0).min(usable_w - 1.0);
        let w = gw.min(usable_w - x_rel).max(1.0);
        let y_rel = (gy - y_base).max(0.0).min(USABLE_HEIGHT - 1.0);
        let h = gh.min(USABLE_HEIGHT - y_rel).max(1.0);
        (
            page,
            Rect {
                x: (f64::from(MARGIN) + x_rel) as f32,
                y: (f64::from(MARGIN) + y_rel) as f32,
                width: w as f32,
                height: h as f32,
            },
        )
    }

    /// Greedy pagination over emitted rows: break before the row that would
    /// overflow the page, never breaking an empty page (a row taller than
    /// the page — the corpus max is 410 pt — gets a page to itself).
    fn page_ranges(&self) -> Vec<Range<usize>> {
        let n = self.y_off.len() - 1;
        let mut ranges = Vec::new();
        let mut start = 0;
        let mut acc = 0.0;
        for i in 0..n {
            let h = self.row_height(i);
            if acc > 0.0 && acc + h > USABLE_HEIGHT {
                ranges.push(start..i);
                start = i;
                acc = 0.0;
            }
            acc += h;
        }
        ranges.push(start..n);
        ranges
    }

    /// One page: the rows of `range`, each non-empty cell an item at its
    /// cell box, merge anchors at the merge's box clamped to this page.
    fn build_page(
        &self,
        wb: &Workbook,
        plan: &SheetPlan<'_>,
        range: Range<usize>,
        page_number: usize,
    ) -> Page {
        let grid_w = *self.x_off.last().unwrap();
        let page_width = (grid_w as f32 + 2.0 * MARGIN).max(MIN_PAGE_WIDTH);
        let y_base = self.y_off[range.start];

        let mut text_items: Vec<TextItem> = Vec::new();
        for i in range.clone() {
            let (row, cells) = &plan.rows[i];
            let y = MARGIN + (self.y_off[i] - y_base) as f32;
            let row_h = self.row_height(i) as f32;
            for cell in cells {
                let text = wb.display_text(cell).unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let ci = plan.cols.binary_search(&cell.at.col).unwrap_or_else(|_| {
                    unreachable!("retained columns are the union of all cells")
                });
                let (x, w, h) = match plan.anchors.get(&(row.index, cell.at.col)) {
                    Some(&idx) => {
                        let m = &plan.plans[idx];
                        // Column extent of the merge over retained columns.
                        let clo = plan.cols.partition_point(|&c| c < m.col_range.0);
                        let chi = plan.cols.partition_point(|&c| c <= m.col_range.1);
                        // Row extent, clamped to this page's rows: the block
                        // slicer clamps the rowspan the same way.
                        let mut j = i;
                        while j + 1 < range.end && plan.rows[j + 1].0.index <= m.row_range.1 {
                            j += 1;
                        }
                        (
                            self.x_off[clo],
                            self.x_off[chi] - self.x_off[clo],
                            (self.y_off[j + 1] - self.y_off[i]) as f32,
                        )
                    }
                    None => (self.x_off[ci], self.x_off[ci + 1] - self.x_off[ci], row_h),
                };
                let font = wb.styles.font(cell.style);
                text_items.push(TextItem {
                    text: text.to_string(),
                    x: MARGIN + x as f32 + TEXT_INSET,
                    y,
                    width: (w as f32 - 2.0 * TEXT_INSET).max(0.0),
                    height: h,
                    font_name: font.name.clone(),
                    font_size: font.size,
                    font_weight: font.bold.then_some(700),
                    link: plan.link_at(row.index, cell.at.col).map(str::to_string),
                    ..Default::default()
                });
            }
        }

        let content_bounds = text_items
            .iter()
            .map(|t| (t.x, t.y, t.x + t.width, t.y + t.height))
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
            .map(|(x0, y0, x1, y1)| Rect {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
            });

        Page {
            page_number,
            page_width,
            page_height: PAGE_HEIGHT,
            content_bounds,
            text_items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations: None,
            form_fields: None,
            structure_tree: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::office::xlsx::tests::workbook_from;

    fn layout_of(sheet_xml: &str, extra: &[(&str, &str)]) -> NativeXlsx {
        let wb = workbook_from(sheet_xml, extra);
        workbook_to_pages(&wb, EmitOptions::default())
    }

    /// w=10 → px = round(10×7)+5 = 75 → 56.25 pt. The second column starts
    /// where the first ends, and items are inset off the shared border.
    #[test]
    fn declared_column_widths_place_cells_side_by_side() {
        let nx = layout_of(
            r#"<worksheet>
            <cols><col min="1" max="1" width="10" customWidth="1"/><col min="2" max="2" width="10" customWidth="1"/></cols>
            <sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" t="inlineStr"><is><t>b</t></is></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c></row>
            </sheetData></worksheet>"#,
            &[],
        );
        assert_eq!(nx.pages.len(), 1);
        let items = &nx.pages[0].text_items;
        let a = &items[0];
        let b = &items[1];
        assert_eq!(a.x, MARGIN + TEXT_INSET);
        assert!(
            (b.x - (MARGIN + 56.25 + TEXT_INSET)).abs() < 1e-4,
            "b.x = {}",
            b.x
        );
        // The inset leaves a real gap between adjacent cells, which is what
        // native_page_text keys word spacing on.
        assert!(b.x - (a.x + a.width) > 1.0);
    }

    /// 15 rows at 100 pt against 720 pt of usable height: 7 + 7 + 1.
    #[test]
    fn tall_sheets_paginate_and_the_slices_match() {
        let rows_xml: String = (1..=15)
            .map(|r| format!(r#"<row r="{r}" ht="100"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#))
            .collect();
        let nx = layout_of(
            &format!("<worksheet><sheetData>{rows_xml}</sheetData></worksheet>"),
            &[],
        );
        assert_eq!(nx.pages.len(), 3);
        assert_eq!(nx.pages[0].text_items.len(), 14);
        assert_eq!(nx.pages[2].text_items.len(), 2);
        // Every page restarts at the margin.
        for p in &nx.pages {
            assert_eq!(p.text_items[0].y, MARGIN);
            assert_eq!(p.page_height, PAGE_HEIGHT);
        }
        // Per-page tables hold exactly the rows on that page; the doc-level
        // table is unsplit.
        let table_rows = |blocks: &[Block]| {
            blocks
                .iter()
                .find_map(|b| match b {
                    Block::MergedTable { rows, .. } => Some(rows.len()),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(table_rows(&nx.page_blocks[0]), 7);
        assert_eq!(table_rows(&nx.page_blocks[1]), 7);
        assert_eq!(table_rows(&nx.page_blocks[2]), 1);
        assert_eq!(table_rows(&nx.all_blocks), 15);
    }

    /// A merge anchor's item covers the merged box; cut by a page break it
    /// clamps to its own page, like the block slicer's rowspan.
    #[test]
    fn merge_rects_span_and_clamp_at_the_break() {
        let rows_xml: String = (1..=10)
            .map(|r| {
                let extra = if r == 1 {
                    r#"<c r="A1" t="inlineStr"><is><t>tall</t></is></c>"#.to_string()
                } else {
                    String::new()
                };
                format!(r#"<row r="{r}" ht="100">{extra}<c r="B{r}"><v>{r}</v></c></row>"#)
            })
            .collect();
        let nx = layout_of(
            &format!(
                "<worksheet><sheetData>{rows_xml}</sheetData><mergeCells><mergeCell ref=\"A1:A10\"/></mergeCells></worksheet>"
            ),
            &[],
        );
        assert_eq!(nx.pages.len(), 2);
        let tall = nx.pages[0]
            .text_items
            .iter()
            .find(|t| t.text == "tall")
            .unwrap();
        // 7 rows fit on page one; the merge's item stops at the page.
        assert_eq!(tall.height, 700.0);
        assert!(
            !nx.pages[1].text_items.iter().any(|t| t.text == "tall"),
            "the continuation page repeats no text"
        );
    }

    #[test]
    fn an_empty_sheet_is_one_empty_page_with_its_heading() {
        let nx = layout_of("<worksheet><sheetData/></worksheet>", &[]);
        assert_eq!(nx.pages.len(), 1);
        assert!(nx.pages[0].text_items.is_empty());
        assert!(nx.pages[0].content_bounds.is_none());
        assert!(matches!(&nx.page_blocks[0][0], Block::Heading { .. }));
        assert_eq!(nx.outline.len(), 1);
        assert_eq!(nx.outline[0].page_index, 0);
    }

    /// A narrow sheet still gets a Letter-width page; a wide one takes its
    /// content width instead of clipping.
    #[test]
    fn page_width_is_content_width_with_a_letter_floor() {
        let narrow = layout_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
            &[],
        );
        assert_eq!(narrow.pages[0].page_width, MIN_PAGE_WIDTH);

        let cols: String = (1..=30)
            .map(|c| format!(r#"<col min="{c}" max="{c}" width="20" customWidth="1"/>"#))
            .collect();
        let cells: String = (0..30)
            .map(|c| {
                let col = crate::office::xlsx_layout::tests::col_label(c);
                format!(r#"<c r="{col}1"><v>{c}</v></c>"#)
            })
            .collect();
        let wide = layout_of(
            &format!(
                r#"<worksheet><cols>{cols}</cols><sheetData><row r="1">{cells}</row><row r="2"><c r="A2"><v>9</v></c></row></sheetData></worksheet>"#
            ),
            &[],
        );
        // 30 × (round(20×7)+5 = 145 px → 108.75 pt) + margins.
        let expect = 30.0 * 108.75 + 2.0 * MARGIN;
        assert!(
            (wide.pages[0].page_width - expect).abs() < 0.01,
            "got {}",
            wide.pages[0].page_width
        );
    }

    /// The sub-unit branch of the pixel formula: w=0.5 → round(0.5×12) = 6 px.
    #[test]
    fn sub_unit_widths_use_the_padded_scale() {
        assert_eq!(super::col_px(0.5), 6.0);
        assert_eq!(super::col_px(8.43), 64.0);
        assert_eq!(super::col_px(10.0), 75.0);
    }

    /// Hidden rows keep their height — their text is emitted, so their
    /// geometry is real.
    #[test]
    fn hidden_rows_keep_their_declared_height() {
        let nx = layout_of(
            r#"<worksheet><sheetData>
                <row r="1" ht="30" hidden="1"><c r="A1" t="inlineStr"><is><t>secret</t></is></c><c r="B1"><v>1</v></c></row>
                <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>3</v></c></row>
            </sheetData></worksheet>"#,
            &[],
        );
        let items = &nx.pages[0].text_items;
        assert_eq!(items[0].height, 30.0);
        assert_eq!(items[2].y, MARGIN + 30.0);
    }

    /// Items carry raw cell text; the hyperlink is a field, not markdown.
    #[test]
    fn items_carry_raw_text_and_the_link_field() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>site</t></is></c><c r="B1"><v>1</v></c></row>
            <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>3</v></c></row>
        </sheetData><hyperlinks><hyperlink ref="A1" r:id="rId9"/></hyperlinks></worksheet>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/>
</Relationships>"#;
        let wb = workbook_from(sheet, &[("xl/worksheets/_rels/sheet1.xml.rels", rels)]);
        let nx = workbook_to_pages(
            &wb,
            EmitOptions {
                links: true,
                ..Default::default()
            },
        );
        let site = &nx.pages[0].text_items[0];
        assert_eq!(site.text, "site");
        assert_eq!(site.link.as_deref(), Some("https://example.com"));
    }

    pub(crate) fn col_label(mut col: u32) -> String {
        let mut s = String::new();
        loop {
            s.insert(0, (b'A' + (col % 26) as u8) as char);
            if col < 26 {
                break;
            }
            col = col / 26 - 1;
        }
        s
    }

    // ── pictures ────────────────────────────────────────────────────────────

    /// Parts that hang one PNG picture off sheet1 via a drawing part. The
    /// anchor XML is the caller's, so each test states its own placement.
    fn drawing_parts(anchor_xml: &str) -> Vec<(String, String)> {
        let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#;
        let drawing = format!(
            r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">{anchor_xml}</xdr:wsDr>"#
        );
        let drawing_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="../media/image1.png"/>
</Relationships>"#;
        // Real 1x1 PNG bytes are not valid UTF-8, and `workbook_from` writes
        // string parts; the extension decides the format, and dimensions
        // failing to parse is the (0, 0) fallback the sink already has.
        vec![
            (
                "xl/worksheets/_rels/sheet1.xml.rels".to_string(),
                sheet_rels.to_string(),
            ),
            ("xl/drawings/drawing1.xml".to_string(), drawing),
            (
                "xl/drawings/_rels/drawing1.xml.rels".to_string(),
                drawing_rels.to_string(),
            ),
            (
                "xl/media/image1.png".to_string(),
                "not-really-png-but-bytes".to_string(),
            ),
        ]
    }

    fn pic_anchor(body: &str) -> String {
        format!(
            r#"{body}<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="2" name="P"/></xdr:nvPicPr><xdr:blipFill><a:blip r:embed="rId1"/></xdr:blipFill></xdr:pic><xdr:clientData/>"#
        )
    }

    fn layout_with_pic(sheet_xml: &str, anchor_xml: &str, opts: EmitOptions) -> NativeXlsx {
        let parts = drawing_parts(anchor_xml);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let wb = workbook_from(sheet_xml, &extra);
        workbook_to_pages(&wb, opts)
    }

    /// A picture anchored on page 2's rows gets its figure block and rect on
    /// page 2, page-relative — and the `ExtractedImage` id, page and bbox all
    /// agree with the block ref.
    #[test]
    fn a_picture_lands_on_its_anchor_row_page() {
        // 15 rows at 100 pt = 3 pages of 7/7/1 (the pagination test's grid).
        let rows_xml: String = (1..=15)
            .map(|r| format!(r#"<row r="{r}" ht="100"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#))
            .collect();
        let sheet = format!(
            r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>{rows_xml}</sheetData><drawing r:id="rId7"/></worksheet>"#
        );
        // Anchor on row index 8 (sheet row 9): page 2 locally, rows 8..14.
        let anchor = pic_anchor(
            r#"<xdr:twoCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>8</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>10</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:to>"#,
        );
        let nx = layout_with_pic(
            &sheet,
            &format!("<xdr:twoCellAnchor>{}</xdr:twoCellAnchor>", ""),
            EmitOptions::default(),
        );
        // Guard against the helper silently producing nothing.
        assert!(nx.pic_rects.iter().all(|r| r.is_empty()));

        let nx = layout_with_pic(
            &sheet,
            &format!("{anchor}</xdr:twoCellAnchor>"),
            EmitOptions {
                figures: true,
                images: true,
                ..Default::default()
            },
        );
        assert_eq!(nx.pages.len(), 3);
        assert!(nx.pic_rects[0].is_empty() && nx.pic_rects[2].is_empty());
        assert_eq!(nx.pic_rects[1].len(), 1);
        let rect = &nx.pic_rects[1][0];
        // Page 2 starts at emitted row 7 (y 700 in grid space); row 8 is
        // 100 pt below that, page-relative y = MARGIN + 100.
        assert_eq!(rect.y, MARGIN + 100.0);
        assert_eq!(rect.x, MARGIN);
        assert_eq!(rect.height, 200.0, "rows 8..10 at 100 pt each");
        // The figure block sits on page 2, after the table.
        assert!(
            nx.page_blocks[1]
                .iter()
                .any(|b| matches!(b, Block::Figure { id, .. } if id == "s1_1")),
            "page 2 carries the figure ref: {:?}",
            nx.page_blocks[1]
        );
        assert!(
            !nx.page_blocks[0]
                .iter()
                .any(|b| matches!(b, Block::Figure { .. }))
        );
        // Doc-level: the figure ref follows the sheet's table.
        assert!(matches!(
            nx.all_blocks.last().unwrap(),
            Block::Figure { id, format } if id == "s1_1" && format == "png"
        ));
        // The image entry names the same id, the workbook-level page, and
        // the same rect.
        assert_eq!(nx.images.len(), 1);
        assert_eq!(nx.images[0].id, "s1_1");
        assert_eq!(nx.images[0].page, 2);
        assert_eq!(nx.images[0].bbox.y, rect.y);
        assert_eq!(nx.images[0].bytes.as_slice(), b"not-really-png-but-bytes");
    }

    /// `figures` without `images`: refs appear, no bytes are collected —
    /// the default-config (placeholder image mode) shape.
    #[test]
    fn figure_refs_do_not_require_byte_collection() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
        </sheetData><drawing r:id="rId7"/></worksheet>"#;
        let anchor = pic_anchor(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>"#,
        );
        let nx = layout_with_pic(
            sheet,
            &format!("{anchor}</xdr:oneCellAnchor>"),
            EmitOptions {
                figures: true,
                ..Default::default()
            },
        );
        assert!(nx.images.is_empty());
        assert_eq!(nx.pic_rects[0].len(), 1);
        // 914,400 EMU = 72 pt; 457,200 EMU = 36 pt.
        assert_eq!(nx.pic_rects[0][0].width, 72.0);
        assert_eq!(nx.pic_rects[0][0].height, 36.0);
        assert!(
            nx.page_blocks[0]
                .iter()
                .any(|b| matches!(b, Block::Figure { .. }))
        );
    }

    /// An image-only sheet — no written cells at all — still surfaces its
    /// picture, on its single empty page. This is the case `extract_images`
    /// exists for.
    #[test]
    fn an_image_only_sheet_still_extracts_its_picture() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><drawing r:id="rId7"/></worksheet>"#;
        let anchor = pic_anchor(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>10</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="914400"/>"#,
        );
        let nx = layout_with_pic(
            sheet,
            &format!("{anchor}</xdr:oneCellAnchor>"),
            EmitOptions {
                figures: true,
                images: true,
                ..Default::default()
            },
        );
        assert_eq!(nx.pages.len(), 1);
        assert!(nx.pages[0].text_items.is_empty());
        assert_eq!(nx.images.len(), 1);
        assert_eq!(nx.images[0].page, 1);
        // The empty grid collapses the anchor to the origin; the extent
        // survives and the rect stays inside the page box.
        let r = &nx.pic_rects[0][0];
        assert_eq!((r.x, r.y), (MARGIN, MARGIN));
        assert_eq!((r.width, r.height), (72.0, 72.0));
        assert!(
            nx.page_blocks[0]
                .iter()
                .any(|b| matches!(b, Block::Figure { .. }))
        );
    }

    /// The doc-level block stream from the geometry pass must equal the
    /// emitter's, figures included — the emitter is what the corpus gates
    /// score, and the CLI ships the geometry pass's.
    #[test]
    fn doc_blocks_match_the_emitter_with_figures_on() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
            <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
        </sheetData><drawing r:id="rId7"/></worksheet>"#;
        let anchor = pic_anchor(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>1</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>"#,
        );
        let parts = drawing_parts(&format!("{anchor}</xdr:oneCellAnchor>"));
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let wb = workbook_from(sheet, &extra);
        let opts = EmitOptions {
            figures: true,
            ..Default::default()
        };
        let emitted: Vec<Block> = crate::office::xlsx::emit_workbook(&wb, opts)
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        let nx = workbook_to_pages(&wb, opts);
        assert_eq!(
            format!("{emitted:?}"),
            format!("{:?}", nx.all_blocks),
            "geometry-pass doc blocks diverge from the emitter's"
        );
    }
}
