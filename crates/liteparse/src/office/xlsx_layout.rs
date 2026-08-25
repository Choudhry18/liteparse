//! XLSX geometry: the planned grid → [`Page`]s with per-cell [`TextItem`]s.
//!
//! Unlike DOCX (a layout engine) and PPTX (a cascade), XLSX geometry is
//! *stated*: the file declares every column width and row height, so this
//! pass is unit conversion plus pagination — no fonts, no measurement, no
//! host dependence. Every constant traces to `xlsx_geometry_census` over the
//! 1,248-workbook corpus.
//!
//! The **paint** built on top of it does measure, and that is a deliberate
//! divergence between the two consumers of one read rather than a crack in
//! the claim above: a raster has to decide where a glyph lands and where a
//! string stops, and a `TextItem` does not. Nothing the measurer returns
//! reaches a [`Page`] — the items are byte-identical with and without a font
//! registry, and a test says so. Constants:
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
use std::rc::Rc;

use liteparse_ooxml::render::dimension::Pt;
use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::geometry::{PtOffset, PtRect, PtSize};
use liteparse_ooxml::render::layout::draw_command::{DrawCommand, LayoutedPage};
use liteparse_ooxml::render::layout::fragment::FontProps;
use liteparse_ooxml::render::layout::measurer::TextMeasurer;
use liteparse_ooxml::render::resolve::color::RgbColor;
use liteparse_ooxml::render::resolve::images::MediaEntry;
use liteparse_ooxml::xlsx::{
    Alignment, Border, BorderEdge, CellAnchor, CellValue, HorizontalAlign, PatternType, PicAnchor,
    Row, Sheet, SheetShape, VerticalAlign, Workbook,
};

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

/// Excel's gridline grey, and one device pixel of it at 96 DPI. Gridlines are
/// not decoration: 12.1% of corpus sheets declare neither a fill nor a border
/// anywhere, so this is the only ink holding their numbers in a grid.
const GRIDLINE_COLOR: RgbColor = RgbColor {
    r: 0xD0,
    g: 0xD7,
    b: 0xE5,
};
const GRIDLINE_W: f64 = 0.75;

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
    /// The paint of each page, aligned with `pages`: gridlines, cell fills
    /// and cell borders as [`DrawCommand::Rect`]s, then the cell text as
    /// [`DrawCommand::Text`]s, then each picture on the page as a
    /// [`DrawCommand::Image`] — ready for `render::raster::rasterize_page`.
    /// Empty unless `EmitOptions::paint`; textless unless a font registry was
    /// supplied too.
    ///
    /// Command order carries no z-order here: the rasterizer paints in fixed
    /// passes (`Path` → `Rect` → `Image` → ink), so a picture always covers
    /// the grid beneath it and cell text always covers the picture. Excel
    /// floats a picture above cell text, so that last relation is inverted —
    /// see the overlap census for how often it can be seen.
    pub layouts: Vec<LayoutedPage>,
    /// What the text pass met, summed over the workbook. Zero when nothing
    /// was painted; read by the corpus gate, not by `parser.rs`.
    pub text_stats: TextPaintStats,
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
///
/// `fonts` is the registry the *text* paint measures with, and is needed only
/// under `EmitOptions::paint`: a parse wants `TextItem`s, whose boxes are the
/// declared grid and so cost no measurement at all — that is this pass's whole
/// host-independence claim, and it survives here because nothing the measurer
/// returns reaches a `Page`. Painting with `None` draws the grid and no text.
pub fn workbook_to_pages(
    wb: &Workbook,
    opts: EmitOptions,
    fonts: Option<&FontRegistry>,
) -> NativeXlsx {
    let measurer = fonts.map(TextMeasurer::new);
    let mut out = NativeXlsx {
        pages: Vec::new(),
        page_blocks: Vec::new(),
        all_blocks: Vec::new(),
        outline: Vec::new(),
        images: Vec::new(),
        pic_rects: Vec::new(),
        layouts: Vec::new(),
        text_stats: TextPaintStats::default(),
    };
    // One sink across the workbook: a logo placed on every sheet dedups to
    // one canonical entry, the same cross-page rule the PPTX path applies
    // across slides.
    let mut sink = FigureSink::default();
    // Workbook-scoped so every placement of one media part shares one `Arc`,
    // which is the identity the painter's bitmap cache keys on. Populated
    // only under `EmitOptions::paint`.
    let mut media: std::collections::HashMap<&str, MediaEntry> = std::collections::HashMap::new();

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

        // Doc-level: title-like shapes above the grid, the unsplit emission
        // (one full-range slice), the remaining shapes, then the sheet's
        // figures — the same order `emit_workbook` writes, so the two doc
        // emissions stay byte-identical.
        let (shapes_above, shapes_below) = super::xlsx::shape_blocks(sheet);
        out.all_blocks.extend(shapes_above);
        if let Some(p) = &plan {
            let n = p.rows.len();
            for page in p.page_blocks(wb, &[0..n]) {
                out.all_blocks.extend(page);
            }
        }
        out.all_blocks.extend(shapes_below);
        if opts.figures {
            out.all_blocks.extend(super::xlsx::figure_blocks(sheet, si));
        }

        let mut blocks_per_page: Vec<Vec<Block>> = match &plan {
            Some(p) => p.page_blocks(wb, &ranges),
            None => vec![Vec::new()],
        };
        blocks_per_page[0].insert(0, heading);

        // Text shapes: page assignment + rect through the same anchor
        // placement as pictures, blocks mirroring the doc order per page —
        // above-grid shapes right after the heading, the rest after the
        // page's table slice (and before its figure refs, pushed below).
        // Items follow the pass's own philosophy — unit conversion, not
        // layout: one item per paragraph, stacked evenly in the shape's box.
        let grid_w = *geo.x_off.last().unwrap() as f32;
        let page_width = (grid_w + 2.0 * MARGIN).max(MIN_PAGE_WIDTH);
        let canvas = CanvasGrid::build(sheet);
        let first_row = super::xlsx::first_written_row(sheet);
        let mut shape_items_per_page: Vec<Vec<TextItem>> = vec![Vec::new(); ranges.len()];
        let mut above_blocks: Vec<Block> = Vec::new();
        for shape in super::xlsx::ordered_shapes(sheet) {
            let (page_local, rect) = geo.place_pic(
                cols,
                &row_indices,
                &ranges,
                page_width,
                &canvas,
                &shape.anchor,
            );
            shape_text_items(shape, &rect, &mut shape_items_per_page[page_local]);
            let blocks = super::xlsx::shape_paragraphs(shape);
            if super::xlsx::shape_is_above(shape, first_row) {
                above_blocks.extend(blocks);
            } else {
                blocks_per_page[page_local].extend(blocks);
            }
        }
        blocks_per_page[0].splice(1..1, above_blocks);

        // Pictures: page assignment + rect from the packed grid, figure
        // blocks on the page the anchor lands on, bytes into the sink. The
        // local ordinal mirrors `figure_blocks` (both skip media we do not
        // surface), and the sink's id must agree with it by construction.
        let mut rects_per_page: Vec<Vec<Rect>> = vec![Vec::new(); ranges.len()];
        let mut pic_cmds_per_page: Vec<Vec<DrawCommand>> = vec![Vec::new(); ranges.len()];
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
            if opts.paint {
                // One `MediaEntry` per media part for the whole workbook, not
                // per placement: the painter's bitmap cache keys on the `Arc`
                // pointer, so a logo anchored on every sheet decodes once.
                // The copy out of the reader's `Arc<Vec<u8>>` is paid here
                // rather than at read time so a parse — which never paints —
                // keeps costing zero byte copies.
                let entry = media
                    .entry(pic.media_path.as_str())
                    .or_insert_with(|| MediaEntry {
                        data: std::sync::Arc::from(pic.bytes.as_slice()),
                        format: pic.format,
                    })
                    .clone();
                pic_cmds_per_page[page_local].push(DrawCommand::Image {
                    rect: PtRect::from_xywh(
                        Pt::new(rect.x),
                        Pt::new(rect.y),
                        Pt::new(rect.width),
                        Pt::new(rect.height),
                    ),
                    image_data: entry,
                    // `a:srcRect` is not read by the XLSX drawing parser, so
                    // a cropped picture paints uncropped. Recorded, not
                    // guessed: the crop math already exists for the DOCX and
                    // PPTX paths and can be threaded through when the anchor
                    // parser grows the element.
                    src_rect: None,
                    // Excel floats a picture above the grid *and* its values,
                    // so this is the one producer that opts out of the
                    // painter's flow order — without it 29.2% of corpus
                    // placements have cell text drawn through them.
                    float: true,
                });
            }
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

        // The `<col style>` of each retained column, resolved once per sheet:
        // the painter asks for it on every row of every page.
        let col_styles: Vec<Option<u32>> = cols.iter().map(|&c| sheet.col_style(c)).collect();

        for (i, blocks) in blocks_per_page.into_iter().enumerate() {
            let mut page = match &plan {
                Some(p) => geo.build_page(wb, p, ranges[i].clone(), out.pages.len() + 1),
                None => empty_page(out.pages.len() + 1),
            };
            append_items(&mut page, std::mem::take(&mut shape_items_per_page[i]));
            let mut layout = LayoutedPage::new(PtSize::new(
                Pt::new(page.page_width),
                Pt::new(page.page_height),
            ));
            if opts.paint {
                if let Some(p) = &plan {
                    let (cmds, stats) = geo.paint_page(
                        wb,
                        sheet,
                        p,
                        &col_styles,
                        ranges[i].clone(),
                        measurer.as_ref(),
                    );
                    layout.commands = cmds;
                    out.text_stats.add(stats);
                }
                // Outside the `plan` arm on purpose: a sheet whose only
                // content is a picture has no grid to paint and still has
                // something to draw.
                layout.commands.append(&mut pic_cmds_per_page[i]);
            }
            out.layouts.push(layout);
            out.pages.push(page);
            out.page_blocks.push(blocks);
            out.pic_rects.push(std::mem::take(&mut rects_per_page[i]));
        }
    }
    out.images = sink.images;
    out
}

/// One `TextItem` per non-empty shape paragraph, stacked evenly inside the
/// shape's box — the same fidelity contract as a cell's item (the box is
/// real, the intra-box line layout is not attempted). Font facts come from
/// the paragraph's first run: 99.5% of corpus shape runs declare `sz`, and a
/// theme-referenced face (no explicit name) stays `None` like an unstyled
/// cell would.
fn shape_text_items(shape: &SheetShape, rect: &Rect, out: &mut Vec<TextItem>) {
    let paras: Vec<&liteparse_ooxml::pptx::TextParagraph> = shape
        .body
        .paragraphs
        .iter()
        .filter(|p| !p.text().trim().is_empty())
        .collect();
    if paras.is_empty() {
        return;
    }
    let each_h = rect.height / paras.len() as f32;
    for (i, para) in paras.iter().enumerate() {
        fn first_run(
            inlines: &[liteparse_ooxml::model::Inline],
        ) -> Option<&liteparse_ooxml::model::TextRun> {
            inlines.iter().find_map(|inl| match inl {
                liteparse_ooxml::model::Inline::TextRun(r) => Some(&**r),
                liteparse_ooxml::model::Inline::Hyperlink(h) => first_run(&h.content),
                _ => None,
            })
        }
        let props = first_run(&para.content).map(|r| &r.properties);
        out.push(TextItem {
            text: para.text().trim().replace('\n', " "),
            x: rect.x,
            y: rect.y + i as f32 * each_h,
            width: rect.width,
            height: each_h,
            font_name: props.and_then(|p| p.fonts.ascii.explicit.clone()),
            font_size: props
                .and_then(|p| p.font_size)
                .map(|s| s.raw() as f32 / 2.0),
            font_weight: props.and_then(|p| p.bold).unwrap_or(false).then_some(700),
            ..Default::default()
        });
    }
}

/// Extend a built page with extra items, keeping `content_bounds` the union
/// it was defined as.
fn append_items(page: &mut Page, items: Vec<TextItem>) {
    if items.is_empty() {
        return;
    }
    page.text_items.extend(items);
    page.content_bounds = page
        .text_items
        .iter()
        .map(|t| (t.x, t.y, t.x + t.width, t.y + t.height))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
        .map(|(x0, y0, x1, y1)| Rect {
            x: x0,
            y: y0,
            width: x1 - x0,
            height: y1 - y0,
        });
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

/// A page-space rectangle command. Coordinates arrive grid-relative and leave
/// page-absolute, which is the one place [`MARGIN`] is added to paint.
fn rect_cmd(x: f64, y: f64, w: f64, h: f64, color: RgbColor) -> DrawCommand {
    DrawCommand::Rect {
        rect: PtRect::from_xywh(
            Pt::new(MARGIN + x as f32),
            Pt::new(MARGIN + y as f32),
            Pt::new(w as f32),
            Pt::new(h as f32),
        ),
        color,
    }
}

fn rgb_of([r, g, b]: [u8; 3]) -> RgbColor {
    RgbColor { r, g, b }
}

/// The effective style of every retained column of one row, written into
/// `out` (reused across rows: a wide sheet would otherwise allocate one vector
/// per row per page).
///
/// The cascade is Excel's: the cell's own `s=` wins, then the row's
/// `customFormat` style, then the `<col style>` of the span it falls in. A
/// cell that states no `s=` inherits rather than resolving to style 0 — that
/// is the whole reason the row and column carriers exist.
///
/// Value-less styled cells come from the paint side-channel and are treated
/// exactly like a cell's own `s=`; the ones on columns the packed grid
/// compacted out are dropped here (class C's column half).
fn row_paint_styles(
    row: &Row,
    cells: &[&liteparse_ooxml::xlsx::Cell],
    cols: &[u32],
    col_styles: &[Option<u32>],
    out: &mut Vec<Option<u32>>,
) {
    out.clear();
    if row.style.is_some() {
        out.resize(cols.len(), row.style);
    } else {
        out.extend_from_slice(col_styles);
    }
    let mut set = |col: u32, style: u32| {
        if let Ok(ci) = cols.binary_search(&col) {
            out[ci] = Some(style);
        }
    };
    for cell in cells {
        if let Some(s) = cell.style {
            set(cell.at.col, s);
        }
    }
    for blank in &row.styled_blanks {
        set(blank.col, blank.style);
    }
}

/// One cell's four edges as thin rects, inside the cell box. Vertical edges
/// are inset by the horizontal ones so a corner is painted once, the same
/// rule the DOCX table painter uses.
///
/// An edge that names no colour is black: SpreadsheetML's automatic border
/// colour is the window text colour, unlike an automatic *fill*, which is the
/// background and must not be painted at all.
fn push_border(
    out: &mut Vec<DrawCommand>,
    wb: &Workbook,
    border: &Border,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) {
    let color = |e: &BorderEdge| {
        e.color
            .and_then(|c| wb.resolve_color(c))
            .map_or(RgbColor::BLACK, rgb_of)
    };
    let width = |e: &BorderEdge| {
        f64::from(e.style.width_pt())
            .min(h.max(0.0))
            .min(w.max(0.0))
    };
    let (top_w, bot_w) = (width(&border.top), width(&border.bottom));
    if border.top.style.paints() {
        out.push(rect_cmd(x, y, w, top_w, color(&border.top)));
    }
    if border.bottom.style.paints() {
        out.push(rect_cmd(x, y + h - bot_w, w, bot_w, color(&border.bottom)));
    }
    let top_inset = if border.top.style.paints() {
        top_w
    } else {
        0.0
    };
    let bot_inset = if border.bottom.style.paints() {
        bot_w
    } else {
        0.0
    };
    let v_h = h - top_inset - bot_inset;
    if v_h <= 0.0 {
        return;
    }
    if border.left.style.paints() {
        let lw = width(&border.left);
        out.push(rect_cmd(x, y + top_inset, lw, v_h, color(&border.left)));
    }
    if border.right.style.paints() {
        let rw = width(&border.right);
        out.push(rect_cmd(
            x + w - rw,
            y + top_inset,
            rw,
            v_h,
            color(&border.right),
        ));
    }
}

// ── cell text ───────────────────────────────────────────────────────────────

/// Excel's own defaults when the styles part names neither.
const DEFAULT_FONT_FAMILY: &str = "Calibri";
const DEFAULT_FONT_SIZE: f32 = 11.0;
/// §18.8.1: one `indent` step is three characters' worth of the cell's font.
const INDENT_STEP: &str = "   ";

/// What the text pass did with the overflow it met, per the four rules Excel
/// applies — the counters the corpus gate reads back against the paint
/// census's independent predictions (12.8% clipped, 6.1% spilled, 2.0%
/// hashed, 6.9% wrapped).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextPaintStats {
    /// Cells that painted at least one line.
    pub cells: u64,
    /// Cells with text that painted nothing: the fit left no character at
    /// all, which is a hidden (zero-width) column or a number in a column too
    /// narrow for one `#`. Named rather than silent — `cells + blank` is what
    /// the recall oracle must equal.
    pub blank: u64,
    /// Cells whose style declares `wrapText`. The census's denominator.
    pub wrap_declared: u64,
    /// Cells whose text actually broke over more than one line — a declared
    /// wrap on a string that fits does not.
    pub wrapped: u64,
    /// Cells that ran into at least one empty neighbour.
    pub spilled: u64,
    /// Overflowing cells whose *declared* right neighbour is unwritten but
    /// whose *packed* one is not: the spill Excel would do and this pass
    /// cannot, because the empty column between them is compacted out of the
    /// page. The size of the one divergence between this pass's grid and
    /// Excel's that the text rules can see.
    pub spill_lost_to_packing: u64,
    /// Cells whose text was cut because the room ran out.
    pub clipped: u64,
    /// Numeric cells replaced by `#######`.
    pub hashed: u64,
}

impl TextPaintStats {
    fn add(&mut self, o: TextPaintStats) {
        self.cells += o.cells;
        self.blank += o.blank;
        self.wrap_declared += o.wrap_declared;
        self.wrapped += o.wrapped;
        self.spilled += o.spilled;
        self.spill_lost_to_packing += o.spill_lost_to_packing;
        self.clipped += o.clipped;
        self.hashed += o.hashed;
    }
}

/// A cell's font as the measurer wants it. Underline and strike are read by
/// the reader but not drawn: both are separate commands in a stream whose Ink
/// pass would put them over the text, and neither changes where a glyph lands.
fn font_props(font: &liteparse_ooxml::xlsx::Font) -> FontProps {
    FontProps {
        family: Rc::from(font.name.as_deref().unwrap_or(DEFAULT_FONT_FAMILY)),
        size: Pt::new(font.size.unwrap_or(DEFAULT_FONT_SIZE)),
        bold: font.bold,
        italic: font.italic,
        underline: false,
        char_spacing: Pt::ZERO,
        text_scale: 1.0,
        underline_position: Pt::ZERO,
        underline_thickness: Pt::ZERO,
    }
}

fn width_of(m: &TextMeasurer, fp: &FontProps, text: &str) -> f64 {
    f64::from(f32::from(m.measure(text, fp).0))
}

/// The longest prefix of `text` that fits in `avail`, at a **character**
/// boundary — Excel clips mid-glyph, and stopping one glyph early is the whole
/// difference. Binary search, so a long string costs log(n) measures against
/// the memo rather than one per character.
fn fit_prefix<'a>(m: &TextMeasurer, fp: &FontProps, text: &'a str, avail: f64) -> &'a str {
    let bounds: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if width_of(m, fp, &text[..bounds[mid]]) <= avail {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    &text[..bounds[lo]]
}

/// Greedy word wrap inside `avail`, honouring the cell's own newlines. A word
/// longer than the whole box is broken at the character that overflows rather
/// than hanging out of it — Excel does the same.
fn wrap_lines(m: &TextMeasurer, fp: &FontProps, text: &str, avail: f64) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if width_of(m, fp, &candidate) <= avail || line.is_empty() && avail <= 0.0 {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // The word alone, broken as many times as it takes.
            let mut rest = word;
            while !rest.is_empty() && width_of(m, fp, rest) > avail {
                let head = fit_prefix(m, fp, rest, avail);
                // An `avail` too small for one character would loop forever;
                // emit that character and let it overhang.
                let head = if head.is_empty() {
                    &rest[..rest.chars().next().map_or(0, char::len_utf8)]
                } else {
                    head
                };
                out.push(head.to_string());
                rest = &rest[head.len()..];
            }
            line = rest.to_string();
        }
        out.push(line);
    }
    out
}

/// Which retained columns of one row hold something text may not run into: a
/// cell with display text, or any column a merge on this row covers (its box
/// is already claimed, even where the merge itself is blank).
///
/// Excel's rule is content, not style — a filled but empty cell does not stop
/// a spill — which is why this is derived from the row's valued cells rather
/// than from the paint cascade beside it.
fn occupied_columns(
    wb: &Workbook,
    geo: &SheetGeometry,
    plan: &SheetPlan<'_>,
    range: &Range<usize>,
    i: usize,
) -> Vec<bool> {
    let (_, cells) = &plan.rows[i];
    let mut occupied = vec![false; plan.cols.len()];
    for cell in cells {
        let b = geo.cell_box(plan, range, i, cell);
        let has_text = wb.display_text(cell).is_some_and(|t| !t.trim().is_empty());
        if !has_text && b.cols.len() <= 1 {
            continue;
        }
        for ci in b.cols {
            occupied[ci] = true;
        }
    }
    occupied
}

/// One cell's text as [`DrawCommand::Text`]s, one per line.
///
/// The rules are Excel's, and each carries the census number that justified
/// building it (paint census, 1,247 workbooks):
///
/// * **`wrapText` or an embedded newline** (6.9% + 76,952 cells) breaks over
///   lines inside the box.
/// * **Text wider than its box spills** into the run of empty neighbours in
///   the direction its alignment points (6.1% of unwrapped text cells), and is
///   **clipped** where a written neighbour stops it (12.8%) — the case a
///   painter that just draws the string at its origin turns into overlapping
///   mush.
/// * **A number wider than its column becomes `#######`** (2.0% of unwrapped
///   numeric cells). Numbers never spill; that is Excel's rule, not a
///   simplification.
/// * **`General` is "numbers right, text left"** — the alignment nobody
///   declares, and the reason a spreadsheet reads as a table.
///
/// The clip is at a character boundary rather than mid-glyph: the command
/// stream has no clip bracket, and adding one to paint half a glyph would put
/// a variant in front of every consumer that matches on `DrawCommand`.
#[allow(clippy::too_many_arguments)]
fn paint_cell_text(
    out: &mut Vec<DrawCommand>,
    m: &TextMeasurer,
    text: &str,
    numeric: bool,
    font: &liteparse_ooxml::xlsx::Font,
    align: &Alignment,
    color: RgbColor,
    b: &CellBox,
    y: f64,
    room: (f64, f64),
    declared_neighbour_free: bool,
) -> TextPaintStats {
    let mut stats = TextPaintStats::default();
    if align.wrap_text {
        stats.wrap_declared += 1;
    }
    let fp = font_props(font);
    let indent = if align.indent > 0 {
        width_of(m, &fp, INDENT_STEP) * f64::from(align.indent)
    } else {
        0.0
    };
    let right_aligned = matches!(align.horizontal, HorizontalAlign::Right)
        || (numeric && align.horizontal == HorizontalAlign::General);

    let mut x0 = b.x + f64::from(TEXT_INSET) + if right_aligned { 0.0 } else { indent };
    let mut avail = (b.w - 2.0 * f64::from(TEXT_INSET) - indent).max(0.0);

    let mut lines: Vec<String> = if align.wrap_text || text.contains('\n') {
        wrap_lines(m, &fp, text, avail)
    } else {
        vec![text.to_string()]
    };
    if lines.len() > 1 {
        stats.wrapped += 1;
    }

    // The overflow rules apply to the single-line case only: a wrapped cell
    // has already been fitted to its box.
    if lines.len() == 1 {
        let w = width_of(m, &fp, &lines[0]);
        if w > avail {
            if numeric {
                let hash = width_of(m, &fp, "#");
                let n = if hash > 0.0 {
                    (avail / hash).floor() as usize
                } else {
                    0
                };
                lines[0] = "#".repeat(n);
                stats.hashed += 1;
            } else {
                let (left, right) = room;
                let (grow_l, grow_r) = match align.horizontal {
                    HorizontalAlign::Right => (left, 0.0),
                    HorizontalAlign::Center | HorizontalAlign::CenterContinuous => (left, right),
                    _ => (0.0, right),
                };
                if grow_l > 0.0 || grow_r > 0.0 {
                    stats.spilled += 1;
                } else if declared_neighbour_free {
                    stats.spill_lost_to_packing += 1;
                }
                x0 -= grow_l;
                avail += grow_l + grow_r;
                if w > avail {
                    lines[0] = fit_prefix(m, &fp, &lines[0], avail).to_string();
                    stats.clipped += 1;
                }
            }
        }
    }

    let (_, metrics) = m.measure("", &fp);
    let (ascent, descent) = (
        f64::from(f32::from(metrics.ascent)),
        f64::from(f32::from(metrics.descent)),
    );
    let line_h = ascent + descent + f64::from(f32::from(metrics.leading));
    let n = lines.len() as f64;
    let inset_v = f64::from(TEXT_INSET);
    let first_baseline = match align.vertical {
        VerticalAlign::Top => y + inset_v + ascent,
        VerticalAlign::Center => y + (b.h - n * line_h) / 2.0 + ascent,
        // Bottom is the default, and `justify`/`distributed` over a single
        // line are bottom in Excel too.
        _ => y + b.h - inset_v - descent - (n - 1.0) * line_h,
    };

    for (li, line) in lines.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        let lw = width_of(m, &fp, line);
        let x = match align.horizontal {
            HorizontalAlign::Right => x0 + avail - lw,
            HorizontalAlign::Center | HorizontalAlign::CenterContinuous => x0 + (avail - lw) / 2.0,
            HorizontalAlign::General if numeric => x0 + avail - lw,
            _ => x0,
        };
        out.push(DrawCommand::Text {
            position: PtOffset {
                x: Pt::new(MARGIN + x as f32),
                y: Pt::new(MARGIN + (first_baseline + li as f64 * line_h) as f32),
            },
            text: Rc::from(line.as_str()),
            font_family: Rc::clone(&fp.family),
            char_spacing: Pt::ZERO,
            font_size: fp.size,
            bold: fp.bold,
            italic: fp.italic,
            color,
            text_scale: 1.0,
        });
    }
    if lines.iter().all(String::is_empty) {
        stats.blank += 1;
    } else {
        stats.cells += 1;
    }
    stats
}

/// One cell's box on a page, grid-relative: the rectangle plus the retained
/// columns it covers, which is what the spill rule needs to find the
/// neighbours it may run into.
struct CellBox {
    x: f64,
    w: f64,
    h: f64,
    cols: Range<usize>,
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

    /// One page's paint: gridlines under fills under borders, every one of
    /// them a [`DrawCommand::Rect`] so the raster's Shading pass keeps them in
    /// emission order (its pass order is fixed — `Path` < `Rect` < `Image` <
    /// `Line`/`Text` — so three variants would be three *layers*, in the wrong
    /// sequence).
    ///
    /// The population is the census's classes A + B + D + E: valued cells,
    /// the value-less styled cells the reader keeps in [`Row::styled_blanks`],
    /// `<row customFormat>` and `<col style>`. That is 15.9% of all declared
    /// paint on top of the 42.4% a valued-cells-only painter sees, and every
    /// unit of it lands on a cell box the geometry pass has already placed —
    /// no page changes size, no `TextItem` moves. Class C (paint on cells
    /// outside the packed grid, 41.7%) is dropped by design: honouring it
    /// means inventing the rows and columns to hang it on, which is the canvas
    /// explosion this pass exists to prevent, and Excel does not print it
    /// either.
    ///
    /// Three approximations, each with its number:
    ///
    /// * **Only `solid` fills paint.** 100.0% of filled corpus cells are
    ///   solid; the hatch branch is skipped rather than approximated because
    ///   slot 1 of every styles part is `gray125` — Excel's "no fill"
    ///   placeholder, usually with a black `fg` — and painting it solid would
    ///   black out sheets that asked for nothing.
    /// * **Dashed edges paint solid** (0.2% of edges), the same trade
    ///   [`BorderStyle`](liteparse_ooxml::xlsx::BorderStyle) already made by
    ///   keeping the full enum rather than mapping the tail to `None`.
    /// * **Diagonals are not drawn**: a `Rect` cannot express one, and the
    ///   variant that could (`Line`) paints in the Ink pass, over everything.
    ///
    /// Merged regions paint per cell rather than as one box. Excel writes the
    /// covered cells with the merge's own style, so the fill is right; the
    /// cost is the interior edges of a merge whose covered cells declare
    /// borders, which Excel suppresses.
    fn paint_page(
        &self,
        wb: &Workbook,
        sheet: &Sheet,
        plan: &SheetPlan<'_>,
        col_styles: &[Option<u32>],
        range: Range<usize>,
        measurer: Option<&TextMeasurer<'_>>,
    ) -> (Vec<DrawCommand>, TextPaintStats) {
        let y_base = self.y_off[range.start];
        let grid_w = *self.x_off.last().unwrap();
        let grid_h = self.y_off[range.end] - y_base;
        let mut cmds = Vec::new();
        let mut stats = TextPaintStats::default();

        // 1. Gridlines: one line per emitted-row and retained-column boundary,
        // under everything. They follow the *packed* grid, so a compacted-out
        // empty column leaves no line — the grid the reader sees is the grid
        // the text is in.
        if sheet.gridlines_visible() && grid_h > 0.0 {
            for x in &self.x_off {
                cmds.push(rect_cmd(*x, 0.0, GRIDLINE_W, grid_h, GRIDLINE_COLOR));
            }
            for i in range.start..=range.end {
                let y = self.y_off[i] - y_base;
                cmds.push(rect_cmd(0.0, y, grid_w, GRIDLINE_W, GRIDLINE_COLOR));
            }
        }

        // 2 & 3. Fills, then borders — collected in one walk and concatenated
        // so every fill is under every border, including its neighbour's.
        let mut borders = Vec::new();
        let mut styles: Vec<Option<u32>> = Vec::new();
        for i in range.clone() {
            let (row, cells) = &plan.rows[i];
            let y = self.y_off[i] - y_base;
            let h = self.row_height(i);
            row_paint_styles(row, cells, &plan.cols, col_styles, &mut styles);
            for (ci, style) in styles.iter().enumerate() {
                let Some(style) = *style else { continue };
                let (x, w) = (self.x_off[ci], self.x_off[ci + 1] - self.x_off[ci]);
                let fill = wb.styles.fill(Some(style));
                if fill.pattern == PatternType::Solid {
                    // An automatic `fg` is not white — it is "the consumer's
                    // default background", which is the page this is painted
                    // on. Painting it would cover the gridlines with the
                    // colour they are already on.
                    if let Some(rgb) = fill.fg.and_then(|c| wb.resolve_color(c)) {
                        cmds.push(rect_cmd(x, y, w, h, rgb_of(rgb)));
                    }
                }
                let border = wb.styles.border(Some(style));
                if border.paints() {
                    push_border(&mut borders, wb, &border, x, y, w, h);
                }
            }
        }
        cmds.append(&mut borders);

        // 4. Cell text, over every rect: the raster's Ink pass paints `Text`
        // after `Rect` whatever the emission order, so this is placement, not
        // layering. The style cascade is the paint walk's — a cell with no
        // `s=` takes its font from its row's `customFormat` or its column,
        // exactly as its fill does.
        if let Some(m) = measurer {
            for i in range.clone() {
                let (row, cells) = &plan.rows[i];
                let y = self.y_off[i] - y_base;
                row_paint_styles(row, cells, &plan.cols, col_styles, &mut styles);
                let occupied = occupied_columns(wb, self, plan, &range, i);
                for cell in cells {
                    let text = wb.display_text(cell).unwrap_or_default();
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let b = self.cell_box(plan, &range, i, cell);
                    if b.cols.is_empty() {
                        continue;
                    }
                    let style = cell
                        .style
                        .or_else(|| styles.get(b.cols.start).copied().flatten());
                    let color = wb
                        .styles
                        .font(style)
                        .color
                        .and_then(|c| wb.resolve_color(c))
                        .map_or(RgbColor::BLACK, rgb_of);
                    stats.add(paint_cell_text(
                        &mut cmds,
                        m,
                        text,
                        matches!(cell.value, CellValue::Number(_)),
                        &wb.styles.font(style),
                        &wb.styles.alignment(style),
                        color,
                        &b,
                        y,
                        self.spill_room(&occupied, &b.cols),
                        !cells.iter().any(|n| {
                            n.at.col == cell.at.col + 1
                                && wb.display_text(n).is_some_and(|t| !t.trim().is_empty())
                        }),
                    ));
                }
            }
        }
        (cmds, stats)
    }

    /// How far a cell's text may run left and right before it meets written
    /// content, in points: the widths of the unbroken runs of empty retained
    /// columns on each side of the cell's own span.
    fn spill_room(&self, occupied: &[bool], cols: &Range<usize>) -> (f64, f64) {
        let mut lo = cols.start;
        while lo > 0 && !occupied[lo - 1] {
            lo -= 1;
        }
        let mut hi = cols.end;
        while hi < occupied.len() && !occupied[hi] {
            hi += 1;
        }
        (
            self.x_off[cols.start] - self.x_off[lo],
            self.x_off[hi] - self.x_off[cols.end],
        )
    }

    /// One cell's box in grid-relative points: its own column and row, or the
    /// merge's extent when it anchors one, clamped to the rows this page holds
    /// (the block slicer clamps a cut merge's rowspan the same way).
    ///
    /// Shared by the item builder and the text painter so a glyph cannot be
    /// laid out in a box the `TextItem` does not have — the two walk the same
    /// rows for different outputs, and a merge rule stated twice is a merge
    /// rule that drifts.
    fn cell_box(
        &self,
        plan: &SheetPlan<'_>,
        range: &Range<usize>,
        i: usize,
        cell: &liteparse_ooxml::xlsx::Cell,
    ) -> CellBox {
        let (row, _) = &plan.rows[i];
        let ci = plan
            .cols
            .binary_search(&cell.at.col)
            .unwrap_or_else(|_| unreachable!("retained columns are the union of all cells"));
        match plan.anchors.get(&(row.index, cell.at.col)) {
            Some(&idx) => {
                let m = &plan.plans[idx];
                let clo = plan.cols.partition_point(|&c| c < m.col_range.0);
                let chi = plan.cols.partition_point(|&c| c <= m.col_range.1);
                let mut j = i;
                while j + 1 < range.end && plan.rows[j + 1].0.index <= m.row_range.1 {
                    j += 1;
                }
                CellBox {
                    x: self.x_off[clo],
                    w: self.x_off[chi] - self.x_off[clo],
                    h: self.y_off[j + 1] - self.y_off[i],
                    cols: clo..chi,
                }
            }
            None => CellBox {
                x: self.x_off[ci],
                w: self.x_off[ci + 1] - self.x_off[ci],
                h: self.row_height(i),
                cols: ci..ci + 1,
            },
        }
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
            for cell in cells {
                let text = wb.display_text(cell).unwrap_or_default();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let b = self.cell_box(plan, &range, i, cell);
                let (x, w, h) = (b.x, b.w, b.h as f32);
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
        workbook_to_pages(&wb, EmitOptions::default(), None)
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
            None,
        );
        let site = &nx.pages[0].text_items[0];
        assert_eq!(site.text, "site");
        assert_eq!(site.link.as_deref(), Some("https://example.com"));
    }

    // ── the grid painter ────────────────────────────────────────────────────

    /// A styles part with the fills, borders and cell formats the paint tests
    /// name by index. Slots 0 and 1 are Excel's own (`none`, `gray125`), which
    /// is what makes the gray125 test below meaningful.
    const PAINT_STYLES: &str = r#"<styleSheet>
      <fonts count="1"><font/></fonts>
      <fills count="4">
        <fill><patternFill patternType="none"/></fill>
        <fill><patternFill patternType="gray125"><fgColor rgb="FF000000"/></patternFill></fill>
        <fill><patternFill patternType="solid"><fgColor rgb="FF3366CC"/></patternFill></fill>
        <fill><patternFill patternType="solid"><fgColor rgb="FFFFDD00"/></patternFill></fill>
      </fills>
      <borders count="2">
        <border><left/><right/><top/><bottom/></border>
        <border><left style="thin"><color rgb="FFFF0000"/></left><right/><top/><bottom style="medium"/></border>
      </borders>
      <cellXfs count="6">
        <xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>
        <xf numFmtId="0" fontId="0" fillId="2" borderId="0"/>
        <xf numFmtId="0" fontId="0" fillId="3" borderId="0"/>
        <xf numFmtId="0" fontId="0" fillId="0" borderId="1"/>
        <xf numFmtId="0" fontId="0" fillId="1" borderId="0"/>
        <xf numFmtId="0" fontId="0" fillId="2" borderId="1"/>
      </cellXfs>
    </styleSheet>"#;

    fn painted(sheet_xml: &str) -> NativeXlsx {
        let wb = workbook_from(sheet_xml, &[("xl/styles.xml", PAINT_STYLES)]);
        workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            // Grid only: the text pass has its own fixtures, and a measured
            // string would make every rect assertion host-dependent.
            None,
        )
    }

    fn rects(page: &LayoutedPage) -> Vec<(f32, f32, f32, f32, RgbColor)> {
        page.commands
            .iter()
            .map(|c| match c {
                DrawCommand::Rect { rect, color } => (
                    rect.origin.x.raw(),
                    rect.origin.y.raw(),
                    rect.size.width.raw(),
                    rect.size.height.raw(),
                    *color,
                ),
                other => panic!("the grid painter emits Rects only, got {other:?}"),
            })
            .collect()
    }

    const BLUE: RgbColor = RgbColor {
        r: 0x33,
        g: 0x66,
        b: 0xCC,
    };
    const YELLOW: RgbColor = RgbColor {
        r: 0xFF,
        g: 0xDD,
        b: 0x00,
    };

    /// Gridlines are one line per boundary of the *packed* grid, under
    /// everything, and they obey the sheet's own switch.
    #[test]
    fn gridlines_frame_the_packed_grid_and_can_be_turned_off() {
        let body = r#"<sheetData>
            <row r="1" ht="20"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c></row>
            <row r="2" ht="20"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
        </sheetData>"#;
        let on = painted(&format!("<worksheet>{body}</worksheet>"));
        let lines = rects(&on.layouts[0]);
        // 2 columns → 3 verticals, 2 rows → 3 horizontals, nothing else.
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|l| l.4 == GRIDLINE_COLOR));
        // Verticals span the page's rows; horizontals span the grid width.
        assert!((lines[0].3 - 40.0).abs() < 1e-4, "{:?}", lines[0]);
        assert_eq!(lines[0].0, MARGIN);
        assert_eq!(lines[3].1, MARGIN);
        assert!((lines[5].1 - (MARGIN + 40.0)).abs() < 1e-4);

        let off = painted(&format!(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews>{body}</worksheet>"#
        ));
        assert!(off.layouts[0].commands.is_empty());
    }

    /// Fills under borders, both over the gridlines — the emission order the
    /// raster's single Shading pass turns into z-order.
    #[test]
    fn fills_paint_under_borders_and_over_the_gridlines() {
        let nx = painted(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews><sheetData>
                <row r="1" ht="20"><c r="A1" s="5"><v>1</v></c><c r="B1" s="1"><v>2</v></c></row>
                <row r="2" ht="20"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        let r = rects(&nx.layouts[0]);
        // Two solid fills, then A1's two edges: left thin red, bottom medium.
        assert_eq!(r.len(), 4, "{r:?}");
        assert_eq!(r[0].4, BLUE);
        assert_eq!(r[1].4, BLUE);
        let (left, bottom) = (r[3], r[2]);
        assert_eq!(bottom.4, RgbColor::BLACK, "an uncoloured edge is black");
        assert_eq!(
            left.4,
            RgbColor {
                r: 0xFF,
                g: 0,
                b: 0
            }
        );
        // Bottom edge: full cell width, medium (1.5 pt), at the row's foot.
        assert!((bottom.3 - 1.5).abs() < 1e-4);
        assert!((bottom.1 - (MARGIN + 20.0 - 1.5)).abs() < 1e-4);
        // Left edge: thin (0.75 pt), inset below the bottom edge it meets.
        assert!((left.2 - 0.75).abs() < 1e-4);
        assert!((left.3 - (20.0 - 1.5)).abs() < 1e-4);
    }

    /// `gray125` is slot 1 of every styles part and means "no fill". Painting
    /// its `fg` solid would black out sheets that asked for nothing.
    #[test]
    fn the_gray125_placeholder_paints_nothing() {
        let nx = painted(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews><sheetData>
                <row r="1"><c r="A1" s="4"><v>1</v></c><c r="B1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        assert!(nx.layouts[0].commands.is_empty());
    }

    /// The census's classes B, D and E — the 15.9% of declared paint that a
    /// valued-cells-only painter cannot see. A styled blank paints its own
    /// cell, `<row customFormat>` paints the row's retained columns, and
    /// `<col style>` paints its span; the cascade is cell > row > column.
    #[test]
    fn blanks_and_row_and_column_formats_all_paint() {
        let nx = painted(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews>
            <cols><col min="1" max="2" width="10" customWidth="1" style="1"/></cols>
            <sheetData>
                <row r="1" ht="20"><c r="A1"><v>1</v></c><c r="B1" s="4"/></row>
                <row r="2" ht="20" s="2" customFormat="1"><c r="A2"><v>3</v></c><c r="B2" s="1"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        let r = rects(&nx.layouts[0]);
        // Row 1: A1 takes the column's blue (class E), B1's own style wins and
        // is the no-paint placeholder (class B). Row 2: the row's yellow on
        // both columns (class D), with B2's own blue overriding it.
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!((r[0].1, r[0].4), (MARGIN, BLUE));
        assert!((r[0].2 - 56.25).abs() < 1e-4, "the column's own width");
        assert_eq!((r[1].1, r[1].4), (MARGIN + 20.0, YELLOW));
        assert_eq!((r[2].1, r[2].4), (MARGIN + 20.0, BLUE));
        assert!((r[2].0 - (MARGIN + 56.25)).abs() < 1e-4, "column B");
    }

    /// Paint follows the page split: a page holds its own rows' ink and the
    /// page-relative origin, like every item on it.
    #[test]
    fn paint_paginates_with_the_rows() {
        let rows_xml: String = (1..=15)
            .map(|r| {
                format!(
                    r#"<row r="{r}" ht="100"><c r="A{r}" s="1"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#
                )
            })
            .collect();
        let nx = painted(&format!(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews><sheetData>{rows_xml}</sheetData></worksheet>"#
        ));
        assert_eq!(nx.layouts.len(), 3);
        assert_eq!(rects(&nx.layouts[0]).len(), 7, "one fill per row on page 1");
        assert_eq!(rects(&nx.layouts[2]).len(), 1);
        assert_eq!(rects(&nx.layouts[2])[0].1, MARGIN, "page-relative");
        // The layout's page box is the item pages' box.
        for (layout, page) in nx.layouts.iter().zip(&nx.pages) {
            assert_eq!(layout.page_size.width.raw(), page.page_width);
            assert_eq!(layout.page_size.height.raw(), page.page_height);
        }
    }

    /// An empty sheet has a page and no paint — the painter must not invent a
    /// grid for a sheet the plan refused.
    #[test]
    fn an_empty_sheet_paints_nothing() {
        let nx = painted("<worksheet><sheetData/></worksheet>");
        assert_eq!(nx.layouts.len(), 1);
        assert!(nx.layouts[0].commands.is_empty());
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
        workbook_to_pages(&wb, opts, None)
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

    /// `paint` alone paints the picture: neither `figures` nor `images` is
    /// what reaches the bytes, which is the contract the screenshot path
    /// depends on (it asks for neither). The command's rect is the same rect
    /// `pic_rects` reports, so a raster and a bbox consumer cannot disagree.
    #[test]
    fn paint_emits_one_image_command_per_placement() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1"><v>1</v></c></row>
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
                paint: true,
                ..Default::default()
            },
        );
        assert!(nx.images.is_empty(), "paint collects no extraction entries");
        let images: Vec<_> = nx.layouts[0]
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Image {
                    rect,
                    image_data,
                    src_rect,
                    float,
                } => Some((rect, image_data, src_rect, float)),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        let (rect, data, src_rect, float) = images[0];
        assert!(src_rect.is_none(), "a:srcRect is not read for XLSX yet");
        assert!(*float, "an XLSX picture paints above the grid's text");
        assert_eq!(&*data.data, b"not-really-png-but-bytes");
        let r = &nx.pic_rects[0][0];
        assert_eq!(
            (
                rect.origin.x.raw(),
                rect.origin.y.raw(),
                rect.size.width.raw(),
                rect.size.height.raw()
            ),
            (r.x, r.y, r.width, r.height),
            "the painted box and the reported box are one box"
        );
        // The grid still painted underneath — the floating layer appends, it
        // does not replace.
        assert!(
            nx.layouts[0]
                .commands
                .iter()
                .any(|c| matches!(c, DrawCommand::Rect { .. }))
        );
    }

    /// Without `paint` there is no image command and no byte copy — the
    /// parse path's cost is unchanged by this layer existing.
    #[test]
    fn a_parse_paints_no_pictures() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="1"><c r="A1"><v>1</v></c></row>
        </sheetData><drawing r:id="rId7"/></worksheet>"#;
        let anchor = pic_anchor(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="457200"/>"#,
        );
        let nx = layout_with_pic(
            sheet,
            &format!("{anchor}</xdr:oneCellAnchor>"),
            EmitOptions::default(),
        );
        assert_eq!(nx.pic_rects[0].len(), 1, "the rect is still reported");
        assert!(nx.layouts[0].commands.is_empty());
    }

    /// An image-only sheet has no grid to paint and still has something to
    /// draw: the picture command lives outside the `SheetPlan` arm.
    #[test]
    fn a_plan_less_sheet_still_paints_its_picture() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><drawing r:id="rId7"/></worksheet>"#;
        let anchor = pic_anchor(
            r#"<xdr:oneCellAnchor>
                 <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
                 <xdr:ext cx="914400" cy="914400"/>"#,
        );
        let nx = layout_with_pic(
            sheet,
            &format!("{anchor}</xdr:oneCellAnchor>"),
            EmitOptions {
                paint: true,
                ..Default::default()
            },
        );
        assert_eq!(nx.layouts.len(), 1);
        assert_eq!(
            nx.layouts[0]
                .commands
                .iter()
                .filter(|c| matches!(c, DrawCommand::Image { .. }))
                .count(),
            1
        );
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
        let nx = workbook_to_pages(&wb, opts, None);
        assert_eq!(
            format!("{emitted:?}"),
            format!("{:?}", nx.all_blocks),
            "geometry-pass doc blocks diverge from the emitter's"
        );
    }

    // ── floating text shapes ────────────────────────────────────────────────

    fn shape_drawing_parts(anchor_xml: &str) -> Vec<(String, String)> {
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

    /// A two-paragraph shape yields one item per paragraph, stacked in the
    /// shape's box, carrying the run's declared face and size — and the doc
    /// blocks stay byte-equal to the emitter's, title-before-table split
    /// included.
    #[test]
    fn shape_items_stack_in_the_box_and_doc_blocks_match_the_emitter() {
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
            <row r="4"><c r="A4" t="inlineStr"><is><t>a</t></is></c><c r="B4"><v>1</v></c></row>
            <row r="5"><c r="A5"><v>2</v></c><c r="B5"><v>3</v></c></row>
        </sheetData><drawing r:id="rId7"/></worksheet>"#;
        // Anchored at row 0 — above the first written row (r=4, index 3).
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="TextBox 1"/><xdr:cNvSpPr txBox="1"/></xdr:nvSpPr>
               <xdr:spPr/><xdr:txBody><a:bodyPr/>
                 <a:p><a:r><a:rPr sz="1400" b="1"><a:latin typeface="Arial"/></a:rPr><a:t>Title</a:t></a:r></a:p>
                 <a:p><a:r><a:rPr sz="1100"/><a:t>subtitle</a:t></a:r></a:p>
               </xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let parts = shape_drawing_parts(anchor);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let wb = workbook_from(sheet, &extra);
        let opts = EmitOptions::default();

        let emitted: Vec<Block> = crate::office::xlsx::emit_workbook(&wb, opts)
            .blocks
            .into_iter()
            .map(|(b, _)| b)
            .collect();
        let nx = workbook_to_pages(&wb, opts, None);
        assert_eq!(
            format!("{emitted:?}"),
            format!("{:?}", nx.all_blocks),
            "geometry-pass doc blocks diverge from the emitter's"
        );

        // Page blocks mirror the doc split: title right after the heading.
        assert!(matches!(&nx.page_blocks[0][0], Block::Heading { .. }));
        assert!(
            matches!(&nx.page_blocks[0][1], Block::Paragraph { text, .. } if text == "Title"),
            "got {:?}",
            nx.page_blocks[0]
        );

        // Two items, stacked halves of the 914400×457200 EMU box (72×36 pt),
        // anchored at the grid origin (row 0 collapses to the packed top).
        let items: Vec<&TextItem> = nx.pages[0]
            .text_items
            .iter()
            .filter(|t| t.text == "Title" || t.text == "subtitle")
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].font_name.as_deref(), Some("Arial"));
        assert_eq!(items[0].font_size, Some(14.0));
        assert_eq!(items[0].font_weight, Some(700));
        assert_eq!(items[1].font_size, Some(11.0));
        assert!(
            (items[0].width - 72.0).abs() < 0.5,
            "box width is the anchor ext"
        );
        assert!((items[0].height - 18.0).abs() < 0.5, "half the box each");
        assert!((items[1].y - items[0].y - 18.0).abs() < 0.5, "stacked");
    }

    /// A shape anchored on later pages' rows lands its blocks and items on
    /// that page, like a picture.
    #[test]
    fn a_shape_lands_on_its_anchor_row_page() {
        let mut rows = String::new();
        for r in 1..=60 {
            rows.push_str(&format!(
                r#"<row r="{r}" ht="20"><c r="A{r}"><v>{r}</v></c></row>"#
            ));
        }
        let sheet = format!(
            r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>{rows}</sheetData><drawing r:id="rId7"/></worksheet>"#
        );
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>50</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r><a:t>deep note</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let parts = shape_drawing_parts(anchor);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let wb = workbook_from(&sheet, &extra);
        let nx = workbook_to_pages(&wb, EmitOptions::default(), None);
        assert!(nx.pages.len() > 1, "60 rows at 20pt must paginate");
        let with_item: Vec<usize> = nx
            .pages
            .iter()
            .enumerate()
            .filter(|(_, p)| p.text_items.iter().any(|t| t.text == "deep note"))
            .map(|(i, _)| i)
            .collect();
        let with_block: Vec<usize> = nx
            .page_blocks
            .iter()
            .enumerate()
            .filter(|(_, blocks)| {
                blocks
                    .iter()
                    .any(|b| matches!(b, Block::Paragraph { text, .. } if text == "deep note"))
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(with_item, with_block, "item page and block page agree");
        assert_eq!(with_item.len(), 1);
        assert!(with_item[0] > 0, "row 50 is not on page 1");
    }
    // ── cell text ───────────────────────────────────────────────────────────

    /// A styles part for the text tests: alignments, a wrap, an indent and a
    /// coloured font. Sizes are stated so a host with a different default
    /// cannot move the assertions.
    const TEXT_STYLES: &str = r#"<styleSheet>
      <fonts count="2">
        <font><sz val="11"/><name val="Arial"/></font>
        <font><sz val="11"/><name val="Arial"/><color rgb="FFCC0000"/></font>
      </fonts>
      <fills count="1"><fill><patternFill patternType="none"/></fill></fills>
      <borders count="1"><border/></borders>
      <cellXfs count="6">
        <xf numFmtId="0" fontId="0"/>
        <xf numFmtId="0" fontId="0"><alignment horizontal="right"/></xf>
        <xf numFmtId="0" fontId="0"><alignment horizontal="center"/></xf>
        <xf numFmtId="0" fontId="0"><alignment wrapText="1"/></xf>
        <xf numFmtId="0" fontId="1"/>
        <xf numFmtId="0" fontId="0"><alignment vertical="top"/></xf>
      </cellXfs>
    </styleSheet>"#;

    /// The text pass measures, so its fixtures need the host's fonts. Every
    /// assertion below is a *relation* between two measured runs — never an
    /// absolute width — because the host that runs this test is not the host
    /// that wrote the file.
    fn texted(sheet_xml: &str) -> NativeXlsx {
        let wb = workbook_from(sheet_xml, &[("xl/styles.xml", TEXT_STYLES)]);
        let registry = FontRegistry::new();
        workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            Some(&registry),
        )
    }

    /// (text, x, baseline y, colour) of every text command on a page, in
    /// emission order.
    fn texts(page: &LayoutedPage) -> Vec<(String, f32, f32, RgbColor)> {
        page.commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Text {
                    text,
                    position,
                    color,
                    ..
                } => Some((text.to_string(), position.x.raw(), position.y.raw(), *color)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_paints_only_with_a_registry_and_never_into_the_items() {
        let sheet = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>label</t></is></c></row>
        </sheetData></worksheet>"#;
        let with = texted(sheet);
        assert_eq!(texts(&with.layouts[0]).len(), 1);
        assert_eq!(with.text_stats.cells, 1);

        // The same workbook painted without a registry: the grid, no text.
        let wb = workbook_from(sheet, &[("xl/styles.xml", TEXT_STYLES)]);
        let without = workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            None,
        );
        assert!(texts(&without.layouts[0]).is_empty());
        assert_eq!(without.text_stats, TextPaintStats::default());
        // And the item — the thing a parse consumes — is identical either way.
        assert_eq!(
            format!("{:?}", with.pages[0].text_items),
            format!("{:?}", without.pages[0].text_items),
            "measuring must not move a TextItem"
        );
    }

    /// `General` is the alignment nobody declares, and the reason a sheet
    /// reads as a table: the number sits at its cell's right edge, the label
    /// at its left.
    #[test]
    fn general_alignment_puts_numbers_right_and_text_left() {
        let nx = texted(
            r#"<worksheet><cols><col min="1" max="2" width="12" customWidth="1"/></cols><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>ab</t></is></c><c r="B1"><v>7</v></c></row>
        </sheetData></worksheet>"#,
        );
        let t = texts(&nx.layouts[0]);
        assert_eq!(t.len(), 2);
        let (label, number) = (&t[0], &t[1]);
        // The label starts at its cell's left inset; the number's right edge
        // is at its own cell's right inset, so it starts well inside.
        let col_w = nx.pages[0].text_items[1].x - nx.pages[0].text_items[0].x;
        assert!(
            number.1 - label.1 > col_w,
            "a General number must hug its right edge: label at {}, number at {}, column {col_w}",
            label.1,
            number.1
        );
    }

    #[test]
    fn declared_alignments_place_the_line_in_its_box() {
        let sheet = |style: u32| {
            format!(
                r#"<worksheet><cols><col min="1" max="1" width="20" customWidth="1"/></cols><sheetData>
                <row r="1"><c r="A1" s="{style}" t="inlineStr"><is><t>ab</t></is></c></row>
            </sheetData></worksheet>"#
            )
        };
        let left = texted(&sheet(0)).layouts[0].commands.clone();
        let right = texted(&sheet(1)).layouts[0].commands.clone();
        let centre = texted(&sheet(2)).layouts[0].commands.clone();
        let x = |cmds: &[DrawCommand]| {
            cmds.iter()
                .find_map(|c| match c {
                    DrawCommand::Text { position, .. } => Some(position.x.raw()),
                    _ => None,
                })
                .expect("one text command")
        };
        let (l, c, r) = (x(&left), x(&centre), x(&right));
        assert!(l < c && c < r, "left {l} < centre {c} < right {r}");
    }

    /// Bottom is Excel's default vertical alignment, so a one-line label in a
    /// tall row sits on the row's floor — the rule that keeps a heading on its
    /// own gridline instead of floating at the top of the box.
    #[test]
    fn vertical_default_is_bottom_and_top_is_honoured() {
        let sheet = |style: u32| {
            format!(
                r#"<worksheet><sheetData>
                <row r="1" ht="80" customHeight="1"><c r="A1" s="{style}" t="inlineStr"><is><t>ab</t></is></c></row>
            </sheetData></worksheet>"#
            )
        };
        let bottom = texted(&sheet(0));
        let top = texted(&sheet(5));
        let y = |nx: &NativeXlsx| texts(&nx.layouts[0])[0].2;
        assert!(
            y(&bottom) - y(&top) > 40.0,
            "an 80 pt row must separate a bottom baseline from a top one: {} vs {}",
            y(&bottom),
            y(&top)
        );
    }

    #[test]
    fn wrap_text_breaks_over_lines_inside_the_box() {
        let sheet = |style: u32| {
            format!(
                r#"<worksheet><cols><col min="1" max="2" width="6" customWidth="1"/></cols><sheetData>
                <row r="1" ht="60" customHeight="1"><c r="A1" s="{style}" t="inlineStr"><is><t>alpha beta gamma delta</t></is></c><c r="B1" t="inlineStr"><is><t>x</t></is></c></row>
            </sheetData></worksheet>"#
            )
        };
        let wrapped = texted(&sheet(3));
        let lines = texts(&wrapped.layouts[0]);
        assert!(lines.len() > 2, "a narrow wrapped cell breaks: {lines:?}");
        assert_eq!(wrapped.text_stats.wrapped, 1);
        // Every line stays inside the cell — the neighbour is written, so
        // nothing may reach into it. The last command is the neighbour's own
        // "x": the row walk paints A1's lines, then B1.
        let (_, b_x, ..) = *lines.last().expect("the neighbour paints");
        for (line, x, ..) in &lines[..lines.len() - 1] {
            assert!(*x < b_x, "line {line:?} at {x} escaped into column B");
        }
    }

    /// The 12.8% / 6.1% pair from the paint census, as one fixture: the same
    /// oversized string clips against a written neighbour and spills past an
    /// empty one.
    #[test]
    fn overflow_spills_into_an_empty_neighbour_and_clips_against_a_written_one() {
        let long = "the quick brown fox jumps over the lazy dog";
        let sheet = |neighbour: &str| {
            format!(
                r#"<worksheet><cols><col min="1" max="3" width="6" customWidth="1"/></cols><sheetData>
                <row r="1"><c r="A1" t="inlineStr"><is><t>{long}</t></is></c>{neighbour}</row>
                <row r="2"><c r="C2" t="inlineStr"><is><t>C</t></is></c></row>
            </sheetData></worksheet>"#
            )
        };
        let blocked = texted(&sheet(
            r#"<c r="B1" t="inlineStr"><is><t>stop</t></is></c>"#,
        ));
        let spilled = texted(&sheet(""));

        assert_eq!(blocked.text_stats.clipped, 1);
        assert_eq!(blocked.text_stats.spilled, 0);
        let cut = &texts(&blocked.layouts[0])[0].0;
        assert!(
            cut.len() < long.len() && long.starts_with(cut.as_str()),
            "the clip is a prefix at a character boundary, got {cut:?}"
        );

        assert_eq!(spilled.text_stats.spilled, 1);
        let run = &texts(&spilled.layouts[0])[0].0;
        assert!(
            run.len() > cut.len(),
            "spilling past two empty columns must show more than clipping at one: {run:?} vs {cut:?}"
        );
    }

    /// Numbers never spill — Excel's own rule, not a simplification — so a
    /// number too wide for its column becomes hashes instead.
    #[test]
    fn a_number_too_wide_for_its_column_becomes_hashes() {
        let nx = texted(
            r#"<worksheet><cols><col min="1" max="2" width="3" customWidth="1"/></cols><sheetData>
            <row r="1"><c r="A1"><v>123456789012345</v></c></row>
        </sheetData></worksheet>"#,
        );
        let t = texts(&nx.layouts[0]);
        assert_eq!(nx.text_stats.hashed, 1);
        assert!(
            !t[0].0.is_empty() && t[0].0.chars().all(|c| c == '#'),
            "expected hashes, got {:?}",
            t[0].0
        );
        // The item keeps the value's *display* text — General renders a
        // number this large in scientific notation, which is Excel's own
        // answer and fits nowhere near a 3-unit column either. Only the
        // raster says it does not fit; an extraction consumer still reads it.
        assert_eq!(nx.pages[0].text_items[0].text, "1.23457E+14");
    }

    #[test]
    fn a_font_colour_reaches_the_command_and_an_absent_one_is_black() {
        let nx = texted(
            r#"<worksheet><sheetData>
            <row r="1"><c r="A1" s="4" t="inlineStr"><is><t>red</t></is></c><c r="B1" t="inlineStr"><is><t>plain</t></is></c></row>
        </sheetData></worksheet>"#,
        );
        let t = texts(&nx.layouts[0]);
        assert_eq!(
            t[0].3,
            RgbColor {
                r: 0xCC,
                g: 0,
                b: 0
            }
        );
        assert_eq!(t[1].3, RgbColor::BLACK);
    }

    /// A cell with no `s=` takes its font from the row's `customFormat` or its
    /// column, exactly as its fill does — the cascade the paint walk already
    /// owns, applied to text.
    #[test]
    fn an_unstyled_cell_takes_the_row_and_column_font_colour() {
        let nx = texted(
            r#"<worksheet><cols><col min="2" max="2" width="9" style="4"/></cols><sheetData>
            <row r="1" s="4" customFormat="1"><c r="A1" t="inlineStr"><is><t>row</t></is></c></row>
            <row r="2"><c r="B2" t="inlineStr"><is><t>col</t></is></c></row>
        </sheetData></worksheet>"#,
        );
        let red = RgbColor {
            r: 0xCC,
            g: 0,
            b: 0,
        };
        let t = texts(&nx.layouts[0]);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].3, red, "row customFormat font");
        assert_eq!(t[1].3, red, "col style font");
    }
}
