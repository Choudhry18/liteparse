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

use liteparse_ooxml::model::dimension::Dimension;
use liteparse_ooxml::render::dimension::Pt;
use liteparse_ooxml::render::fonts::FontRegistry;
use liteparse_ooxml::render::geometry::{PtLineSegment, PtOffset, PtRect, PtSize};
use liteparse_ooxml::render::layout::draw_command::{
    DrawCommand, LayoutedPage, ShapeTransform, TransformMark,
};
use liteparse_ooxml::render::layout::fragment::FontProps;
use liteparse_ooxml::render::layout::measurer::TextMeasurer;
use liteparse_ooxml::render::resolve::color::{RgbColor, rgb_from_u32};
use liteparse_ooxml::render::resolve::drawing_color::{DrawingColorContext, resolve_drawing_color};
use liteparse_ooxml::render::resolve::fonts::resolve_font_set_themes;
use liteparse_ooxml::render::resolve::images::MediaEntry;
use liteparse_ooxml::xlsx::{
    Alignment, Border, BorderEdge, BorderStyle, CellAnchor, CellValue, HorizontalAlign,
    PatternType, PicAnchor, Row, Sheet, SheetShape, VerticalAlign, Workbook,
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
/// §21.1.2.3 default run colour — what a shape run paints in when it
/// declares no `a:solidFill` of its own (a spec default, not a guess).
/// Declared colours resolve through the workbook's theme in
/// [`shape_run_color`].
const SHAPE_TEXT_COLOR: RgbColor = RgbColor { r: 0, g: 0, b: 0 };

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
    /// Every placed floating text shape, in the flat
    /// [`super::xlsx::ordered_shapes`] walk across sheets, paired with the
    /// 0-based index into `pages` its anchor landed on. One entry per shape
    /// with no filter of any kind, so a consumer that re-walks the workbook
    /// the same way pairs by construction rather than by matching rectangles
    /// — the same contract the picture ordinal carries.
    ///
    /// The rectangle is the shape's box, which is *not* where its text is:
    /// the items inside it are stacked evenly rather than laid out. That gap
    /// is what this field exists to let a census measure.
    pub shape_rects: Vec<(usize, Rect)>,
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
        shape_rects: Vec::new(),
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
        // Page-local here, rebased onto the workbook's page numbering below —
        // `page_local` counts within this sheet, and `pages` runs across all
        // of them.
        let mut shape_rects: Vec<(usize, Rect)> = Vec::new();
        let mut shape_cmds_per_page: Vec<Vec<DrawCommand>> = vec![Vec::new(); ranges.len()];
        // The paint channel: every anchored drawing object's fills and
        // outlines, placed by the same anchor math as everything else.
        // Emitted before the text loop below so a page's fills precede its
        // shape text in the stream — the float pass paints in stream order,
        // and that order is what keeps a callout's words on top of its box.
        let mut ink_cmds_per_page: Vec<Vec<DrawCommand>> = vec![Vec::new(); ranges.len()];
        if opts.paint {
            for ink in &sheet.ink {
                let (page_local, rect) = geo.place_pic(
                    cols,
                    &row_indices,
                    &ranges,
                    page_width,
                    &canvas,
                    &ink.anchor,
                    None,
                );
                ink_commands(
                    ink,
                    &rect,
                    wb.theme.as_ref(),
                    &mut ink_cmds_per_page[page_local],
                );
            }
        }
        for shape in super::xlsx::ordered_shapes(sheet) {
            let (page_local, rect) = geo.place_pic(
                cols,
                &row_indices,
                &ranges,
                page_width,
                &canvas,
                &shape.anchor,
                None,
            );
            shape_rects.push((page_local, rect.clone()));
            shape_text_items(shape, &rect, &mut shape_items_per_page[page_local]);
            // Painted from a real sub-layout, not from the items above — see
            // `shape_commands` for why the two are allowed to disagree.
            if let (true, Some(m)) = (opts.paint, measurer.as_ref()) {
                shape_cmds_per_page[page_local].extend(shape_commands(
                    shape,
                    &rect,
                    wb.theme.as_ref(),
                    m,
                ));
            }
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
                pic.frac,
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

        let page_base = out.pages.len();
        out.shape_rects
            .extend(shape_rects.into_iter().map(|(p, r)| (page_base + p, r)));

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
                // content is a picture — or a shape — has no grid to paint
                // and still has something to draw.
                //
                // The floating drawing layer, in Excel's z-order: shape
                // fills and outlines, then shape text, both bracketed into
                // the rasterizer's `Float` pass so they draw over the grid
                // *and* the cell values (the same relation pictures already
                // claim via their flag), then the pictures on top. Within
                // the layer the approximation is the PPTX painter's: all
                // fills before all text, so an overlapping shape's box never
                // covers a neighbour's words.
                use liteparse_ooxml::render::layout::draw_command::FloatMark;
                if !ink_cmds_per_page[i].is_empty() {
                    layout.commands.push(DrawCommand::Float(FloatMark::Begin));
                    layout.commands.append(&mut ink_cmds_per_page[i]);
                    layout.commands.push(DrawCommand::Float(FloatMark::End));
                }
                if !shape_cmds_per_page[i].is_empty() {
                    layout.commands.push(DrawCommand::Float(FloatMark::Begin));
                    layout.commands.append(&mut shape_cmds_per_page[i]);
                    layout.commands.push(DrawCommand::Float(FloatMark::End));
                }
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

/// One floating text shape's body as page-absolute [`DrawCommand`]s.
///
/// This is the *paint* half of a shape, and it is a real §20.1.10.60
/// sub-layout — the same [`layout_shape_body`] the PPTX shape and DOCX
/// textbox paths use, so insets, measured wrapping, `a:pPr/@algn`, the
/// anchor and `@vertOverflow` all apply. The `TextItem`s beside it stay the
/// even stack `shape_text_items` builds, and the two deliberately disagree
/// about where a *line* falls inside the box.
///
/// That split is the same one the cell path already makes — an item is the
/// cell box, the painted glyphs are measured inside it — and it is what keeps
/// this module's host-independence claim intact: items come from declared
/// geometry and need no registry, paint measures and does. Feeding items from
/// the layout instead would make a plain parse depend on the host's fonts for
/// 154 corpus workbooks, which is a far larger price than the disagreement.
///
/// The layout census (1,637 shapes) is what bought the sub-layout rather than
/// painting the stack: 61.5% anchor non-top, 34.8% of paragraphs wrap (2.29x
/// the line count the stack draws), 51.1% declare an alignment the stack
/// cannot express, and the first line moves by more than 24pt on 19.2% of
/// shapes.
///
/// Run colour and theme faces resolve against the workbook's full `Theme` —
/// the shape-visuals census measured 5,041 of 5,902 runs declaring a colour,
/// 52% of them `schemeClr`, which is what made keeping the whole theme (not
/// just its colour scheme) worth it.
fn shape_commands(
    shape: &SheetShape,
    rect: &Rect,
    theme: Option<&liteparse_ooxml::model::Theme>,
    m: &TextMeasurer,
) -> Vec<DrawCommand> {
    use liteparse_ooxml::model::BodyProperties;
    use liteparse_ooxml::model::dimension::Dimension;
    use liteparse_ooxml::pptx::textcascade::TextCascade;
    use liteparse_ooxml::render::layout::ShapeAutoFit;
    use liteparse_ooxml::render::layout::section::LayoutBlock;
    use liteparse_ooxml::render::layout::shape_body::{BodyInsets, layout_shape_body};

    let body_pr = shape.body.body_pr.as_ref();
    let auto_fit = ShapeAutoFit::from_body(body_pr.and_then(|b| b.auto_fit));
    // Rung 3 only. A sheet shape has no layout, master or deck above it, so
    // the cascade's other rungs are genuinely absent rather than unwired —
    // and rung 2 (the paragraph's own `a:pPr`) plus the spec default is what
    // `resolve` seeds itself with.
    let cascade = TextCascade {
        shape: Some(&shape.body.list_style),
        layout: None,
        master: None,
        deck: None,
    };

    let mut blocks: Vec<LayoutBlock> = Vec::with_capacity(shape.body.paragraphs.len());
    let mut line_height = Pt::new(DEFAULT_FONT_SIZE * 1.2);
    for (i, para) in shape.body.paragraphs.iter().enumerate() {
        let resolved = cascade.resolve(&para.properties, None);
        let mut fragments = Vec::new();
        shape_fragments(&para.content, &resolved, auto_fit, theme, m, &mut fragments);
        if i == 0 {
            // The fallback height for content that states none — an empty
            // paragraph between two filled ones still occupies a line.
            let size = auto_fit.scale_font(
                resolved
                    .run_defaults
                    .font_size
                    .map(Pt::from)
                    .unwrap_or(Pt::new(DEFAULT_FONT_SIZE)),
            );
            line_height = m.default_line_height(DEFAULT_FONT_FAMILY, size);
        }
        blocks.push(LayoutBlock::Paragraph {
            fragments,
            style: super::pptx_layout::paragraph_style(&resolved, auto_fit),
            page_break_before: false,
            footnotes: Vec::new(),
            floating_images: Vec::new(),
            floating_shapes: Vec::new(),
        });
    }

    // The insets cliff. `layout_shape_body` returns *nothing* when the left
    // and right insets leave no width to wrap in, and the §20.1.2.1.1
    // defaults are 7.2pt a side — so every shape narrower than 14.4pt paints
    // empty. That is **6.5% of corpus shapes (106)**, and they are not
    // decorative: they are the narrow vertical marker labels a timeline is
    // made of. Excel draws them, overflowing the box rather than vanishing,
    // so the horizontal insets are dropped for exactly those shapes. The
    // clamp lives here rather than in the shared sub-layout because it is a
    // statement about this producer's boxes, not about §20.1.10.60.
    let extent = PtSize::new(Pt::new(rect.width), Pt::new(rect.height));
    let squeezed = BodyInsets::resolve(body_pr).content_width(extent) <= Pt::ZERO;
    let unsqueezed;
    let body_pr = if squeezed {
        let mut bp = body_pr.cloned().unwrap_or(BodyProperties {
            rotation: None,
            vert: None,
            wrap: None,
            left_inset: None,
            top_inset: None,
            right_inset: None,
            bottom_inset: None,
            anchor: None,
            vert_overflow: None,
            auto_fit: None,
        });
        bp.left_inset = Some(Dimension::new(0));
        bp.right_inset = Some(Dimension::new(0));
        unsqueezed = bp;
        Some(&unsqueezed)
    } else {
        body_pr
    };
    let mut commands = layout_shape_body(&blocks, extent, body_pr, line_height);
    // Shape-local → page-absolute. `place_pic` already returns the rect in
    // the page's own point space, the same space the picture commands use.
    for cmd in &mut commands {
        cmd.shift(Pt::new(rect.x), Pt::new(rect.y));
    }
    commands
}

/// Maps a box in the drawing's declared EMU space onto the anchor's page
/// rect: the top-level object's own declared box stands for the whole
/// anchor, and every descendant scales through it. Fraction-based rather
/// than EMU→pt conversion because the two spaces genuinely disagree — the
/// anchor is computed from cells and column widths, the `a:xfrm` is whatever
/// the producer wrote — and the anchor is the authority on where the object
/// sits on the page.
struct AnchorMap {
    base_x: f64,
    base_y: f64,
    /// Scale from declared EMU to page pt, per axis. Non-uniform when the
    /// anchor's aspect differs from the declared box's — under which a
    /// rotated child's angle is carried unscaled, the closest similarity.
    sx: f64,
    sy: f64,
    to_x: f64,
    to_y: f64,
}

impl AnchorMap {
    fn new(
        base: liteparse_ooxml::model::geometry::Rect<liteparse_ooxml::model::dimension::Emu>,
        target: &Rect,
    ) -> Option<Self> {
        let (bw, bh) = (base.size.width.raw() as f64, base.size.height.raw() as f64);
        // A box that is zero in *both* axes is a point — 540 corpus shapes,
        // one workbook, all legacy `Line NNN` leftovers — and a point has no
        // ink. A box that is zero in *one* axis is a flat line (every
        // horizontal connector declares `cy="0"`), and Excel draws those:
        // the degenerate axis collapses (scale 0) instead of vetoing the
        // whole tree.
        if bw <= 0.0 && bh <= 0.0 {
            return None;
        }
        Some(Self {
            base_x: base.origin.x.raw() as f64,
            base_y: base.origin.y.raw() as f64,
            sx: if bw > 0.0 {
                target.width as f64 / bw
            } else {
                0.0
            },
            sy: if bh > 0.0 {
                target.height as f64 / bh
            } else {
                0.0
            },
            to_x: target.x as f64,
            to_y: target.y as f64,
        })
    }

    fn map(
        &self,
        r: liteparse_ooxml::model::geometry::Rect<liteparse_ooxml::model::dimension::Emu>,
    ) -> (f64, f64, f64, f64) {
        (
            self.to_x + (r.origin.x.raw() as f64 - self.base_x) * self.sx,
            self.to_y + (r.origin.y.raw() as f64 - self.base_y) * self.sy,
            r.size.width.raw() as f64 * self.sx,
            r.size.height.raw() as f64 * self.sy,
        )
    }
}

/// One anchored drawing object's fills and outlines as self-placing
/// [`DrawCommand::Path`]s — the XLSX twin of the PPTX paint walk, over the
/// same shape tree, the same `resolve_shape_visuals`, and the same vendored
/// preset geometry.
///
/// What it deliberately shares with the PPTX painter's approximations: all
/// of one page's fills paint before any of its shape text, so an overlapping
/// shape's fill does not cover a neighbour's words; a picture *inside a
/// group* is not painted here (it paints via the float pass, at the composed
/// box the reader derived from this same tree — see [`SheetPic::frac`]); an
/// unbuildable preset is skipped, never approximated by its bounding box.
fn ink_commands(
    ink: &liteparse_ooxml::xlsx::SheetInk,
    rect: &Rect,
    theme: Option<&liteparse_ooxml::model::Theme>,
    out: &mut Vec<DrawCommand>,
) {
    use liteparse_ooxml::pptx::apply_slide_geometry;

    let Some(mut root) = ink.shape() else { return };
    apply_slide_geometry(std::slice::from_mut(&mut root));
    let ctx = DrawingColorContext::new(theme);
    match root.slide_rect {
        Some(sr) => {
            // The anchor covers the object as *seen* — its rotated bounding
            // box, not its unrotated rect. 88 corpus shapes are vertical
            // lines rotated a quarter turn into horizontal ones, whose
            // unrotated rect is zero-width; scaling through it collapses
            // them, scaling through the bounding box does not.
            let Some(map) = AnchorMap::new(sr.bounding_box(), rect) else {
                return;
            };
            walk_ink(&root, &map, &ctx, None, out);
        }
        // No transform on the top-level object: its declared box *is* the
        // anchor's, unrotated. Only a leaf can be placed this way — a
        // group with no box gives its children nothing to map through.
        None => {
            let placed = PlacedBox {
                x: rect.x as f64,
                y: rect.y as f64,
                w: rect.width as f64,
                h: rect.height as f64,
                rotation: liteparse_ooxml::model::dimension::Dimension::new(0),
                flip_h: false,
                flip_v: false,
            };
            paint_ink_shape(&root, placed, &ctx, None, out);
        }
    }
}

/// A shape's box after anchor mapping, in page points.
struct PlacedBox {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    rotation: liteparse_ooxml::model::dimension::Dimension<
        liteparse_ooxml::model::dimension::SixtieThousandthDeg,
    >,
    flip_h: bool,
    flip_v: bool,
}

fn walk_ink(
    shape: &liteparse_ooxml::pptx::Shape,
    map: &AnchorMap,
    ctx: &DrawingColorContext<'_>,
    group_fill: Option<&liteparse_ooxml::render::layout::draw_command::ResolvedFill>,
    out: &mut Vec<DrawCommand>,
) {
    use liteparse_ooxml::model::DrawingFill;
    use liteparse_ooxml::pptx::ShapeKind;
    use liteparse_ooxml::render::resolve::shape_visuals::resolve_fill;

    // Hidden is the legacy-form-control filter the text channel already
    // applies; the paint channel honours the same bit.
    if shape.non_visual.hidden == Some(true) {
        return;
    }
    match &shape.kind {
        ShapeKind::Group(group) => {
            // §20.1.8.35: a child's `a:grpFill` inherits the nearest
            // enclosing group's fill, resolved once in the group's own
            // context. A group that itself declares `grpFill` (or nothing)
            // passes its parent's answer through.
            let resolved: Option<liteparse_ooxml::render::layout::draw_command::ResolvedFill> =
                match &group.fill {
                    Some(DrawingFill::Group) | None => None,
                    Some(f) => Some(resolve_fill(f, ctx, None, group_fill)),
                };
            let next = resolved.as_ref().or(group_fill);
            for child in &group.children {
                walk_ink(child, map, ctx, next, out);
            }
        }
        // A grouped picture already paints as a float image at the anchor
        // box; painting its frame here would draw it twice. Frames (charts)
        // carry no ink this pass can build.
        ShapeKind::Picture(_) | ShapeKind::GraphicFrame(_) => {}
        ShapeKind::AutoShape(_) | ShapeKind::Connector(_) => {
            // A shape the geometry pass could not place has no box to paint
            // into; skipped rather than guessed.
            let Some(sr) = shape.slide_rect else { return };
            // The *bounding* box maps — it is the on-page footprint the
            // anchor's axis-aligned scale meaningfully applies to. The
            // painter then needs the pre-rotation extent back, because
            // `DrawCommand::Path` rotates its box about the centre: for a
            // quarter turn that is the bounding box swapped in place; for
            // an oblique angle the bounding box itself is the closest
            // axis-aligned stand-in (the PPTX painter's approximation).
            let (bx, by, bw, bh) = map.map(sr.bounding_box());
            let rot = sr.rotation.raw().rem_euclid(21_600_000);
            let quarter = rot == 5_400_000 || rot == 16_200_000;
            let placed = if quarter {
                let (cx, cy) = (bx + bw / 2.0, by + bh / 2.0);
                PlacedBox {
                    x: cx - bh / 2.0,
                    y: cy - bw / 2.0,
                    w: bh,
                    h: bw,
                    rotation: sr.rotation,
                    flip_h: sr.flip_h,
                    flip_v: sr.flip_v,
                }
            } else {
                PlacedBox {
                    x: bx,
                    y: by,
                    w: bw,
                    h: bh,
                    rotation: sr.rotation,
                    flip_h: sr.flip_h,
                    flip_v: sr.flip_v,
                }
            };
            paint_ink_shape(shape, placed, ctx, group_fill, out);
        }
    }
}

/// One shape's fill and outline as a [`DrawCommand::Path`], if it puts ink
/// on the page — the paint gate, the style-ref fallback and the
/// no-approximation rule all mirror the PPTX `paint_shape`.
fn paint_ink_shape(
    shape: &liteparse_ooxml::pptx::Shape,
    placed: PlacedBox,
    ctx: &DrawingColorContext<'_>,
    group_fill: Option<&liteparse_ooxml::render::layout::draw_command::ResolvedFill>,
    out: &mut Vec<DrawCommand>,
) {
    use liteparse_ooxml::pptx::ShapeKind;
    use liteparse_ooxml::render::layout::draw_command::ResolvedFill;
    use liteparse_ooxml::render::resolve::shape_geometry::build_geometry;
    use liteparse_ooxml::render::resolve::shape_visuals::resolve_shape_visuals;

    // Checked here as well as in the walk: the no-`xfrm` fallback reaches
    // this function without passing through `walk_ink`'s filter.
    if shape.non_visual.hidden == Some(true) {
        return;
    }
    let props = match &shape.kind {
        ShapeKind::AutoShape(a) => a.properties.as_ref(),
        ShapeKind::Connector(c) => c.properties.as_ref(),
        _ => return,
    };
    let Some(props) = props else { return };
    let style = shape.style.as_ref();
    let visuals = resolve_shape_visuals(
        Some(props),
        style.and_then(|s| s.line_ref.as_ref()),
        style.and_then(|s| s.effect_ref.as_ref()),
        style.and_then(|s| s.fill_ref.as_ref()),
        ctx,
        // No `PartMedia`: a blip fill on an XLSX shape (4 on the corpus)
        // resolves to nothing and the shape paints its outline alone.
        None,
        group_fill,
    );
    if matches!(visuals.fill, ResolvedFill::None) && visuals.stroke.is_none() {
        return;
    }
    // A point paints nothing — the both-axes-degenerate case `AnchorMap`
    // already rejects at the root, restated here for grouped children.
    if placed.w <= 0.0 && placed.h <= 0.0 {
        return;
    }
    let extent = PtSize::new(Pt::new(placed.w as f32), Pt::new(placed.h as f32));
    let Some(geometry) = props.geometry.as_ref() else {
        return;
    };
    // An unimplemented preset is skipped, never approximated by its bounding
    // box: a `rect` drawn where a `cloud` was asked for is a wrong page, not
    // a coarse one.
    let Some(path) = build_geometry(geometry, extent) else {
        return;
    };
    out.push(DrawCommand::Path {
        origin: PtOffset::new(Pt::new(placed.x as f32), Pt::new(placed.y as f32)),
        rotation: placed.rotation,
        flip_h: placed.flip_h,
        flip_v: placed.flip_v,
        extent,
        paths: path.paths,
        fill: visuals.fill,
        stroke: visuals.stroke,
        effects: visuals.effects,
    });
}

/// A shape paragraph's runs as measured [`Fragment`]s.
///
/// The XLSX-local twin of the PPTX `collect_run_fragments`: same shared
/// word-splitter and measurer, minus the theme-font resolution and the colour
/// cascade a slide has and a sheet does not.
fn shape_fragments(
    inlines: &[liteparse_ooxml::model::Inline],
    resolved: &liteparse_ooxml::pptx::textcascade::ResolvedTextStyle,
    auto_fit: liteparse_ooxml::render::layout::ShapeAutoFit,
    theme: Option<&liteparse_ooxml::model::Theme>,
    m: &TextMeasurer,
    out: &mut Vec<liteparse_ooxml::render::layout::fragment::Fragment>,
) {
    use liteparse_ooxml::model::{Inline, RunElement};
    use liteparse_ooxml::render::layout::fragment::{
        Fragment, emit_run_fragments, font_props_from_run,
    };

    for inline in inlines {
        match inline {
            Inline::TextRun(run) => {
                let mut props = run.properties.clone();
                resolved.apply_to_run(&mut props);
                // A run naming `+mn-lt` carries a `ThemeFontRef` the measurer
                // cannot use; resolve it now the workbook's theme is in hand.
                if let Some(theme) = theme {
                    resolve_font_set_themes(&mut props.fonts, theme);
                }
                let font = font_props_from_run(
                    &props,
                    DEFAULT_FONT_FAMILY,
                    Pt::new(DEFAULT_FONT_SIZE),
                    auto_fit,
                );
                let color = shape_run_color(&props, theme);
                for element in &run.content {
                    match element {
                        RunElement::Text(text) => {
                            emit_run_fragments(text, &font, color, None, m, out)
                        }
                        RunElement::Tab => out.push(Fragment::Tab {
                            line_height: font.size,
                            font: Rc::new(font.clone()),
                            color,
                            fitting_width: None,
                        }),
                        RunElement::LineBreak(_) => out.push(Fragment::LineBreak {
                            line_height: font.size,
                        }),
                        _ => {}
                    }
                }
            }
            Inline::Hyperlink(link) => {
                shape_fragments(&link.content, resolved, auto_fit, theme, m, out);
            }
            _ => {}
        }
    }
}

/// A shape run's declared colour, resolved against the workbook's theme —
/// the XLSX twin of the PPTX path's `run_color`, minus the `p:clrMap` (a
/// workbook has no master to remap `bg1`/`tx1` through) and minus the
/// counters (the corpus census already sized both branches: 5,041 of 5,902
/// runs declare, 52% of them `schemeClr`).
///
/// Alpha is dropped, as it is on the PPTX side: `DrawCommand::Text` carries
/// an opaque colour, and solid is a smaller error than the black it
/// replaces. No declaration is the §21.1.2.3 black default, not a guess.
fn shape_run_color(
    props: &liteparse_ooxml::model::RunProperties,
    theme: Option<&liteparse_ooxml::model::Theme>,
) -> RgbColor {
    match props.drawing_color.as_ref() {
        Some(declared) => rgb_from_u32(
            resolve_drawing_color(declared, &DrawingColorContext::new(theme)).to_rgb24(),
        ),
        None => SHAPE_TEXT_COLOR,
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

/// [`Workbook::fill_split`] rebased onto the trimmed string the painter draws.
///
/// The format's own padding — `_(`'s skip-widths — is part of `display_text`
/// and is trimmed off before painting, which moves the split by however much
/// came off the front. A split that the trim swallowed entirely (the repeat
/// was in the padding) is no split at all.
fn fill_split_of(
    wb: &Workbook,
    cell: &liteparse_ooxml::xlsx::Cell,
    trimmed: &str,
) -> Option<usize> {
    let at = wb.fill_split(cell)?;
    let full = wb.display_text(cell)?;
    let lead = full.len() - full.trim_start().len();
    let at = at.checked_sub(lead)?;
    (at > 0 && at < trimmed.len()).then_some(at)
}

/// The one colour a cell's fill paints, or `None` when it paints nothing.
///
/// A solid fill is its `fgColor`. A hatch has no pattern engine behind it and
/// does not need one: at 150 DPI an 8×8 Excel pattern tile is under 4 px
/// across, so the tile averages to `fg` blended over `bg` by the pattern's
/// coverage, and that blend is what gets painted. `bg` defaults to the page
/// the cell sits on, which is white.
///
/// Blending is also what makes a hatch safe to paint at all: slot 1 of every
/// styles part is `gray125` with a black `fg` — Excel's "no fill" placeholder
/// — and at 12.5% coverage it lands as the light grey Excel shows rather than
/// the black a solid reading would give it. 323 corpus cells are hatched, 210
/// of them `gray0625`.
fn fill_color(wb: &Workbook, fill: &liteparse_ooxml::xlsx::Fill) -> Option<RgbColor> {
    let fg = fill.fg.and_then(|c| wb.resolve_color(c))?;
    match fill.pattern {
        PatternType::Solid => Some(rgb_of(fg)),
        PatternType::Hatch(h) => {
            let bg = fill
                .bg
                .and_then(|c| wb.resolve_color(c))
                .unwrap_or([0xff, 0xff, 0xff]);
            let k = h.coverage();
            let mix = |f: u8, b: u8| (f32::from(f) * k + f32::from(b) * (1.0 - k)).round() as u8;
            Some(RgbColor {
                r: mix(fg[0], bg[0]),
                g: mix(fg[1], bg[1]),
                b: mix(fg[2], bg[2]),
            })
        }
        PatternType::None | PatternType::Gradient => None,
    }
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
    // The pen, and the band the edge occupies. They differ only for `double`,
    // which lays two pens and a gap inside its band.
    let pen = |e: &BorderEdge| {
        f64::from(e.style.width_pt())
            .min(h.max(0.0))
            .min(w.max(0.0))
    };
    let band = |e: &BorderEdge| {
        f64::from(e.style.extent_pt())
            .min(h.max(0.0))
            .min(w.max(0.0))
    };
    let (top_b, bot_b) = (band(&border.top), band(&border.bottom));
    if border.top.style.paints() {
        push_edge(out, &border.top, x, y, w, pen(&border.top), true, &color);
    }
    if border.bottom.style.paints() {
        let p = pen(&border.bottom);
        push_edge(out, &border.bottom, x, y + h - bot_b, w, p, true, &color);
    }
    let top_inset = if border.top.style.paints() {
        top_b
    } else {
        0.0
    };
    let bot_inset = if border.bottom.style.paints() {
        bot_b
    } else {
        0.0
    };
    let v_h = h - top_inset - bot_inset;
    if v_h <= 0.0 {
        return;
    }
    if border.left.style.paints() {
        let p = pen(&border.left);
        push_edge(out, &border.left, x, y + top_inset, v_h, p, false, &color);
    }
    if border.right.style.paints() {
        let (p, b) = (pen(&border.right), band(&border.right));
        push_edge(
            out,
            &border.right,
            x + w - b,
            y + top_inset,
            v_h,
            p,
            false,
            &color,
        );
    }
}

/// One edge, expanded into the rects its style asks for.
///
/// `(x, y)` is the band's top-left corner, `len` its length along the edge and
/// `pen` one stroke's thickness; `horizontal` says which way the edge runs.
/// Three shapes come out of here:
///
/// * a continuous edge is one rect — the shape every edge had before;
/// * a `double` edge is two pens with a pen-wide gap, filling the band
///   [`BorderStyle::extent_pt`] reserves for it. 27,300 corpus edges, more
///   than every broken style put together;
/// * a broken edge is one rect per "on" run of
///   [`BorderStyle::dash_pattern`], the last clipped to the cell rather than
///   allowed to overhang it. 14,094 corpus edges.
///
/// The dash phase restarts at each cell, so a row of dashed cells shows a
/// stroke at every boundary rather than one pattern running across it. Excel
/// phases per cell too.
#[allow(clippy::too_many_arguments)]
fn push_edge(
    out: &mut Vec<DrawCommand>,
    edge: &BorderEdge,
    x: f64,
    y: f64,
    len: f64,
    pen: f64,
    horizontal: bool,
    color: &impl Fn(&BorderEdge) -> RgbColor,
) {
    let c = color(edge);
    // One stroke: `at` runs along the edge, `off` across it.
    //
    // A zero-*length* run still paints, and the corpus insists on it: a hidden
    // column is a retained column of zero width, and its cells' horizontal
    // edges were zero-area rects before this walk existed. Dropping them looks
    // like a tidy-up and is a silent 1,230-rect change on one corpus sheet.
    // Only a negative run — which the dash walk cannot produce — is skipped.
    let mut stroke = |at: f64, run: f64, off: f64| {
        if run < 0.0 {
            return;
        }
        if horizontal {
            out.push(rect_cmd(x + at, y + off, run, pen, c));
        } else {
            out.push(rect_cmd(x + off, y + at, pen, run, c));
        }
    };
    let offsets: &[f64] = if edge.style == BorderStyle::Double {
        &[0.0, 2.0]
    } else {
        &[0.0]
    };
    for &o in offsets {
        let off = o * pen;
        match edge.style.dash_pattern() {
            None => stroke(0.0, len, off),
            Some(pattern) => {
                // A zero pen would never advance the walk. The table writes no
                // zero-length run, but the loop must not depend on that.
                let step = pen.max(0.01);
                let (mut at, mut i) = (0.0, 0usize);
                while at < len {
                    let run = f64::from(pattern[i % pattern.len()]) * step;
                    if i % 2 == 0 {
                        stroke(at, run.min(len - at), off);
                    }
                    at += run;
                    i += 1;
                }
            }
        }
    }
}

/// The cell's diagonal, as one stroked line per declared direction.
///
/// A `Line` rather than a `Rect` because a rect cannot express a diagonal —
/// and the recorded objection to `Line`, that it "paints in the Ink pass, over
/// everything", is only half right. It does paint over the fills and borders,
/// which is where a diagonal belongs; and it paints *under* the cell's text so
/// long as it is emitted before it, which the paint walk does. The whole class
/// is **2 corpus edges in 1 workbook**, and it is here because the note
/// explaining its absence was longer than the code.
fn push_diagonal(
    out: &mut Vec<DrawCommand>,
    wb: &Workbook,
    border: &Border,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) {
    if !border.diagonal.style.paints() || !(border.diagonal_up || border.diagonal_down) {
        return;
    }
    let color = border
        .diagonal
        .color
        .and_then(|c| wb.resolve_color(c))
        .map_or(RgbColor::BLACK, rgb_of);
    let width = Pt::new(border.diagonal.style.width_pt());
    let pt = |px: f64, py: f64| PtOffset {
        x: Pt::new(MARGIN + px as f32),
        y: Pt::new(MARGIN + py as f32),
    };
    let mut line = |start, end| {
        out.push(DrawCommand::Line {
            line: PtLineSegment { start, end },
            color,
            width,
        });
    };
    // `diagonalUp` runs bottom-left to top-right, `diagonalDown` the other
    // way; both flags at once is Excel's cross and draws both.
    if border.diagonal_up {
        line(pt(x, y + h), pt(x + w, y));
    }
    if border.diagonal_down {
        line(pt(x, y), pt(x + w, y + h));
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
    /// Cells painted in two pieces because their format asked for a `*` fill
    /// between them — Excel's Accounting shape.
    pub filled: u64,
    /// Cells whose text was laid out along an angle and placed by a
    /// [`TransformMark`] bracket — §18.8.1's `1..=180` spellings.
    pub rotated: u64,
    /// Cells painted one glyph under the next, `@textRotation="255"`. Not a
    /// rotation: the glyphs stay upright, so these carry no bracket.
    pub stacked: u64,
    /// Cells cut across the direction their lines stack rather than along
    /// them: a stacked cell with more glyphs than its row is tall, or a
    /// rotated one whose wrapped lines ran past the cell's cross extent. The
    /// cross-axis twin of `clipped`, kept apart from it so the horizontal
    /// rule's gated rate stays a statement about the horizontal rule.
    pub cross_clipped: u64,
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
        self.filled += o.filled;
        self.rotated += o.rotated;
        self.stacked += o.stacked;
        self.cross_clipped += o.cross_clipped;
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
/// One line of a cell's text, placed on its baseline in page coordinates.
fn text_cmd(line: &str, fp: &FontProps, color: RgbColor, x: f64, baseline: f64) -> DrawCommand {
    DrawCommand::Text {
        position: PtOffset {
            x: Pt::new(MARGIN + x as f32),
            y: Pt::new(MARGIN + baseline as f32),
        },
        text: Rc::from(line),
        font_family: Rc::clone(&fp.family),
        char_spacing: Pt::ZERO,
        font_size: fp.size,
        bold: fp.bold,
        italic: fp.italic,
        color,
        text_scale: 1.0,
    }
}

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
    fill_split: Option<usize>,
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

    // §18.8.31's `*c` fill, honoured now that there is a box to fill: the part
    // before the repeat goes against the cell's left edge and the part after
    // it against the right, which is how Excel's Accounting format puts the
    // currency symbol in one corner and the number in the other. Only for a
    // single line that fits — a wrapped, hashed or clipped cell has already
    // been fitted by the rules above, and re-splitting it would undo them.
    if let (Some(at), 1) = (fill_split, lines.len())
        && width_of(m, &fp, &lines[0]) <= avail
        && at < lines[0].len()
    {
        let (left, right) = lines[0].split_at(at);
        let (lw, rw) = (width_of(m, &fp, left), width_of(m, &fp, right));
        if lw + rw <= avail {
            let mut push = |s: &str, x: f64| {
                if !s.is_empty() {
                    out.push(text_cmd(s, &fp, color, x, first_baseline));
                }
            };
            push(left, x0);
            push(right, x0 + avail - rw);
            stats.filled += 1;
            stats.cells += 1;
            return stats;
        }
    }

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
        out.push(text_cmd(
            line,
            &fp,
            color,
            x,
            first_baseline + li as f64 * line_h,
        ));
    }
    if lines.iter().all(String::is_empty) {
        stats.blank += 1;
    } else {
        stats.cells += 1;
    }
    stats
}

/// §18.8.1 `@textRotation`, as the three cases a painter has to tell apart.
///
/// One attribute with three meanings: `1..=90` is degrees **counter-clockwise**,
/// `91..=180` is `value - 90` degrees **clockwise**, and `255` is not a
/// rotation at all — it is Excel's *stacked* text, upright glyphs one under the
/// next. Paint census, 1,247 workbooks / 1,902 rotated cells, and the corpus
/// declares only five values: 951 at `90`, 40 at `180`, 184 at `60`, 48 at
/// `45`, 679 stacked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum CellRotation {
    /// No rotation, and 99.994% of text cells.
    None,
    /// Degrees counter-clockwise from the cell's own baseline; negative for
    /// the `91..=180` clockwise spellings.
    Angled(f64),
    /// One glyph per line, unrotated.
    Stacked,
}

fn cell_rotation(align: &Alignment) -> CellRotation {
    match align.text_rotation {
        Some(255) => CellRotation::Stacked,
        // `0` never reaches here — the reader filters it — but a rotation of
        // zero degrees is the unrotated case either way.
        Some(r @ 1..=90) => CellRotation::Angled(f64::from(r)),
        Some(r @ 91..=180) => CellRotation::Angled(-f64::from(r - 90)),
        // Out of §18.8.1's range: paint it flat rather than guess.
        _ => CellRotation::None,
    }
}

/// The longest run of text the cell can hold at angle θ, in the text's own
/// frame.
///
/// A `W × H` box laid along θ covers `W·|cos θ| + H·|sin θ|` of the cell's
/// width and `W·|sin θ| + H·|cos θ|` of its height, so a quarter turn may run
/// the cell's whole *height* and an oblique angle gets whichever of the two
/// constraints binds first. `H` is one line, which is what the corpus's angled
/// cells are: a rotated header is a single string, and wrapping and *then*
/// rotating would need the line count before the length that decides it.
fn rotated_run_length(theta_deg: f64, b: &CellBox, line_h: f64) -> f64 {
    let (s, c) = theta_deg.to_radians().sin_cos();
    let (s, c) = (s.abs(), c.abs());
    if c < 1e-9 {
        return b.h;
    }
    let h = line_h.min(b.h);
    ((b.w - h * s) / c)
        .min((b.h - h * c) / s)
        .max(0.0)
        .min(b.w + b.h)
}

/// The block a run of laid-out commands occupies, in their own space:
/// `(x, y, w, h)`, with the vertical edges taken from the font rather than
/// from the baselines the commands carry.
fn block_bounds(
    m: &TextMeasurer,
    cmds: &[DrawCommand],
    fp: &FontProps,
    ascent: f64,
    descent: f64,
) -> Option<(f64, f64, f64, f64)> {
    let (mut x0, mut x1) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut y0, mut y1) = (f64::INFINITY, f64::NEG_INFINITY);
    for cmd in cmds {
        let DrawCommand::Text { position, text, .. } = cmd else {
            continue;
        };
        let (x, base) = (f64::from(position.x.raw()), f64::from(position.y.raw()));
        x0 = x0.min(x);
        x1 = x1.max(x + width_of(m, fp, text));
        y0 = y0.min(base - ascent);
        y1 = y1.max(base + descent);
    }
    (x0.is_finite()).then_some((x0, y0, x1 - x0, y1 - y0))
}

/// One rotated cell's text: laid out flush along its own baseline, then placed
/// in the cell **by the page-axis alignment Excel keeps for it** and turned by
/// a [`TransformMark`] bracket.
///
/// A bracket rather than a second painter, for two reasons that are really one:
/// [`paint_cell_text`]'s rules (wrap, clip, `#######`) are stated in a box and
/// do not care which way the box points, and the bracket is the only place a
/// page coordinate is introduced — so the local commands come back out of the
/// shared painter with [`MARGIN`] already in them and are shifted off it, which
/// is cheaper than teaching every `text_cmd` call site a second origin.
///
/// **Alignment does not rotate with the text, and that is measured rather than
/// assumed.** The obvious model — the alignment frame turns with the glyphs, so
/// a 90° cell's `horizontal` runs up the column — is wrong: rendering a probe
/// workbook of every rotation × `vertical` × `horizontal` combination through
/// LibreOffice puts `horizontal` on the page's x axis and `vertical` on its y
/// axis at every angle, quarter turn included. So the text is laid out flush,
/// its finished block is measured, and *the block's upright bounding box* is
/// what the two alignments place inside the cell's inset rect. Centre-on-centre
/// then does the rest: `place_transform` turns the local box about its own
/// centre before translating, so putting the local centre on the bounding box's
/// centre lands the rotated block exactly where the alignment put it, at any
/// angle and with no case split per quadrant.
///
/// **A rotated cell does not spill**: Excel keeps angled text inside its own
/// cell, so the room the flat path would grow into is passed as zero and the
/// run clips against the length the cell can hold instead.
#[allow(clippy::too_many_arguments)]
fn paint_rotated_cell_text(
    out: &mut Vec<DrawCommand>,
    m: &TextMeasurer,
    theta_deg: f64,
    text: &str,
    numeric: bool,
    font: &liteparse_ooxml::xlsx::Font,
    align: &Alignment,
    color: RgbColor,
    b: &CellBox,
    y: f64,
) -> TextPaintStats {
    let fp = font_props(font);
    let (_, metrics) = m.measure("", &fp);
    let (ascent, descent) = (
        f64::from(f32::from(metrics.ascent)),
        f64::from(f32::from(metrics.descent)),
    );
    let line_h = ascent + descent + f64::from(f32::from(metrics.leading));
    let inset = f64::from(TEXT_INSET);

    // Flush along the baseline and at the top of the cross axis: the finished
    // block is what carries the cell's alignment, so applying it twice would
    // double it.
    let flush = Alignment {
        horizontal: HorizontalAlign::Left,
        vertical: VerticalAlign::Top,
        ..align.clone()
    };
    let local_box = CellBox {
        x: 0.0,
        w: rotated_run_length(theta_deg, b, line_h) + 2.0 * inset,
        h: b.w + b.h,
        cols: b.cols.clone(),
    };
    let mut local = Vec::new();
    let mut stats = paint_cell_text(
        &mut local,
        m,
        text,
        numeric,
        font,
        &flush,
        color,
        &local_box,
        0.0,
        (0.0, 0.0),
        false,
        None,
    );
    let Some((bx, by, bw, bh)) = block_bounds(m, &local, &fp, ascent, descent) else {
        return stats;
    };

    // The cross extent the cell can hold once the block's own length is
    // spoken for — the same two constraints `rotated_run_length` solves, read
    // the other way round. A wrapped cell is the only one that can exceed it,
    // and a quarter-turned one that did would run its lines out across the
    // whole sheet rather than down its own column: 3 corpus cells wrap on an
    // 8 pt column. Lines past it are cut, which is the cross-axis twin of the
    // clip the flat path applies along the baseline.
    let (s, c) = theta_deg.to_radians().sin_cos();
    let (s, c) = (s.abs(), c.abs());
    let cross_max = if c < 1e-9 {
        b.w - 2.0 * inset
    } else {
        ((b.w - bw * c) / s).min((b.h - bw * s) / c)
    }
    .max(line_h);
    let (bw, bh) = if bh > cross_max {
        local.retain(|cmd| match cmd {
            DrawCommand::Text { position, .. } => {
                f64::from(position.y.raw()) + descent - by <= cross_max
            }
            _ => true,
        });
        stats.cross_clipped += 1;
        match block_bounds(m, &local, &fp, ascent, descent) {
            Some((_, _, w, h)) => (w, h),
            None => return stats,
        }
    } else {
        (bw, bh)
    };

    // The block's upright bounding box once turned, and where the two
    // alignments put it inside the cell's inset rect. A block too big for the
    // cell hangs out of the near edge rather than being centred on the
    // overflow, which is Excel's behaviour and the flat path's.
    let (rw, rh) = (bw * c + bh * s, bw * s + bh * c);
    let slack_x = (b.w - 2.0 * inset - rw).max(0.0);
    let slack_y = (b.h - 2.0 * inset - rh).max(0.0);
    let dx = match align.horizontal {
        HorizontalAlign::Center | HorizontalAlign::CenterContinuous => slack_x / 2.0,
        HorizontalAlign::Right => slack_x,
        HorizontalAlign::General if numeric => slack_x,
        _ => 0.0,
    };
    let dy = match align.vertical {
        VerticalAlign::Top => 0.0,
        VerticalAlign::Center => slack_y / 2.0,
        _ => slack_y,
    };
    // Centre of the turned block on the page, and the local box's own centre
    // put on top of it.
    let (cx, cy) = (b.x + inset + dx + rw / 2.0, y + inset + dy + rh / 2.0);

    // The block's own offset comes off, leaving commands whose top-left is the
    // local origin. `bx`/`by` are measured on the emitted commands, so the
    // page `MARGIN` the shared painter bakes into every one of them is already
    // inside them and comes off with the rest.
    for cmd in &mut local {
        cmd.shift(Pt::new(-(bx as f32)), Pt::new(-(by as f32)));
    }
    out.push(DrawCommand::Transform(TransformMark::Begin(
        ShapeTransform {
            origin: PtOffset::new(
                Pt::new(MARGIN + (cx - bw / 2.0) as f32),
                Pt::new(MARGIN + (cy - bh / 2.0) as f32),
            ),
            // The bracket's angle is clockwise-positive (§20.1.10.3, and what
            // the rasterizer's `from_rotate_at` takes); `theta_deg` is
            // counter-clockwise, which is §18.8.1's.
            rotation: Dimension::new((-theta_deg * 60_000.0).round() as i64),
            flip_h: false,
            flip_v: false,
            extent: PtSize::new(Pt::new(bw as f32), Pt::new(bh as f32)),
        },
    )));
    out.append(&mut local);
    out.push(DrawCommand::Transform(TransformMark::End));
    stats.rotated += 1;
    stats
}

/// One stacked cell's text (`@textRotation="255"`): the same glyphs, upright,
/// one under the next.
///
/// Not a rotation and so not a bracket — it is a *line breaking* rule, and it
/// is expressed as one: the string becomes one character per line and the flat
/// painter lays it out, which is what keeps the alignment, the font and the
/// vertical placement the cell's own. Excel breaks between characters, not
/// grapheme clusters, and every stacked cell in the corpus is ASCII.
///
/// The lines are cut to what the row is tall enough to hold, the vertical twin
/// of the horizontal clip: 40 stacked glyphs in a 15 pt row would otherwise
/// paint down over every row beneath it, and the command stream has no clip.
#[allow(clippy::too_many_arguments)]
fn paint_stacked_cell_text(
    out: &mut Vec<DrawCommand>,
    m: &TextMeasurer,
    text: &str,
    numeric: bool,
    font: &liteparse_ooxml::xlsx::Font,
    align: &Alignment,
    color: RgbColor,
    b: &CellBox,
    y: f64,
) -> TextPaintStats {
    let fp = font_props(font);
    let (_, metrics) = m.measure("", &fp);
    let line_h = f64::from(f32::from(metrics.ascent))
        + f64::from(f32::from(metrics.descent))
        + f64::from(f32::from(metrics.leading));
    let room = (b.h - 2.0 * f64::from(TEXT_INSET)).max(0.0);
    let fits = if line_h > 0.0 {
        (room / line_h).floor() as usize
    } else {
        0
    }
    .max(1);

    let glyphs: Vec<char> = text.chars().filter(|c| *c != '\n').collect();
    let clipped = glyphs.len() > fits;
    let stacked: String = glyphs
        .iter()
        .take(fits)
        .map(char::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    let mut stats = paint_cell_text(
        out,
        m,
        &stacked,
        numeric,
        font,
        align,
        color,
        b,
        y,
        (0.0, 0.0),
        false,
        None,
    );
    stats.stacked += 1;
    if clipped {
        stats.cross_clipped += 1;
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
        frac: Option<[f64; 4]>,
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
        // A grouped picture's anchor covers its whole group; its own box is
        // the reader's composed fraction of that rect ([`SheetPic::frac`]),
        // applied before page assignment so a pic at the bottom of a tall
        // group lands on the page its top edge is actually on.
        let (gx, gy, gw, gh) = match frac {
            Some([fx, fy, fw, fh]) => (
                gx + fx * gw,
                gy + fy * gh,
                (gw * fw).max(1.0),
                (gh * fh).max(1.0),
            ),
            None => (gx, gy, gw, gh),
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
        // Diagonals are `Line`s and so paint in the Ink pass, not the Shading
        // one; they are collected apart only so they land before the text the
        // same pass draws over them.
        let mut diagonals = Vec::new();
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
                // An automatic `fg` is not white — it is "the consumer's
                // default background", which is the page this is painted on.
                // Painting it would cover the gridlines with the colour they
                // are already on.
                if let Some(rgb) = fill_color(wb, &fill) {
                    cmds.push(rect_cmd(x, y, w, h, rgb));
                }
                let border = wb.styles.border(Some(style));
                if border.paints() {
                    push_border(&mut borders, wb, &border, x, y, w, h);
                    push_diagonal(&mut diagonals, wb, &border, x, y, w, h);
                }
            }
        }
        cmds.append(&mut borders);
        cmds.append(&mut diagonals);

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
                    let numeric = matches!(cell.value, CellValue::Number(_));
                    let font = wb.styles.font(style);
                    let align = wb.styles.alignment(style);
                    stats.add(match cell_rotation(&align) {
                        CellRotation::Angled(deg) => paint_rotated_cell_text(
                            &mut cmds, m, deg, text, numeric, &font, &align, color, &b, y,
                        ),
                        CellRotation::Stacked => paint_stacked_cell_text(
                            &mut cmds, m, text, numeric, &font, &align, color, &b, y,
                        ),
                        CellRotation::None => paint_cell_text(
                            &mut cmds,
                            m,
                            text,
                            numeric,
                            &font,
                            &align,
                            color,
                            &b,
                            y,
                            self.spill_room(&occupied, &b.cols),
                            !cells.iter().any(|n| {
                                n.at.col == cell.at.col + 1
                                    && wb.display_text(n).is_some_and(|t| !t.trim().is_empty())
                            }),
                            // Against the trimmed string the painter draws,
                            // not the padded one the format produced:
                            // `display_text` keeps `_(`'s spaces and `trim`
                            // takes them off both ends, which moves the split.
                            fill_split_of(wb, cell, &text),
                        ),
                    });
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

    /// `gray125` is slot 1 of every styles part, and its `fg` is usually
    /// black. Painting it *solid* would black out sheets that asked for
    /// nothing; painting it at the pattern's 12.5% coverage gives the light
    /// grey Excel shows, which is why the blend is what makes a hatch safe to
    /// paint at all.
    #[test]
    fn the_gray125_placeholder_paints_a_light_grey_not_black() {
        let nx = painted(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews><sheetData>
                <row r="1"><c r="A1" s="4"><v>1</v></c><c r="B1"><v>2</v></c></row>
                <row r="2"><c r="A2"><v>3</v></c><c r="B2"><v>4</v></c></row>
            </sheetData></worksheet>"#,
        );
        let r = rects(&nx.layouts[0]);
        assert_eq!(r.len(), 1, "{r:?}");
        // Black over the page's white at one part in eight.
        let grey = (255.0 * 0.875_f32).round() as u8;
        assert_eq!(
            r[0].4,
            RgbColor {
                r: grey,
                g: grey,
                b: grey
            }
        );
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
        // is the `gray125` placeholder, which blends rather than covering
        // (class B). Row 2: the row's yellow on both columns (class D), with
        // B2's own blue overriding it.
        assert_eq!(r.len(), 4, "{r:?}");
        assert_eq!((r[0].1, r[0].4), (MARGIN, BLUE));
        assert!((r[0].2 - 56.25).abs() < 1e-4, "the column's own width");
        assert_eq!(r[1].1, MARGIN, "the placeholder blend, on row 1");
        assert!(r[1].4.r > 0xD0 && r[1].4.r == r[1].4.b);
        assert_eq!((r[2].1, r[2].4), (MARGIN + 20.0, YELLOW));
        assert_eq!((r[3].1, r[3].4), (MARGIN + 20.0, BLUE));
        assert!((r[3].0 - (MARGIN + 56.25)).abs() < 1e-4, "column B");
    }

    /// The paint tail's border styles, each of which a single rect cannot
    /// express: `dashed` becomes one rect per "on" run, `double` two strokes
    /// with a gap, and the diagonal a `Line` — the one command the grid
    /// painter emits that is not a `Rect`.
    const TAIL_STYLES: &str = r#"<styleSheet>
      <fonts count="1"><font/></fonts>
      <fills count="3">
        <fill><patternFill patternType="none"/></fill>
        <fill><patternFill patternType="gray125"/></fill>
        <fill><patternFill patternType="lightGray"><fgColor rgb="FF000000"/><bgColor rgb="FFFFFFFF"/></patternFill></fill>
      </fills>
      <borders count="4">
        <border><left/><right/><top/><bottom/></border>
        <border><top style="dashed"/></border>
        <border><top style="double"/></border>
        <border diagonalUp="1"><diagonal style="thin"/></border>
      </borders>
      <cellXfs count="5">
        <xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>
        <xf numFmtId="0" fontId="0" fillId="0" borderId="1"/>
        <xf numFmtId="0" fontId="0" fillId="0" borderId="2"/>
        <xf numFmtId="0" fontId="0" fillId="0" borderId="3"/>
        <xf numFmtId="0" fontId="0" fillId="2" borderId="0"/>
      </cellXfs>
    </styleSheet>"#;

    fn tail_painted(sheet_xml: &str) -> NativeXlsx {
        let wb = workbook_from(sheet_xml, &[("xl/styles.xml", TAIL_STYLES)]);
        workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            None,
        )
    }

    fn one_cell(style: u32) -> NativeXlsx {
        tail_painted(&format!(
            r#"<worksheet><sheetViews><sheetView showGridLines="0"/></sheetViews>
            <cols><col min="1" max="1" width="10" customWidth="1"/></cols>
            <sheetData><row r="1" ht="20"><c r="A1" s="{style}"><v>1</v></c></row>
            </sheetData></worksheet>"#
        ))
    }

    /// A broken edge is a run of rects along the cell's top, each one pen-wide
    /// and none of them past the cell.
    #[test]
    fn a_dashed_edge_paints_one_rect_per_run() {
        let nx = one_cell(1);
        let r = rects(&nx.layouts[0]);
        assert!(r.len() > 3, "one rect per dash, got {}", r.len());
        let cell_w = 56.25;
        for (x, y, w, h, _) in &r {
            assert_eq!(*y, MARGIN, "every dash sits on the cell's top edge");
            assert!((*h - 0.75).abs() < 1e-4, "one pen thick");
            assert!(
                *x >= MARGIN && x + w <= MARGIN + cell_w + 1e-4,
                "inside the cell"
            );
        }
        // The dashes leave gaps: they cover less than the whole edge.
        let inked: f32 = r.iter().map(|(_, _, w, _, _)| *w).sum();
        assert!(inked < cell_w * 0.8, "a dashed edge is mostly gap: {inked}");
    }

    /// `double` is two strokes and a gap, filling the three-pen band
    /// `extent_pt` reserves — not one thin line, which is what 27,300 corpus
    /// edges were painted as before.
    #[test]
    fn a_double_edge_paints_two_strokes_with_a_gap() {
        let nx = one_cell(2);
        let r = rects(&nx.layouts[0]);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[0].1, MARGIN);
        assert!((r[1].1 - (MARGIN + 1.5)).abs() < 1e-4, "one pen of gap");
        assert!(r.iter().all(|e| (e.3 - 0.75).abs() < 1e-4));
    }

    /// The diagonal is the grid painter's one non-`Rect`, and it runs corner
    /// to corner of the cell it is declared on.
    #[test]
    fn a_diagonal_paints_a_line_across_the_cell() {
        let nx = one_cell(3);
        let lines: Vec<_> = nx.layouts[0]
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Line { line, .. } => Some(*line),
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 1, "diagonalUp alone draws one line");
        let l = lines[0];
        // Bottom-left to top-right: y falls as x rises.
        assert_eq!((l.start.x.raw(), l.end.y.raw()), (MARGIN, MARGIN));
        assert!(l.start.y.raw() > l.end.y.raw() && l.end.x.raw() > l.start.x.raw());
    }

    /// A hatch has no pattern engine behind it: it paints one rect at the
    /// pattern's coverage, blended over its own `bgColor`.
    #[test]
    fn a_hatch_fill_paints_its_coverage_blend() {
        let nx = one_cell(4);
        let r = rects(&nx.layouts[0]);
        assert_eq!(r.len(), 1, "{r:?}");
        // `lightGray` is one part in four of black over white.
        let grey = (255.0 * 0.75_f32).round() as u8;
        assert_eq!(
            r[0].4,
            RgbColor {
                r: grey,
                g: grey,
                b: grey
            }
        );
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
        // The `shape_rects` pairing contract: one entry per shape, no
        // filter, on the same page as the item — and the *global* page
        // index, which is the part a per-sheet loop gets wrong.
        assert_eq!(nx.shape_rects.len(), 1);
        let (page, rect) = &nx.shape_rects[0];
        assert_eq!(*page, with_item[0], "the shape's page, workbook-global");
        let item = nx.pages[*page]
            .text_items
            .iter()
            .find(|t| t.text == "deep note")
            .expect("the item is on that page");
        assert!(
            (rect.x - item.x).abs() < 0.01 && (rect.width - item.width).abs() < 0.01,
            "the reported box is the box the items were stacked in"
        );
        // 914400 x 457200 EMU = 72 x 36 pt.
        assert!((rect.width - 72.0).abs() < 0.5 && (rect.height - 36.0).abs() < 0.5);
    }

    /// `shape_rects` pairs with `ordered_shapes` positionally, so the two
    /// sides must apply the *same* filter — and neither applies one: the
    /// textless shape is already gone by the time either sees it, dropped by
    /// the reader. Pinning where that happens is the point: moving the filter
    /// down here would shift every later entry against a consumer's walk.
    #[test]
    fn a_textless_shape_is_dropped_before_either_side_sees_it() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="Empty"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p/></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let parts = shape_drawing_parts(anchor);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData><row r="1"><c r="A1" t="str"><v>x</v></c></row></sheetData><drawing r:id="rId7"/></worksheet>"#;
        let wb = workbook_from(sheet, &extra);
        let nx = workbook_to_pages(&wb, EmitOptions::default(), None);
        assert!(
            wb.sheets[0].shapes.is_empty(),
            "the reader drops a textless shape"
        );
        assert_eq!(
            nx.shape_rects.len(),
            super::super::xlsx::ordered_shapes(&wb.sheets[0]).len(),
            "both sides count the same shapes"
        );
    }

    // ── shape ink (fills and outlines) ──────────────────────────────────────

    use liteparse_ooxml::render::layout::draw_command::{FloatMark, ResolvedFill};

    /// Every painted `Path` command's (x, y, w, h, fill, has_stroke).
    fn shape_paths(page: &LayoutedPage) -> Vec<(f32, f32, f32, f32, ResolvedFill, bool)> {
        page.commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Path {
                    origin,
                    extent,
                    fill,
                    stroke,
                    ..
                } => Some((
                    origin.x.raw(),
                    origin.y.raw(),
                    extent.width.raw(),
                    extent.height.raw(),
                    fill.clone(),
                    stroke.is_some(),
                )),
                _ => None,
            })
            .collect()
    }

    /// The carrier gap closed: a textless filled rectangle — dropped by the
    /// text channel, invisible before this step — paints as a `Path` in a
    /// `Float` bracket, and the item stream never hears about it.
    #[test]
    fn a_textless_filled_shape_paints_a_path_and_adds_no_item() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="Box"/></xdr:nvSpPr>
               <xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                 <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></xdr:spPr></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let paths = shape_paths(&nx.layouts[0]);
        assert_eq!(paths.len(), 1, "one Path: {paths:?}");
        let (_, _, w, h, fill, _) = &paths[0];
        assert!(
            (w - 72.0).abs() < 0.5 && (h - 36.0).abs() < 0.5,
            "{paths:?}"
        );
        match fill {
            ResolvedFill::Solid(c) => {
                assert!(c.r > 0.99 && c.g < 0.01 && c.b < 0.01, "red: {c:?}")
            }
            other => panic!("expected a solid fill, got {other:?}"),
        }
        // Inside a Float bracket, so it draws over the grid and cell values.
        let cmds = &nx.layouts[0].commands;
        let path_at = cmds
            .iter()
            .position(|c| matches!(c, DrawCommand::Path { .. }))
            .unwrap();
        assert!(
            matches!(cmds[path_at - 1], DrawCommand::Float(FloatMark::Begin))
                && matches!(cmds[path_at + 1], DrawCommand::Float(FloatMark::End)),
            "the Path floats"
        );
        // The item side is exactly the grid cell — the shape adds nothing.
        assert_eq!(
            nx.pages[0]
                .text_items
                .iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>(),
            vec!["grid"]
        );
    }

    /// A grouped child paints in its share of the anchor box: the group's
    /// `chOff`/`chExt` map the child's declared quarter onto a quarter of
    /// the page rect. 3,547 corpus inked shapes are inside a group; without
    /// this mapping every one of them would smear across the full anchor.
    #[test]
    fn a_grouped_child_paints_in_its_share_of_the_anchor_box() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="914400"/>
             <xdr:grpSp>
               <xdr:nvGrpSpPr><xdr:cNvPr id="1" name="G"/></xdr:nvGrpSpPr>
               <xdr:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="914400"/>
                 <a:chOff x="0" y="0"/><a:chExt cx="200" cy="200"/></a:xfrm></xdr:grpSpPr>
               <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="2" name="Full"/></xdr:nvSpPr>
                 <xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="200" cy="200"/></a:xfrm>
                   <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                   <a:solidFill><a:srgbClr val="00FF00"/></a:solidFill></xdr:spPr></xdr:sp>
               <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="3" name="Quarter"/></xdr:nvSpPr>
                 <xdr:spPr><a:xfrm><a:off x="100" y="100"/><a:ext cx="100" cy="100"/></a:xfrm>
                   <a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                   <a:solidFill><a:srgbClr val="0000FF"/></a:solidFill></xdr:spPr></xdr:sp>
             </xdr:grpSp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let paths = shape_paths(&nx.layouts[0]);
        assert_eq!(paths.len(), 2, "{paths:?}");
        let (fx, fy, fw, fh, ..) = paths[0];
        let (qx, qy, qw, qh, ..) = paths[1];
        assert!((fw - 72.0).abs() < 0.5 && (fh - 72.0).abs() < 0.5);
        assert!(
            (qw - fw / 2.0).abs() < 0.5 && (qh - fh / 2.0).abs() < 0.5,
            "the quarter child is half the size: {paths:?}"
        );
        assert!(
            (qx - (fx + fw / 2.0)).abs() < 0.5 && (qy - (fy + fh / 2.0)).abs() < 0.5,
            "and sits in the bottom-right quarter: {paths:?}"
        );
    }

    /// A connector is pure line work: no fill, a stroke, and it reaches the
    /// paint even though the text channel rightly never surfaces it.
    #[test]
    fn a_connector_paints_its_stroke() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:cxnSp><xdr:nvCxnSpPr><xdr:cNvPr id="3" name="L"/></xdr:nvCxnSpPr>
               <xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="914400" cy="457200"/></a:xfrm>
                 <a:prstGeom prst="line"><a:avLst/></a:prstGeom>
                 <a:ln w="19050"><a:solidFill><a:srgbClr val="112233"/></a:solidFill></a:ln></xdr:spPr></xdr:cxnSp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let paths = shape_paths(&nx.layouts[0]);
        assert_eq!(paths.len(), 1, "{paths:?}");
        let (.., fill, has_stroke) = &paths[0];
        assert!(matches!(fill, ResolvedFill::None), "{fill:?}");
        assert!(has_stroke, "the connector strokes");
    }

    /// A flat line — every horizontal connector declares `cy="0"` — still
    /// strokes; a zero-extent *point* (540 corpus shapes, all legacy
    /// `Line NNN` leftovers) paints nothing.
    #[test]
    fn a_flat_line_strokes_and_a_point_does_not() {
        let line = |ext: &str| {
            format!(
                r#"<xdr:twoCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:to><xdr:col>1</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>9525</xdr:rowOff></xdr:to>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="L"/></xdr:nvSpPr>
               <xdr:spPr><a:xfrm><a:off x="0" y="0"/>{ext}</a:xfrm>
                 <a:prstGeom prst="line"><a:avLst/></a:prstGeom><a:noFill/>
                 <a:ln w="19050"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln></xdr:spPr></xdr:sp>
           <xdr:clientData/></xdr:twoCellAnchor>"#
            )
        };
        let flat = painted_shape(&line(r#"<a:ext cx="466725" cy="0"/>"#));
        assert_eq!(
            shape_paths(&flat.layouts[0]).len(),
            1,
            "a flat line strokes"
        );
        let point = painted_shape(&line(r#"<a:ext cx="0" cy="0"/>"#));
        assert!(
            shape_paths(&point.layouts[0]).is_empty(),
            "a point paints nothing"
        );
    }

    /// A vertical line rotated a quarter turn into a horizontal one — 88
    /// corpus shapes — still paints: its unrotated rect is zero-width, and
    /// only the rotated *bounding box* meaningfully maps onto the anchor.
    #[test]
    fn a_quarter_rotated_flat_line_still_paints() {
        let anchor = r#"<xdr:twoCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>100</xdr:rowOff></xdr:from>
             <xdr:to><xdr:col>3</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>2</xdr:row><xdr:rowOff>100</xdr:rowOff></xdr:to>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="2" name="Line 1"/></xdr:nvSpPr>
               <xdr:spPr><a:xfrm rot="16200000" flipV="1"><a:off x="2641600" y="13474700"/><a:ext cx="0" cy="660400"/></a:xfrm>
                 <a:prstGeom prst="line"><a:avLst/></a:prstGeom><a:noFill/>
                 <a:ln w="12700"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:ln></xdr:spPr></xdr:sp>
           <xdr:clientData/></xdr:twoCellAnchor>"#;
        let nx = painted_shape(anchor);
        let paths = shape_paths(&nx.layouts[0]);
        assert_eq!(paths.len(), 1, "{paths:?}");
        // The pre-rotation extent is the flat bounding box swapped back
        // upright: zero wide, anchor-width tall.
        let (_, _, w, h, _, has_stroke) = &paths[0];
        assert!(*w < 0.5 && *h > 10.0, "upright extent: {paths:?}");
        assert!(has_stroke);
    }

    /// A hidden shape — a legacy form control — paints nothing, the same
    /// bit the text channel already honours.
    #[test]
    fn a_hidden_shape_paints_nothing() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="ctl" hidden="1"/></xdr:nvSpPr>
               <xdr:spPr><a:prstGeom prst="rect"><a:avLst/></a:prstGeom>
                 <a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></xdr:spPr></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        assert!(shape_paths(&nx.layouts[0]).is_empty());
    }

    /// A declared run colour reaches the painted text — red glyphs, not the
    /// black default. 5,041 of 5,902 corpus runs declare one.
    #[test]
    fn a_declared_run_colour_reaches_the_painted_text() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r>
                 <a:rPr sz="1200"><a:solidFill><a:srgbClr val="FF0000"/></a:solidFill></a:rPr>
                 <a:t>warm</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let colors: Vec<RgbColor> = nx.layouts[0]
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Text { text, color, .. } if text.as_ref() == "warm" => Some(*color),
                _ => None,
            })
            .collect();
        assert!(!colors.is_empty(), "the run painted");
        assert!(
            colors.iter().all(|c| c.r == 0xFF && c.g == 0 && c.b == 0),
            "and in its declared red: {colors:?}"
        );
    }

    /// A shape with `anchor_xml`, painted with a real registry.
    fn painted_shape(anchor_xml: &str) -> NativeXlsx {
        let parts = shape_drawing_parts(anchor_xml);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData><row r="1"><c r="A1" t="str"><v>grid</v></c></row></sheetData><drawing r:id="rId7"/></worksheet>"#;
        let wb = workbook_from(sheet, &extra);
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

    /// Every painted `Text` command's (x, y, text).
    fn shape_texts(page: &LayoutedPage) -> Vec<(f32, f32, String)> {
        page.commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Text { position, text, .. } => {
                    Some((position.x.raw(), position.y.raw(), text.to_string()))
                }
                _ => None,
            })
            .collect()
    }

    /// A sentence too wide for its box paints as several measured lines while
    /// the item stays the single unmeasured stack entry. This is the whole
    /// shape of step 5b: paint is a layout, items are not, and the census
    /// says 34.8% of corpus paragraphs are on this path.
    #[test]
    fn shape_text_wraps_in_the_paint_and_not_in_the_items() {
        // 72 x 72 pt box, a sentence far wider than 72pt at 12pt Calibri.
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="914400"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="1200"/><a:t>one two three four five six seven eight</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let painted = shape_texts(&nx.layouts[0]);
        assert!(
            painted.len() > 2,
            "the sentence wraps over several lines, got {painted:?}"
        );
        let joined: String = painted
            .iter()
            .map(|(_, _, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            joined.contains("one") && joined.contains("eight"),
            "{joined}"
        );
        // Lines descend, and each starts inside the box's left inset.
        for w in painted.windows(2) {
            assert!(w[1].1 >= w[0].1, "lines descend: {painted:?}");
        }
        let shape_items: Vec<&TextItem> = nx.pages[0]
            .text_items
            .iter()
            .filter(|t| t.text.starts_with("one two"))
            .collect();
        assert_eq!(shape_items.len(), 1, "the item side is still one paragraph");
    }

    /// `anchor="ctr"` moves the painted body down the box — 61.5% of corpus
    /// shapes, and the reason the even stack could not be painted as-is.
    #[test]
    fn a_centred_anchor_moves_the_painted_body_and_not_the_item() {
        let shape = |anchor: &str| {
            format!(
                r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="1828800"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr{anchor}/><a:p><a:r><a:rPr sz="1000"/><a:t>hi</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#
            )
        };
        let top = painted_shape(&shape(r#" anchor="t""#));
        let ctr = painted_shape(&shape(r#" anchor="ctr""#));
        // The sheet's own A1 value paints too, so pick the shape's line by
        // its text rather than by position in the list.
        let y_of = |nx: &NativeXlsx| {
            shape_texts(&nx.layouts[0])
                .into_iter()
                .find(|(_, _, t)| t == "hi")
                .expect("the shape's line")
                .1
        };
        let (top_y, ctr_y) = (y_of(&top), y_of(&ctr));
        // The box is 144pt tall and holds one short line, so centring it has
        // most of that to give away.
        assert!(
            ctr_y - top_y > 50.0,
            "centred body sits far below the top one: {top_y} -> {ctr_y}"
        );
        let item_y = |nx: &NativeXlsx| {
            nx.pages[0]
                .text_items
                .iter()
                .find(|t| t.text == "hi")
                .expect("the item")
                .y
        };
        assert!(
            (item_y(&top) - item_y(&ctr)).abs() < 0.01,
            "the item is unmoved by the anchor — items are not a layout"
        );
    }

    /// The host-independence claim, restated for shapes: the items are
    /// byte-identical with and without a registry, and without one nothing
    /// is painted at all.
    #[test]
    fn shape_text_paints_only_with_a_registry_and_never_into_the_items() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="914400"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="1200"/><a:t>label</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let parts = shape_drawing_parts(anchor);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData><row r="1"><c r="A1" t="str"><v>grid</v></c></row></sheetData><drawing r:id="rId7"/></worksheet>"#;
        let wb = workbook_from(sheet, &extra);
        let with = painted_shape(anchor);
        let without = workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            None,
        );
        let parsed = workbook_to_pages(&wb, EmitOptions::default(), None);

        assert!(
            shape_texts(&with.layouts[0])
                .iter()
                .any(|(_, _, t)| t == "label"),
            "painted with a registry"
        );
        assert!(
            shape_texts(&without.layouts[0]).is_empty(),
            "and not without one"
        );
        let items = |nx: &NativeXlsx| {
            nx.pages[0]
                .text_items
                .iter()
                .map(|t| (t.text.clone(), t.x, t.y, t.width, t.height))
                .collect::<Vec<_>>()
        };
        assert_eq!(items(&with), items(&parsed), "items never see the measurer");
        assert_eq!(items(&without), items(&parsed));
    }

    /// A box narrower than the two default 7.2pt insets still paints. Without
    /// the clamp `layout_shape_body` returns nothing at all, which is 6.5% of
    /// corpus shapes — the narrow marker labels a timeline is made of.
    #[test]
    fn a_box_narrower_than_its_insets_still_paints() {
        // 114300 EMU = 9pt wide, against 14.4pt of default horizontal inset.
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="114300" cy="914400"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="800"/><a:t>END</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let nx = painted_shape(anchor);
        let painted = shape_texts(&nx.layouts[0]);
        let joined: String = painted
            .iter()
            .filter(|(_, _, t)| t != "grid")
            .map(|(_, _, t)| t.as_str())
            .collect();
        // It breaks per character in a 9pt box, which is what a square wrap
        // does — but every character is drawn, and none of it is dropped.
        assert_eq!(joined, "END", "got {painted:?}");
    }

    /// A shape on a sheet with no written cells still paints: the append sits
    /// outside the `SheetPlan` arm, like the picture one.
    #[test]
    fn a_plan_less_sheet_still_paints_its_shape_text() {
        let anchor = r#"<xdr:oneCellAnchor>
             <xdr:from><xdr:col>0</xdr:col><xdr:colOff>0</xdr:colOff><xdr:row>0</xdr:row><xdr:rowOff>0</xdr:rowOff></xdr:from>
             <xdr:ext cx="914400" cy="457200"/>
             <xdr:sp><xdr:nvSpPr><xdr:cNvPr id="5" name="T"/></xdr:nvSpPr><xdr:spPr/>
               <xdr:txBody><a:bodyPr/><a:p><a:r><a:rPr sz="1200"/><a:t>alone</a:t></a:r></a:p></xdr:txBody></xdr:sp>
           <xdr:clientData/></xdr:oneCellAnchor>"#;
        let parts = shape_drawing_parts(anchor);
        let extra: Vec<(&str, &str)> = parts
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let sheet = r#"<worksheet xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData/><drawing r:id="rId7"/></worksheet>"#;
        let wb = workbook_from(sheet, &extra);
        let registry = FontRegistry::new();
        let nx = workbook_to_pages(
            &wb,
            EmitOptions {
                paint: true,
                ..Default::default()
            },
            Some(&registry),
        );
        assert!(
            shape_texts(&nx.layouts[0])
                .iter()
                .any(|(_, _, t)| t == "alone"),
            "an image-less, grid-less sheet still draws its shape"
        );
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
      <cellXfs count="11">
        <xf numFmtId="0" fontId="0"/>
        <xf numFmtId="0" fontId="0"><alignment horizontal="right"/></xf>
        <xf numFmtId="0" fontId="0"><alignment horizontal="center"/></xf>
        <xf numFmtId="0" fontId="0"><alignment wrapText="1"/></xf>
        <xf numFmtId="0" fontId="1"/>
        <xf numFmtId="0" fontId="0"><alignment vertical="top"/></xf>
        <xf numFmtId="0" fontId="0"><alignment textRotation="90"/></xf>
        <xf numFmtId="0" fontId="0"><alignment textRotation="180"/></xf>
        <xf numFmtId="0" fontId="0"><alignment textRotation="255"/></xf>
        <xf numFmtId="0" fontId="0"><alignment textRotation="90" vertical="top"/></xf>
        <xf numFmtId="0" fontId="0"><alignment textRotation="45"/></xf>
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

    // ── §18.8.1 rotation ────────────────────────────────────────────────────

    /// Every bracket on a page as `(rotation in 60,000ths, origin, extent,
    /// the turned bounding box its commands occupy)` — the last computed the
    /// way `raster.rs::place_transform` does, so a test asserts about where
    /// the *rasterizer* will put the glyphs rather than about local
    /// coordinates only the producer can see.
    struct Bracket {
        rotation: i64,
        /// The box the sampled points occupy on the page.
        bbox: (f32, f32, f32, f32),
        /// The first run's baseline start and the point one em above it, both
        /// turned onto the page — the glyphs' own two axes, which is what says
        /// which way the text reads.
        baseline: (f32, f32),
        ascender: (f32, f32),
    }

    fn brackets(page: &LayoutedPage) -> Vec<Bracket> {
        let mut out: Vec<Bracket> = Vec::new();
        let mut open: Option<ShapeTransform> = None;
        let mut runs = 0usize;
        for cmd in &page.commands {
            match cmd {
                DrawCommand::Transform(TransformMark::Begin(t)) => {
                    open = Some(t.clone());
                    runs = 0;
                    out.push(Bracket {
                        rotation: t.rotation.raw(),
                        bbox: (f32::MAX, f32::MAX, f32::MIN, f32::MIN),
                        baseline: (0.0, 0.0),
                        ascender: (0.0, 0.0),
                    });
                }
                DrawCommand::Transform(TransformMark::End) => {
                    open.take().expect("End without Begin");
                }
                DrawCommand::Text {
                    position,
                    font_size,
                    ..
                } => {
                    let Some(t) = open.as_ref() else { continue };
                    let b = out.last_mut().expect("a run inside no bracket");
                    let (cx, cy) = (t.extent.width.raw() / 2.0, t.extent.height.raw() / 2.0);
                    let (sin, cos) = (t.rotation.raw() as f32 / 60_000.0).to_radians().sin_cos();
                    let place = |px: f32, py: f32| {
                        let (dx, dy) = (px - cx, py - cy);
                        (
                            t.origin.x.raw() + cx + dx * cos - dy * sin,
                            t.origin.y.raw() + cy + dx * sin + dy * cos,
                        )
                    };
                    let baseline = place(position.x.raw(), position.y.raw());
                    let ascender = place(position.x.raw(), position.y.raw() - font_size.raw());
                    for (gx, gy) in [baseline, ascender] {
                        b.bbox = (
                            b.bbox.0.min(gx),
                            b.bbox.1.min(gy),
                            b.bbox.2.max(gx),
                            b.bbox.3.max(gy),
                        );
                    }
                    if runs == 0 {
                        b.baseline = baseline;
                        b.ascender = ascender;
                    }
                    runs += 1;
                }
                _ => {}
            }
        }
        assert!(open.is_none(), "a bracket was left open");
        out
    }

    /// §18.8.1's two quarter turns are opposite rotations of the same size,
    /// and the bracket's angle is the *clockwise* one §20.1.10.3 asks for:
    /// `textRotation="90"` reads up the page, `"180"` (90° clockwise) reads
    /// down it.
    #[test]
    fn a_quarter_turn_brackets_its_text_at_ninety_degrees_either_way() {
        let nx = texted(
            r#"<worksheet><cols><col min="1" max="2" width="9" customWidth="1"/></cols><sheetData>
            <row r="1" ht="60"><c r="A1" s="6" t="inlineStr"><is><t>up</t></is></c><c r="B1" s="7" t="inlineStr"><is><t>down</t></is></c></row>
        </sheetData></worksheet>"#,
        );
        let b = brackets(&nx.layouts[0]);
        assert_eq!(b.len(), 2, "one bracket per rotated cell");
        assert_eq!(
            b[0].rotation, -5_400_000,
            "textRotation=90 turns counter-clockwise"
        );
        assert_eq!(b[1].rotation, 5_400_000, "textRotation=180 turns clockwise");
        assert_eq!(nx.text_stats.rotated, 2);
        assert_eq!(nx.text_stats.stacked, 0);

        // The glyphs' up direction stands upright in the local space, so a
        // quarter turn must lay it flat on the page — to the left of the
        // baseline for the turn that reads up the page, to the right for the
        // one that reads down it.
        for k in &b {
            assert!(
                (k.ascender.1 - k.baseline.1).abs() < 0.01,
                "the em lies flat at {}: {:?} {:?}",
                k.rotation,
                k.baseline,
                k.ascender
            );
        }
        assert!(b[0].ascender.0 < b[0].baseline.0 - 5.0, "90° reads up");
        assert!(b[1].ascender.0 > b[1].baseline.0 + 5.0, "180° reads down");
    }

    /// The alignment does **not** turn with the glyphs: `vertical` moves the
    /// block up and down the page and `horizontal` moves it left and right, at
    /// every angle. Measured against LibreOffice on a probe workbook of every
    /// rotation × alignment pair, and the opposite of what a rotated frame
    /// would do.
    #[test]
    fn rotation_leaves_the_alignment_on_the_pages_own_axes() {
        let nx = texted(
            r#"<worksheet><cols><col min="1" max="2" width="9" customWidth="1"/></cols><sheetData>
            <row r="1" ht="80"><c r="A1" s="6" t="inlineStr"><is><t>ab</t></is></c><c r="B1" s="9" t="inlineStr"><is><t>ab</t></is></c></row>
        </sheetData></worksheet>"#,
        );
        let b = brackets(&nx.layouts[0]);
        let (bottom, top) = (b[0].bbox, b[1].bbox);
        // Same angle, same string, same box — only `vertical` differs, and it
        // moves the block down the page rather than along its baseline.
        assert_eq!(b[0].rotation, b[1].rotation);
        assert!(
            top.1 < bottom.1 && top.3 < bottom.3,
            "vertical=top sits above the default bottom: top {top:?} bottom {bottom:?}"
        );
        // Both stay inside the row: 80 pt tall, one page, margin at the top.
        assert!(top.1 >= MARGIN - 1.0 && bottom.3 <= MARGIN + 80.0 + 1.0);
    }

    /// Stacked is not a rotation: no bracket, one command per glyph, and the
    /// stack is cut to the rows the cell is tall — Excel clips vertically,
    /// and a stack that overflowed would paint over every row beneath it.
    #[test]
    fn stacked_text_is_one_upright_glyph_per_line_cut_to_the_row() {
        let nx = texted(
            r#"<worksheet><cols><col min="1" max="1" width="9" customWidth="1"/></cols><sheetData>
            <row r="1" ht="60"><c r="A1" s="8" t="inlineStr"><is><t>abc</t></is></c></row>
            <row r="2" ht="12"><c r="A2" s="8" t="inlineStr"><is><t>abcdefghij</t></is></c></row>
        </sheetData></worksheet>"#,
        );
        assert!(
            brackets(&nx.layouts[0]).is_empty(),
            "stacked carries no bracket"
        );
        assert_eq!(nx.text_stats.stacked, 2);
        assert_eq!(nx.text_stats.rotated, 0);
        assert_eq!(nx.text_stats.cross_clipped, 1, "only the short row cuts");

        let t = texts(&nx.layouts[0]);
        let tall: Vec<&(String, f32, f32, RgbColor)> =
            t.iter().filter(|c| c.2 < MARGIN + 60.0).collect();
        assert_eq!(
            tall.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "one glyph per line, in reading order"
        );
        // Each glyph is a line below the one before it, and they share a column.
        assert!(tall[1].2 > tall[0].2 && tall[2].2 > tall[1].2);
        assert_eq!(tall[0].1, tall[1].1);
        // The 12 pt row holds one line of an 11 pt font, so nine glyphs go.
        assert_eq!(t.len() - tall.len(), 1);
    }

    /// The parse is not a rendering: a rotated cell's `TextItem` is the same
    /// cell box, with the same text, that an unrotated one would have. The
    /// rotation lives in the paint alone.
    #[test]
    fn rotation_never_reaches_the_items() {
        let sheet = |s: &str| {
            format!(
                r#"<worksheet><cols><col min="1" max="1" width="9" customWidth="1"/></cols><sheetData>
                <row r="1" ht="60"><c r="A1"{s} t="inlineStr"><is><t>header</t></is></c></row>
            </sheetData></worksheet>"#
            )
        };
        let flat = texted(&sheet(""));
        for style in [r#" s="6""#, r#" s="7""#, r#" s="8""#, r#" s="10""#] {
            assert_eq!(
                format!("{:?}", texted(&sheet(style)).pages[0].text_items),
                format!("{:?}", flat.pages[0].text_items),
                "{style} moved an item"
            );
        }
    }
}
