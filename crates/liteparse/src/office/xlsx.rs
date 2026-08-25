//! XLSX → [`Block`], reading the workbook grid directly instead of rendering
//! it through LibreOffice.
//!
//! The conversion path's measured failure is not layout, it is *destruction*:
//! LibreOffice clips every cell at its rendered column width before the PDF
//! exists, and emits **zero markdown table rows across 218 corpus workbooks**.
//! A workbook states its grid explicitly, so unlike DOCX (no geometry) and
//! PPTX (reading order), the emitter's job here is mostly to not ruin what the
//! file already says.
//!
//! Every rule below that looks arbitrary is a corpus measurement from
//! `xlsx_emit_census` (1,248 workbooks, 5,439 non-empty visible sheets):
//!
//! * **Merges are the common case, not the tail** — 68.9% of sheets carry at
//!   least one, so [`Block::MergedTable`] is the only table variant emitted
//!   and the plain-grid degeneration to a pipe table is the renderer's call.
//! * **Rows and columns are emitted sparse.** The bbox is a lie at the tail
//!   (a stray cell puts one sheet's bbox at 1,048,576 rows); rows the file
//!   does not write do not become table rows, and columns with no cell
//!   anywhere are compacted out. Nothing a cell says is dropped — only
//!   positions where no cell exists.
//! * **A leading full-width merge is a title, not a row** — 42.1% of sheets
//!   open with one. Emitting it as a table row buries the sheet's content
//!   title; it becomes a [`Block::Paragraph`] above the table.
//! * **Hidden sheets and hidden rows are emitted.** They hold 0.66% and
//!   0.09% of all cells; the text is real, the spreadsheet merely folds it
//!   from view, and an extraction pipeline that silently loses it has no way
//!   to say so. This diverges from what Excel prints on purpose.
//!
//! Cell text goes into [`Cell`] **unescaped**, the same contract as the DOCX
//! and PPTX table emitters: `render_blocks` escapes per dialect (`|` for pipe
//! tables, `&<>` for HTML) on the way out. Paragraph text (banners, one-column
//! sheets) is escaped here, before the hyperlink wraps it, so a URL's own
//! underscores survive.
//!
//! The grid is planned once per sheet ([`SheetPlan`]) and shared between the
//! doc-level emitter, the per-page block slices, and the geometry pass
//! (`xlsx_layout`), so the three can never disagree on which rows and columns
//! exist. Doc-level emission *is* a single full-range slice.

use std::collections::HashMap;
use std::ops::Range;

use liteparse_ooxml::model::{Inline, RunElement};
use liteparse_ooxml::pptx::TextParagraph;
use liteparse_ooxml::xlsx::{
    self, Cell as GridCell, CellValue, Row, Sheet, SheetPic, SheetShape, Workbook,
};

use crate::error::LiteParseError;
use crate::markdown_layout::{Block, Cell, apply_link, escape_inline};
use crate::office::inline::{Chunk, fmt_of, render_chunks};

/// Sheet names become `#` — the workbook's only structural rank, mirroring
/// slide titles on the PPTX path.
pub(crate) const SHEET_HEADING_LEVEL: u8 = 1;

/// A leading merge is a banner when it spans at least this fraction of the
/// sheet's retained width (and at least [`BANNER_MIN_COLS`] columns, so a
/// three-column sheet cannot promote every merged pair into a title).
const BANNER_MIN_WIDTH_FRACTION: f64 = 0.6;
const BANNER_MIN_COLS: usize = 3;

/// Frozen panes freeze title junk along with the header (the corpus histogram
/// runs to 19); a freeze deeper than this many *grid* rows stops being a
/// header declaration and the other signals decide instead.
const FROZEN_HEADER_MAX_ROWS: usize = 5;

/// What the emitter should produce beyond the always-on structure.
#[derive(Default, Clone, Copy)]
pub struct EmitOptions {
    /// Render external hyperlinks as `[text](url)` on the cell the link
    /// anchors to. Mirrors `LiteParseConfig::extract_links`.
    pub links: bool,
    /// Emit `Block::Figure` refs for the sheet's pictures. Mirrors the PDF,
    /// DOCX and PPTX paths' `image_mode != Off`.
    pub figures: bool,
    /// Collect picture bytes as `ExtractedImage`s. Read by the geometry pass
    /// (`xlsx_layout`), which is where pages — and so image ids' page
    /// numbers — exist; the block emitter ignores it.
    pub images: bool,
}

/// What one native-parsed workbook yields: the block stream, tagged with the
/// zero-based sheet index each block came from — the page mapping, once the
/// geometry pass gives sheets pages.
pub struct NativeWorkbook {
    pub blocks: Vec<(Block, usize)>,
    /// Tab-order sheet names, one per emitted sheet (hidden included).
    pub sheet_names: Vec<String>,
}

/// Parse an `.xlsx` and emit the shared block model.
///
/// Errors only when the container or the workbook part is unreadable —
/// everything below that degrades in the reader (a malformed sheet is skipped,
/// missing styles mean `General`), matching the fail-open contract the other
/// native paths converged on.
pub fn xlsx_to_blocks(data: &[u8], opts: EmitOptions) -> Result<Vec<Block>, LiteParseError> {
    Ok(emit_with_sources(data, opts)?
        .blocks
        .into_iter()
        .map(|(b, _)| b)
        .collect())
}

/// [`xlsx_to_blocks`], keeping each block's sheet index.
pub fn emit_with_sources(data: &[u8], opts: EmitOptions) -> Result<NativeWorkbook, LiteParseError> {
    let wb = xlsx::read(data)
        .map_err(|e| LiteParseError::Conversion(format!("xlsx parse failed: {e}")))?;
    Ok(emit_workbook(&wb, opts))
}

/// Emit an already-read workbook. Split from the bytes entry point so tests
/// and the geometry pass can share one parse.
pub fn emit_workbook(wb: &Workbook, opts: EmitOptions) -> NativeWorkbook {
    let mut blocks = Vec::new();
    let mut sheet_names = Vec::new();
    for (si, sheet) in wb.sheets.iter().enumerate() {
        sheet_names.push(sheet.name.clone());
        blocks.push((
            Block::Heading {
                level: SHEET_HEADING_LEVEL,
                text: escape_inline(&sheet.name),
            },
            si,
        ));
        let (shapes_above, shapes_below) = shape_blocks(sheet);
        for b in shapes_above {
            blocks.push((b, si));
        }
        if let Some(plan) = SheetPlan::build(wb, sheet, opts) {
            let n = plan.rows.len();
            for page in plan.page_blocks(wb, &[0..n]) {
                for b in page {
                    blocks.push((b, si));
                }
            }
        }
        for b in shapes_below {
            blocks.push((b, si));
        }
        if opts.figures {
            for b in figure_blocks(sheet, si) {
                blocks.push((b, si));
            }
        }
    }
    NativeWorkbook {
        blocks,
        sheet_names,
    }
}

/// A sheet's pictures in reading order — sorted by anchor cell, top-left
/// first, with the (single corpus) absolute anchor sorting after every cell
/// anchor. This order is the id order: the geometry pass numbers
/// `s{sheet}_{n}` walking the same list, so a `![](…)` ref and its
/// `ExtractedImage` cannot disagree.
pub(crate) fn ordered_pics(sheet: &Sheet) -> Vec<&SheetPic> {
    let mut pics: Vec<&SheetPic> = sheet.pics.iter().collect();
    pics.sort_by_key(|p| match p.anchor.from_cell() {
        Some(c) => (c.row as i64, c.col as i64, c.row_off_emu, c.col_off_emu),
        None => match p.anchor {
            xlsx::PicAnchor::Absolute { pos_emu, .. } => (i64::MAX, pos_emu.1, pos_emu.0, 0),
            _ => unreachable!("from_cell is None only for Absolute"),
        },
    });
    pics
}

/// The `Block::Figure` refs for a sheet's pictures, in [`ordered_pics`]
/// order. Emitted after the sheet's table: the pictures float *over* the
/// grid, so any interleaving with specific rows would be false precision —
/// and it keeps the doc emitter geometry-free.
///
/// Media the platform does not surface (EMF, SVG with no raster fallback)
/// takes no ref and no id, matching `FigureSink::place`.
pub(crate) fn figure_blocks(sheet: &Sheet, sheet_index: usize) -> Vec<Block> {
    let mut out = Vec::new();
    let mut n = 0u32;
    for pic in ordered_pics(sheet) {
        let Some(ext) = super::docx_layout::media_extension(pic.format) else {
            continue;
        };
        n += 1;
        out.push(Block::Figure {
            id: format!("s{}_{n}", sheet_index + 1),
            format: ext.to_string(),
        });
    }
    out
}

/// A sheet's text shapes in reading order — the same anchor sort as
/// [`ordered_pics`], and for the same reason: the geometry pass walks the
/// same list, so a shape's blocks and its placed items cannot disagree on
/// order.
pub(crate) fn ordered_shapes(sheet: &Sheet) -> Vec<&SheetShape> {
    let mut shapes: Vec<&SheetShape> = sheet.shapes.iter().collect();
    shapes.sort_by_key(|s| match s.anchor.from_cell() {
        Some(c) => (c.row as i64, c.col as i64, c.row_off_emu, c.col_off_emu),
        None => match s.anchor {
            xlsx::PicAnchor::Absolute { pos_emu, .. } => (i64::MAX, pos_emu.1, pos_emu.0, 0),
            _ => unreachable!("from_cell is None only for Absolute"),
        },
    });
    shapes
}

/// The paragraph blocks for a sheet's floating text shapes, split at the
/// grid: `(before the table, after the table)`.
///
/// The census (plan doc, floating text-shape entry) measured where shapes
/// anchor: 33% sit *above* the sheet's first written row, and the eye check
/// says those are titles and section navigation — emitting them after the
/// table would bury every title under its own data. Everything else follows
/// the figures precedent: shapes float *over* the grid, so interleaving with
/// specific rows would be false precision, and they emit after the table,
/// before the figure refs.
///
/// Census-scoped non-goals: bullets (7 shapes corpus-wide) emit as plain
/// paragraphs; `a:hlinkClick` (7) keeps its text and drops the link, which
/// would need the drawing part's rels threaded through the reader.
pub(crate) fn shape_blocks(sheet: &Sheet) -> (Vec<Block>, Vec<Block>) {
    let first_row = first_written_row(sheet);
    let mut above = Vec::new();
    let mut below = Vec::new();
    for shape in ordered_shapes(sheet) {
        let dst = if shape_is_above(shape, first_row) {
            &mut above
        } else {
            &mut below
        };
        dst.extend(shape_paragraphs(shape));
    }
    (above, below)
}

/// The index of the sheet's first written row with cells — the boundary the
/// above/below split keys on.
pub(crate) fn first_written_row(sheet: &Sheet) -> Option<u32> {
    sheet
        .rows
        .iter()
        .find(|r| !r.cells.is_empty())
        .map(|r| r.index)
}

/// Whether a shape anchors above the grid's first written row. A shape on a
/// sheet with no written cells is the sheet's only content and lands in the
/// "after" half, where an empty grid emits nothing before it.
pub(crate) fn shape_is_above(shape: &SheetShape, first_row: Option<u32>) -> bool {
    match (shape.anchor.from_cell(), first_row) {
        (Some(c), Some(first)) => c.row < first,
        _ => false,
    }
}

/// One shape's paragraph blocks, in body order. Shared by the doc emitter
/// and the geometry pass so a shape's blocks cannot differ between the two.
pub(crate) fn shape_paragraphs(shape: &SheetShape) -> Vec<Block> {
    let mut out = Vec::new();
    for para in &shape.body.paragraphs {
        let chunks = shape_chunks(para);
        let (text, bold, italic) = render_chunks(&chunks, true);
        if text.is_empty() {
            continue;
        }
        out.push(Block::Paragraph { text, bold, italic });
    }
    out
}

/// A shape paragraph's formatting-tagged chunks. Unlike the PPTX path there
/// is no cascade to resolve — a worksheet drawing has no master or layout —
/// so a run's explicit properties are the whole truth.
fn shape_chunks(para: &TextParagraph) -> Vec<Chunk> {
    fn walk(inlines: &[Inline], out: &mut Vec<Chunk>) {
        for inline in inlines {
            match inline {
                Inline::TextRun(run) => {
                    let mut text = String::new();
                    for el in &run.content {
                        match el {
                            RunElement::Text(t) => text.push_str(t),
                            RunElement::Tab => text.push('\t'),
                            // `a:br` inside one paragraph; `Block`'s
                            // single-line text cannot hold it (the PPTX
                            // path's rule, same reason).
                            RunElement::LineBreak(_) => text.push(' '),
                            _ => {}
                        }
                    }
                    if !text.is_empty() {
                        out.push(Chunk {
                            fmt: fmt_of(&run.properties),
                            link: None,
                            text,
                        });
                    }
                }
                Inline::Hyperlink(h) => walk(&h.content, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&para.content, &mut out);
    out
}

/// A merge's place in the emitted grid: spans counted over *emitted* rows and
/// *retained* columns, so a merge across skipped-empty rows does not claim
/// table rows that do not exist.
pub(crate) struct MergePlan {
    /// Anchor in sheet coordinates, re-anchored to the first emitted row /
    /// retained column inside the merge when the declared corner is empty.
    pub(crate) anchor: (u32, u32),
    pub(crate) colspan: u16,
    /// Clamped sheet-coordinate ranges, for coverage tests.
    pub(crate) col_range: (u32, u32),
    pub(crate) row_range: (u32, u32),
}

/// One sheet's emitted grid, planned once: which rows and columns exist, what
/// the merges claim, where the banners stop and how many rows are headers.
/// The markdown emitter, the per-page slicer and the geometry pass all read
/// this one structure.
pub(crate) struct SheetPlan<'a> {
    /// Rows the file wrote with at least one cell, in index order, each with
    /// its cells sorted by column (ascending is what every producer writes,
    /// but §18.3.1.4 does not require it). Banner rows included.
    pub(crate) rows: Vec<(&'a Row, Vec<&'a GridCell>)>,
    /// Retained columns: every column that holds at least one cell. Wholly
    /// empty columns inside the bbox carry no text and are compacted out —
    /// padding 16,384 pipes onto every row of a sheet whose stray formatting
    /// touched column XFD is the failure this prevents.
    pub(crate) cols: Vec<u32>,
    pub(crate) plans: Vec<MergePlan>,
    pub(crate) anchors: HashMap<(u32, u32), usize>,
    /// Emitted-row index → merges whose clamped range includes that row.
    cover: HashMap<u32, Vec<usize>>,
    /// Text folded into each merge (pass A): every covered cell's text, the
    /// anchor's own first. A *valued* cell under a neighbour's merge is a
    /// producer bug Excel never writes — folding keeps its text, because
    /// "absorbed" must never mean "silently gone".
    merge_text: Vec<Vec<String>>,
    /// Plan index of each leading banner row, in row order. `len()` is the
    /// number of leading rows emitted as paragraphs instead of table rows.
    banners: Vec<usize>,
    /// Header rows of the grid (the rows after the banners).
    header_rows: usize,
    links: HashMap<(u32, u32), &'a str>,
}

impl<'a> SheetPlan<'a> {
    /// `None` when the sheet has no written row with cells — the emitter
    /// produces only the sheet heading, and the geometry pass one empty page.
    pub(crate) fn build(wb: &Workbook, sheet: &'a Sheet, opts: EmitOptions) -> Option<Self> {
        let mut rows: Vec<(&Row, Vec<&GridCell>)> = sheet
            .rows
            .iter()
            .filter(|r| !r.cells.is_empty())
            .map(|r| {
                let mut cells: Vec<&GridCell> = r.cells.iter().collect();
                cells.sort_by_key(|c| c.at.col);
                (r, cells)
            })
            .collect();
        rows.sort_by_key(|(r, _)| r.index);
        if rows.is_empty() {
            return None;
        }

        let links: HashMap<(u32, u32), &str> = if opts.links {
            sheet
                .hyperlinks
                .iter()
                .filter_map(|h| {
                    h.url
                        .as_deref()
                        .map(|u| ((h.at.start.row, h.at.start.col), u))
                })
                .collect()
        } else {
            HashMap::new()
        };

        let mut cols: Vec<u32> = rows
            .iter()
            .flat_map(|(_, cells)| cells.iter().map(|c| c.at.col))
            .collect();
        cols.sort_unstable();
        cols.dedup();
        let width = cols.len();

        let mut plan = SheetPlan {
            rows,
            cols,
            plans: Vec::new(),
            anchors: HashMap::new(),
            cover: HashMap::new(),
            merge_text: Vec::new(),
            banners: Vec::new(),
            header_rows: 0,
            links,
        };

        // A one-column sheet is prose with an address, not a table. Merges
        // cannot move text between rows here — an absorbed cell is value-less
        // — so the grid machinery below is skipped rather than degenerated.
        if width == 1 {
            return Some(plan);
        }

        let row_indices: Vec<u32> = plan.rows.iter().map(|(r, _)| r.index).collect();
        let max_row = *row_indices.last().unwrap();
        let max_col = *plan.cols.last().unwrap();

        // Plan the merges against the emitted grid. First-wins on a shared
        // anchor: overlapping merges are invalid and Excel repairs them on
        // open.
        for m in &sheet.merges {
            let Some((p, _rowspan)) = plan_merge(m, &row_indices, &plan.cols, max_row, max_col)
            else {
                continue;
            };
            if plan.anchors.contains_key(&p.anchor) {
                continue;
            }
            let idx = plan.plans.len();
            plan.anchors.insert(p.anchor, idx);
            let lo = row_indices.partition_point(|&r| r < p.row_range.0);
            let hi = row_indices.partition_point(|&r| r <= p.row_range.1);
            for &ri in &row_indices[lo..hi] {
                plan.cover.entry(ri).or_default().push(idx);
            }
            plan.plans.push(p);
        }

        // Pass A: fold the text of every cell a merge covers into that merge.
        // The anchor's own cell lands first (its row precedes the others).
        plan.merge_text = vec![Vec::new(); plan.plans.len()];
        for (row, cells) in &plan.rows {
            if !plan.cover.contains_key(&row.index) {
                continue;
            }
            for cell in cells {
                if let Some(idx) = plan.covering_plan(row.index, cell.at.col) {
                    let text = plan.cell_text(wb, cell, false);
                    if !text.is_empty() {
                        plan.merge_text[idx].push(text);
                    }
                }
            }
        }

        // Banners: leading rows whose every valued cell sits inside one
        // shallow near-full-width merge anchored on that row. 42.1% of sheets
        // open with one; it reads as a title and is emitted as one.
        let min_banner_cols =
            BANNER_MIN_COLS.max((width as f64 * BANNER_MIN_WIDTH_FRACTION) as usize);
        for (row, cells) in &plan.rows {
            let Some(idx) = banner_plan(
                row,
                cells,
                &plan.plans,
                &plan.anchors,
                &row_indices,
                min_banner_cols,
            ) else {
                break;
            };
            plan.banners.push(idx);
        }

        plan.header_rows = header_rows(wb, sheet, &plan.rows[plan.banners.len()..]);
        Some(plan)
    }

    /// One retained column means paragraphs, never a one-pipe table.
    pub(crate) fn is_prose(&self) -> bool {
        self.cols.len() == 1
    }

    fn covering_plan(&self, row: u32, col: u32) -> Option<usize> {
        self.cover.get(&row)?.iter().copied().find(|&i| {
            let (lo, hi) = self.plans[i].col_range;
            col >= lo && col <= hi
        })
    }

    /// A cell's rendered text. `escape` is true for text bound for a
    /// paragraph, false for a table cell (the renderer escapes those per
    /// dialect). Escape happens *before* the link wraps the text so the URL
    /// half stays raw.
    pub(crate) fn cell_text(&self, wb: &Workbook, cell: &GridCell, escape: bool) -> String {
        // `_(* #,##0_)` pads for column alignment; markdown aligns itself, so
        // the padding is trimmed here and only here (the reader keeps it, per
        // the numfmt step's contract).
        let raw = wb.display_text(cell).unwrap_or_default();
        let raw = raw.trim();
        let text = if escape {
            escape_inline(raw)
        } else {
            raw.to_string()
        };
        match self.links.get(&(cell.at.row, cell.at.col)) {
            Some(url) => apply_link(if text.is_empty() { url } else { &text }, url),
            None => text,
        }
    }

    /// The external hyperlink anchored on this cell, if links are enabled.
    pub(crate) fn link_at(&self, row: u32, col: u32) -> Option<&str> {
        self.links.get(&(row, col)).copied()
    }

    fn merge_string(&self, idx: usize) -> String {
        self.merge_text[idx].join(" ")
    }

    /// The banner paragraph for banner row `i`, or `None` when its text is
    /// empty (the row is still consumed — it does not fall back into the
    /// table).
    fn banner_paragraph(&self, wb: &Workbook, i: usize) -> Option<Block> {
        let (_, cells) = &self.rows[i];
        let idx = self.banners[i];
        // A banner merge is one row tall, so its text is this row's cells —
        // re-rendered escaped, since it is bound for a paragraph.
        let (lo, hi) = self.plans[idx].col_range;
        let text = cells
            .iter()
            .filter(|c| c.at.col >= lo && c.at.col <= hi)
            .map(|c| self.cell_text(wb, c, true))
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            return None;
        }
        let font = cells
            .iter()
            .find(|c| (c.at.row, c.at.col) == self.plans[idx].anchor)
            .map(|c| wb.styles.font(c.style));
        Some(Block::Paragraph {
            text,
            bold: font.as_ref().is_some_and(|f| f.bold),
            italic: font.as_ref().is_some_and(|f| f.italic),
        })
    }

    /// A prose row's paragraph (one-column sheets), or `None` when empty.
    fn prose_paragraph(&self, wb: &Workbook, i: usize) -> Option<Block> {
        let (_, cells) = &self.rows[i];
        let cell = cells[0];
        let text = self.cell_text(wb, cell, true);
        if text.is_empty() {
            return None;
        }
        let font = wb.styles.font(cell.style);
        Some(Block::Paragraph {
            text,
            bold: font.bold,
            italic: font.italic,
        })
    }

    /// Assemble the table rows for a slice of the grid (indices into the rows
    /// *after* the banners), walking each row's sorted cells in step with the
    /// retained columns.
    ///
    /// Spans are counted within the slice: a merge cut by a page break keeps
    /// its text in the first fragment, and the continuation emits an empty
    /// spanning cell at the merge's column so the rows below it do not shift
    /// left. For the full grid range this reproduces doc-level emission.
    fn table_slice(&self, wb: &Workbook, slice: Range<usize>) -> Vec<Vec<Cell>> {
        let grid = &self.rows[self.banners.len()..];
        let rows = &grid[slice];
        let Some((first, _)) = rows.first() else {
            return Vec::new();
        };
        let slice_lo = first.index;
        // Rowspan in emitted units *within this slice* — never claim rows on
        // another page.
        let span_rows = |p: &MergePlan| -> u16 {
            rows.iter()
                .filter(|(r, _)| r.index >= p.row_range.0 && r.index <= p.row_range.1)
                .count()
                .min(u16::MAX as usize) as u16
        };

        let width = self.cols.len();
        let mut table: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
        for (row, cells) in rows {
            let mut cells_out: Vec<Cell> = Vec::with_capacity(width);
            let mut it = cells.iter().copied().peekable();
            for &col in &self.cols {
                // Duplicate cells at one address are malformed; first wins,
                // the rest are skipped by the cursor.
                let mut at_col: Option<&GridCell> = None;
                while let Some(&c) = it.peek() {
                    if c.at.col < col {
                        it.next();
                    } else {
                        if c.at.col == col {
                            at_col = Some(c);
                            it.next();
                        }
                        break;
                    }
                }
                let pos = (row.index, col);
                if let Some(&idx) = self.anchors.get(&pos) {
                    let plan = &self.plans[idx];
                    cells_out.push(Cell::spanning(
                        self.merge_string(idx),
                        plan.colspan,
                        span_rows(plan),
                    ));
                } else if let Some(idx) = self.covering_plan(pos.0, pos.1) {
                    let plan = &self.plans[idx];
                    if plan.anchor.0 < slice_lo && row.index == slice_lo && col == plan.anchor.1 {
                        // The rest of a merge whose anchor is on an earlier
                        // page. Its text already rendered there.
                        cells_out.push(Cell::spanning(
                            String::new(),
                            plan.colspan,
                            span_rows(plan),
                        ));
                    }
                    // Otherwise absorbed by a span; any text already folded
                    // into the anchor in pass A.
                } else {
                    let text = at_col
                        .map(|c| self.cell_text(wb, c, false))
                        .unwrap_or_default();
                    cells_out.push(Cell::new(text));
                }
            }
            table.push(cells_out);
        }
        table
    }

    /// The block stream for each range of emitted rows (banner rows counted).
    /// One range spanning all rows yields the doc-level emission; the
    /// geometry pass hands one range per page so per-page markdown holds the
    /// rows that are actually on that page. `header_rows` marks only the
    /// slice that contains the grid's first row.
    pub(crate) fn page_blocks(&self, wb: &Workbook, ranges: &[Range<usize>]) -> Vec<Vec<Block>> {
        let nb = self.banners.len();
        ranges
            .iter()
            .map(|range| {
                let mut out = Vec::new();
                if self.is_prose() {
                    for i in range.clone() {
                        if let Some(p) = self.prose_paragraph(wb, i) {
                            out.push(p);
                        }
                    }
                    return out;
                }
                for i in range.start..range.end.min(nb) {
                    if let Some(p) = self.banner_paragraph(wb, i) {
                        out.push(p);
                    }
                }
                let g0 = range.start.saturating_sub(nb);
                let g1 = range.end.saturating_sub(nb);
                if g1 > g0 {
                    let rows = self.table_slice(wb, g0..g1);
                    out.push(Block::MergedTable {
                        rows,
                        header_rows: if g0 == 0 { self.header_rows } else { 0 },
                    });
                }
                out
            })
            .collect()
    }
}

/// Clamp a merge to the emitted grid and count its spans in emitted units.
/// `None` means the merge touches no emitted row or retained column and
/// renders as nothing. The rowspan is returned for the census-facing tests;
/// emission recounts spans per slice.
fn plan_merge(
    m: &xlsx::RangeRef,
    row_indices: &[u32],
    cols: &[u32],
    max_row: u32,
    max_col: u32,
) -> Option<(MergePlan, u16)> {
    if m.start.row > max_row || m.start.col > max_col {
        return None;
    }
    let end_row = m.end.row.min(max_row);
    let end_col = m.end.col.min(max_col);
    let col_lo = cols.partition_point(|&c| c < m.start.col);
    let col_hi = cols.partition_point(|&c| c <= end_col);
    let row_lo = row_indices.partition_point(|&r| r < m.start.row);
    let row_hi = row_indices.partition_point(|&r| r <= end_row);
    if col_lo == col_hi || row_lo == row_hi {
        return None;
    }
    Some((
        MergePlan {
            // Re-anchoring moves the visual corner to the first position the
            // emitted grid still has, so a merge whose declared corner sits
            // on a skipped-empty row keeps claiming its cells.
            anchor: (row_indices[row_lo], cols[col_lo]),
            colspan: (col_hi - col_lo).min(u16::MAX as usize) as u16,
            col_range: (m.start.col, end_col),
            row_range: (m.start.row, end_row),
        },
        (row_hi - row_lo).min(u16::MAX as usize) as u16,
    ))
}

/// The banner test: some merge is anchored on this row, one emitted row tall
/// and at least `min_cols` retained columns wide, and every valued cell of
/// the row lies inside its column range.
fn banner_plan(
    row: &Row,
    cells: &[&GridCell],
    plans: &[MergePlan],
    anchors: &HashMap<(u32, u32), usize>,
    row_indices: &[u32],
    min_cols: usize,
) -> Option<usize> {
    let one_row_tall = |p: &MergePlan| {
        let lo = row_indices.partition_point(|&r| r < p.row_range.0);
        let hi = row_indices.partition_point(|&r| r <= p.row_range.1);
        hi - lo == 1
    };
    let idx = cells.iter().find_map(|c| {
        let &idx = anchors.get(&(row.index, c.at.col))?;
        let plan = &plans[idx];
        (one_row_tall(plan) && (plan.colspan as usize) >= min_cols).then_some(idx)
    })?;
    let (lo, hi) = plans[idx].col_range;
    cells
        .iter()
        .all(|c| c.at.col >= lo && c.at.col <= hi)
        .then_some(idx)
}

/// How many leading grid rows are headers.
///
/// The signals, in order of how explicitly the file states them (census
/// coverage in parens):
/// 1. Frozen panes covering 1–5 grid rows (18.8% of sheets freeze, but the
///    freeze includes banner junk — deeper freezes fall through).
/// 2. `<autoFilter>` whose range starts on the first grid row (5.0%).
/// 3. First grid row entirely bold with ≥2 cells (12.7%).
/// 4. Type transition: first grid row has no numbers, second does (14.3%).
fn header_rows(wb: &Workbook, sheet: &Sheet, grid: &[(&Row, Vec<&GridCell>)]) -> usize {
    if grid.is_empty() {
        return 0;
    }
    let frozen = grid
        .iter()
        .take_while(|(r, _)| r.index < sheet.frozen_rows)
        .count();
    if (1..=FROZEN_HEADER_MAX_ROWS).contains(&frozen) && frozen < grid.len() {
        return frozen;
    }
    let (first_row, first_cells) = &grid[0];
    if sheet
        .auto_filter
        .as_ref()
        .is_some_and(|af| af.start.row == first_row.index)
    {
        return 1;
    }
    if first_cells.len() >= 2 && first_cells.iter().all(|c| wb.styles.font(c.style).bold) {
        return 1;
    }
    let first_has_number = first_cells
        .iter()
        .any(|c| matches!(c.value, CellValue::Number(_)));
    let second_has_number = grid.get(1).is_some_and(|(_, cells)| {
        cells
            .iter()
            .any(|c| matches!(c.value, CellValue::Number(_)))
    });
    if !first_has_number && second_has_number {
        return 1;
    }
    0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write as _;

    /// Build a one-sheet workbook from a worksheet XML body (and optional
    /// extra parts), the same in-memory container trick as the reader's own
    /// tests — stating exactly which parts each rule is being tested against.
    pub(crate) fn workbook_from(sheet_xml: &str, extra: &[(&str, &str)]) -> Workbook {
        let mut parts: Vec<(&str, &str)> = vec![
            ("[Content_Types].xml", "<Types/>"),
            (
                "_rels/.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#,
            ),
            (
                "xl/workbook.xml",
                r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Data" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>
</Relationships>"#,
            ),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ];
        parts.extend_from_slice(extra);
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            for (name, body) in &parts {
                zip.start_file(*name, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        xlsx::read(&buf).unwrap()
    }

    fn blocks_of(sheet_xml: &str) -> Vec<Block> {
        let wb = workbook_from(sheet_xml, &[]);
        emit_workbook(&wb, EmitOptions::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect()
    }

    fn only_table(blocks: &[Block]) -> (&Vec<Vec<Cell>>, usize) {
        let tables: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                Block::MergedTable { rows, header_rows } => Some((rows, *header_rows)),
                _ => None,
            })
            .collect();
        assert_eq!(tables.len(), 1, "expected exactly one table: {blocks:?}");
        tables[0]
    }

    #[test]
    fn a_plain_grid_is_one_table_under_the_sheet_heading() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>a</t></is></c><c r="B1" t="inlineStr"><is><t>b</t></is></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c></row>
            </sheetData></worksheet>"#,
        );
        assert!(matches!(&blocks[0], Block::Heading { level: 1, text } if text == "Data"));
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Cell::new("a"));
        assert_eq!(rows[1][1], Cell::new("2"));
    }

    #[test]
    fn merges_become_spans_and_absorbed_cells_are_absent() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>wide</t></is></c><c r="C1" t="inlineStr"><is><t>x</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>tall</t></is></c><c r="B2"><v>1</v></c><c r="C2"><v>2</v></c></row>
                <row r="3"><c r="B3"><v>3</v></c><c r="C3"><v>4</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:B1"/><mergeCell ref="A2:A3"/></mergeCells></worksheet>"#,
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows[0], vec![Cell::spanning("wide", 2, 1), Cell::new("x")]);
        assert_eq!(
            rows[1],
            vec![Cell::spanning("tall", 1, 2), Cell::new("1"), Cell::new("2")]
        );
        // Row 3's A cell is absorbed by the rowspan: absent, not empty.
        assert_eq!(rows[2], vec![Cell::new("3"), Cell::new("4")]);
    }

    /// The census: 37.1% of merges anchor on a value-less cell. The span must
    /// still occupy its place, or every row below shifts left.
    #[test]
    fn a_valueless_anchor_still_claims_its_span() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="C1" t="inlineStr"><is><t>c</t></is></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c><c r="C2"><v>3</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows[0], vec![Cell::spanning("", 2, 1), Cell::new("c")]);
    }

    /// Rows the file never wrote are not table rows, and a merge spanning the
    /// gap counts only the rows that exist.
    #[test]
    fn empty_rows_are_skipped_and_rowspans_count_emitted_rows() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>span</t></is></c><c r="B1"><v>1</v></c></row>
                <row r="4"><c r="B4"><v>2</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:A4"/></mergeCells></worksheet>"#,
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows.len(), 2);
        // 4 sheet rows, but only 2 emitted: the rowspan is 2, not 4.
        assert_eq!(rows[0][0], Cell::spanning("span", 1, 2));
    }

    #[test]
    fn wholly_empty_columns_are_compacted_out() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1"><v>1</v></c><c r="XFD1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>3</v></c></row>
            </sheetData></worksheet>"#,
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows[0].len(), 2, "16,382 empty columns must not pad");
        assert_eq!(rows[1], vec![Cell::new("3"), Cell::new("")]);
    }

    /// The 42.1%: a leading near-full-width shallow merge is a title, emitted
    /// as a paragraph above the table rather than buried as a row.
    #[test]
    fn a_leading_full_width_merge_is_a_banner_paragraph() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>Quarterly Report</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>h1</t></is></c><c r="B2" t="inlineStr"><is><t>h2</t></is></c><c r="C2" t="inlineStr"><is><t>h3</t></is></c></row>
                <row r="3"><c r="A3"><v>1</v></c><c r="B3"><v>2</v></c><c r="C3"><v>3</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:C1"/></mergeCells></worksheet>"#,
        );
        assert!(
            matches!(&blocks[1], Block::Paragraph { text, .. } if text == "Quarterly Report"),
            "got {blocks:?}"
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows.len(), 2, "the banner row left the table");
    }

    /// A row that carries data outside its wide merge is a grid row, however
    /// wide the merge is.
    #[test]
    fn a_wide_merge_next_to_data_is_not_a_banner() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>wide</t></is></c><c r="E1"><v>9</v></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c><c r="C2"><v>3</v></c><c r="D2"><v>4</v></c><c r="E2"><v>5</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:D1"/></mergeCells></worksheet>"#,
        );
        assert!(!blocks.iter().any(|b| matches!(b, Block::Paragraph { .. })));
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows.len(), 2);
    }

    /// "Absorbed must never mean silently gone": a valued cell under a
    /// neighbour's merge is a producer bug, and its text folds into the
    /// anchor instead of vanishing.
    #[test]
    fn a_valued_cell_under_a_merge_folds_into_the_anchor() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>keep</t></is></c><c r="B1" t="inlineStr"><is><t>me</t></is></c><c r="C1"><v>7</v></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c><c r="C2"><v>3</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:B1"/></mergeCells></worksheet>"#,
        );
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows[0][0], Cell::spanning("keep me", 2, 1));
    }

    #[test]
    fn a_one_column_sheet_is_prose_not_a_table() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>Notes on methods</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>All values in mg/kg</t></is></c></row>
            </sheetData></worksheet>"#,
        );
        assert!(
            !blocks
                .iter()
                .any(|b| matches!(b, Block::MergedTable { .. }))
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|b| matches!(b, Block::Paragraph { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn frozen_rows_declare_the_header() {
        let blocks = blocks_of(
            r#"<worksheet>
            <sheetViews><sheetView><pane ySplit="1" state="frozen"/></sheetView></sheetViews>
            <sheetData>
                <row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        let (_, header_rows) = only_table(&blocks);
        assert_eq!(header_rows, 1);
    }

    /// The corpus freezes run to 19 rows — a freeze that deep is a viewport
    /// choice, not a header declaration, and the other signals decide.
    #[test]
    fn a_deep_freeze_is_not_a_header() {
        let rows_xml: String = (1..=10)
            .map(|r| {
                format!(
                    r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#
                )
            })
            .collect();
        let blocks = blocks_of(&format!(
            r#"<worksheet>
            <sheetViews><sheetView><pane ySplit="8" state="frozen"/></sheetView></sheetViews>
            <sheetData>{rows_xml}</sheetData></worksheet>"#
        ));
        let (_, header_rows) = only_table(&blocks);
        assert_eq!(header_rows, 0);
    }

    #[test]
    fn a_text_over_numbers_transition_declares_a_header() {
        let blocks = blocks_of(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="inlineStr"><is><t>score</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>ann</t></is></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        let (_, header_rows) = only_table(&blocks);
        assert_eq!(header_rows, 1);
    }

    #[test]
    fn hidden_sheets_and_hidden_rows_still_emit() {
        let sheet = r#"<worksheet><sheetData>
            <row r="1" hidden="1"><c r="A1" t="inlineStr"><is><t>secret</t></is></c><c r="B1"><v>1</v></c></row>
            <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>3</v></c></row>
        </sheetData></worksheet>"#;
        let wb = {
            let mut wb = workbook_from(sheet, &[]);
            wb.sheets[0].visible = false;
            wb
        };
        let blocks: Vec<Block> = emit_workbook(&wb, EmitOptions::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Cell::new("secret"));
    }

    #[test]
    fn number_formats_reach_the_emitted_text() {
        let styles = r#"<styleSheet>
  <numFmts><numFmt numFmtId="164" formatCode="0.0%"/></numFmts>
  <cellXfs count="2"><xf numFmtId="0" fontId="0"/><xf numFmtId="164" fontId="0"/></cellXfs>
</styleSheet>"#;
        let wb = workbook_from(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" s="1"><v>0.155</v></c><c r="B1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>1</v></c><c r="B2"><v>2</v></c></row>
            </sheetData></worksheet>"#,
            &[("xl/styles.xml", styles)],
        );
        let blocks: Vec<Block> = emit_workbook(&wb, EmitOptions::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        let (rows, _) = only_table(&blocks);
        assert_eq!(rows[0][0], Cell::new("15.5%"));
    }

    #[test]
    fn hyperlinks_wrap_the_anchor_cell_when_asked() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>site</t></is></c><c r="B1"><v>1</v></c></row>
            <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>3</v></c></row>
        </sheetData><hyperlinks><hyperlink ref="A1" r:id="rId9"/></hyperlinks></worksheet>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com" TargetMode="External"/>
</Relationships>"#;
        let wb = workbook_from(sheet, &[("xl/worksheets/_rels/sheet1.xml.rels", rels)]);
        let with = emit_workbook(
            &wb,
            EmitOptions {
                links: true,
                ..Default::default()
            },
        );
        let without = emit_workbook(&wb, EmitOptions::default());
        let cell = |nw: &NativeWorkbook| match &nw.blocks[1].0 {
            Block::MergedTable { rows, .. } => rows[0][0].text.clone(),
            b => panic!("expected table, got {b:?}"),
        };
        assert_eq!(cell(&with), "[site](https://example.com)");
        assert_eq!(cell(&without), "site");
    }

    #[test]
    fn an_empty_sheet_is_a_heading_and_nothing_else() {
        let blocks = blocks_of("<worksheet><sheetData/></worksheet>");
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Heading { .. }));
    }

    /// Page slicing: a rowspan cut by a page break keeps its text on the
    /// first page, and the continuation emits an empty spanning cell so the
    /// columns below do not shift left.
    #[test]
    fn a_page_break_splits_a_rowspan_without_shifting_columns() {
        let wb = workbook_from(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>tall</t></is></c><c r="B1"><v>1</v></c></row>
                <row r="2"><c r="B2"><v>2</v></c></row>
                <row r="3"><c r="B3"><v>3</v></c></row>
                <row r="4"><c r="A4" t="inlineStr"><is><t>d</t></is></c><c r="B4"><v>4</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:A3"/></mergeCells></worksheet>"#,
            &[],
        );
        let plan = SheetPlan::build(&wb, &wb.sheets[0], EmitOptions::default()).unwrap();
        let pages = plan.page_blocks(&wb, &[0..2, 2..4]);
        let table = |page: &[Block]| match &page[0] {
            Block::MergedTable { rows, .. } => rows.clone(),
            b => panic!("expected table, got {b:?}"),
        };
        let first = table(&pages[0]);
        assert_eq!(first[0][0], Cell::spanning("tall", 1, 2), "clamped to page");
        let second = table(&pages[1]);
        // Continuation: empty spanning cell holds the merge's column.
        assert_eq!(second[0], vec![Cell::spanning("", 1, 1), Cell::new("3")]);
        assert_eq!(second[1], vec![Cell::new("d"), Cell::new("4")]);
    }

    /// The full-range slice must reproduce doc-level emission exactly — the
    /// doc path *is* this call.
    #[test]
    fn one_full_range_reproduces_the_doc_table() {
        let wb = workbook_from(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>t</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>h1</t></is></c><c r="B2" t="inlineStr"><is><t>h2</t></is></c><c r="C2" t="inlineStr"><is><t>h3</t></is></c></row>
                <row r="3"><c r="A3"><v>1</v></c><c r="B3"><v>2</v></c><c r="C3"><v>3</v></c></row>
            </sheetData><mergeCells><mergeCell ref="A1:C1"/></mergeCells></worksheet>"#,
            &[],
        );
        let doc: Vec<Block> = emit_workbook(&wb, EmitOptions::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        let plan = SheetPlan::build(&wb, &wb.sheets[0], EmitOptions::default()).unwrap();
        let pages = plan.page_blocks(&wb, &[0..plan.rows.len()]);
        assert_eq!(
            format!("{:?}", &doc[1..]),
            format!("{:?}", &pages[0][..]),
            "doc emission is the full slice"
        );
    }

    /// Header rows mark only the slice containing the grid's first row —
    /// a continuation page's table has no header.
    // ── floating text shapes ────────────────────────────────────────────────

    /// Parts hanging one text shape off sheet1 via a drawing part. Shapes
    /// resolve nothing, so no drawing rels and no media are needed.
    fn shape_parts(anchor_xml: &str) -> Vec<(String, String)> {
        let sheet_rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing1.xml"/>
</Relationships>"#;
        let drawing = format!(
            r#"<xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">{anchor_xml}</xdr:wsDr>"#
        );
        vec![
            (
                "xl/worksheets/_rels/sheet1.xml.rels".to_string(),
                sheet_rels.to_string(),
            ),
            ("xl/drawings/drawing1.xml".to_string(), drawing),
        ]
    }

    fn shape_anchor(from_row: u32, paras: &str) -> String {
        format!(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>{from_row}</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>
                 <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="TextBox 1"/><xdr:cNvSpPr txBox="1"/></xdr:nvSpPr>
                   <xdr:spPr/><xdr:txBody><a:bodyPr/>{paras}</xdr:txBody></xdr:sp>
               <xdr:clientData/></xdr:oneCellAnchor>"#
        )
    }

    fn shape_blocks_of(sheet_xml: &str, anchor_xml: &str) -> Vec<Block> {
        let parts = shape_parts(anchor_xml);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let wb = workbook_from(sheet_xml, &extra);
        emit_workbook(&wb, EmitOptions::default())
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect()
    }

    const TWO_ROW_GRID: &str = r#"<worksheet><sheetData>
        <row r="3"><c r="A3" t="inlineStr"><is><t>a</t></is></c><c r="B3"><v>1</v></c></row>
        <row r="4"><c r="A4" t="inlineStr"><is><t>b</t></is></c><c r="B4"><v>2</v></c></row>
    </sheetData><drawing r:id="rId7"/></worksheet>"#;

    /// The census's placement rule: a shape anchored above the first written
    /// row is a title and must not be buried under its own table.
    #[test]
    fn a_shape_above_the_grid_emits_before_the_table() {
        let blocks = shape_blocks_of(
            TWO_ROW_GRID,
            &shape_anchor(0, r#"<a:p><a:r><a:t>Quarterly Summary</a:t></a:r></a:p>"#),
        );
        assert!(matches!(&blocks[0], Block::Heading { .. }));
        assert!(
            matches!(&blocks[1], Block::Paragraph { text, .. } if text == "Quarterly Summary"),
            "title shape should precede the table, got {blocks:?}"
        );
        assert!(matches!(&blocks[2], Block::MergedTable { .. }));
    }

    /// Everything else follows the figures precedent: after the table.
    #[test]
    fn a_shape_over_the_grid_emits_after_the_table() {
        let blocks = shape_blocks_of(
            TWO_ROW_GRID,
            &shape_anchor(3, r#"<a:p><a:r><a:t>see note</a:t></a:r></a:p>"#),
        );
        assert!(matches!(&blocks[1], Block::MergedTable { .. }));
        assert!(
            matches!(&blocks[2], Block::Paragraph { text, .. } if text == "see note"),
            "annotation shape should follow the table, got {blocks:?}"
        );
    }

    /// 54 corpus shapes sit on sheets with no written cells — today those
    /// sheets emit nothing at all; the shape is the sheet's only content.
    #[test]
    fn a_shape_on_an_empty_sheet_is_its_only_content() {
        let blocks = shape_blocks_of(
            r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><drawing r:id="rId7"/></worksheet>"#,
            &shape_anchor(2, r#"<a:p><a:r><a:t>orphan text</a:t></a:r></a:p>"#),
        );
        assert!(matches!(&blocks[0], Block::Heading { .. }));
        assert!(
            matches!(&blocks[1], Block::Paragraph { text, .. } if text == "orphan text"),
            "got {blocks:?}"
        );
        assert_eq!(blocks.len(), 2);
    }

    /// Run emphasis reaches the block the same way the PPTX path's does, and
    /// each paragraph is its own block.
    #[test]
    fn shape_paragraphs_carry_emphasis_and_split_per_paragraph() {
        let blocks = shape_blocks_of(
            TWO_ROW_GRID,
            &shape_anchor(
                3,
                r#"<a:p><a:r><a:rPr b="1"/><a:t>NOTES:</a:t></a:r></a:p><a:p><a:r><a:t>fill in blue cells</a:t></a:r></a:p>"#,
            ),
        );
        assert!(
            matches!(&blocks[2], Block::Paragraph { text, bold: true, .. } if text == "NOTES:"),
            "got {blocks:?}"
        );
        assert!(
            matches!(&blocks[3], Block::Paragraph { text, bold: false, .. } if text == "fill in blue cells")
        );
    }

    #[test]
    fn header_rows_do_not_repeat_on_continuation_pages() {
        let wb = workbook_from(
            r#"<worksheet><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="inlineStr"><is><t>score</t></is></c></row>
                <row r="2"><c r="A2" t="inlineStr"><is><t>ann</t></is></c><c r="B2"><v>4</v></c></row>
                <row r="3"><c r="A3" t="inlineStr"><is><t>bob</t></is></c><c r="B3"><v>5</v></c></row>
            </sheetData></worksheet>"#,
            &[],
        );
        let plan = SheetPlan::build(&wb, &wb.sheets[0], EmitOptions::default()).unwrap();
        let pages = plan.page_blocks(&wb, &[0..2, 2..3]);
        let header = |page: &[Block]| match &page[0] {
            Block::MergedTable { header_rows, .. } => *header_rows,
            b => panic!("expected table, got {b:?}"),
        };
        assert_eq!(header(&pages[0]), 1);
        assert_eq!(header(&pages[1]), 0);
    }
}
