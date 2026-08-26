//! Skia-free render pipeline: resolve → layout.
//!
//! Upstream this module is `resolve → layout → subset → paint`. Vendored here
//! are `resolve` (style/numbering cascade — everything the structure path
//! needs) and `layout` — dxpdf's pagination engine with its Skia
//! `TextMeasurer` swapped for fontdb + skrifa (`layout::measurer`, `fonts`).
//! `subset` and `painter` are PDF-emission concerns and stay out; that is why
//! `render()` / PDF output do not exist here and layout ends at
//! [`layout::draw_command::LayoutedPage`].

pub mod dimension;
pub mod emoji;
pub mod error;
pub mod fonts;
pub mod geometry;
pub mod layout;
#[cfg(feature = "raster")]
pub mod raster;
pub mod resolve;

use crate::model::Document;

use crate::model::Block;
use crate::render::layout::build::{
    BuildContext, BuildState, build_document_endnotes, build_section_blocks, default_line_height,
};
use crate::render::layout::draw_command::LayoutedPage;
use crate::render::layout::header_footer::{
    HeaderFooterBlocks, HeaderFooterClearance, PageRange, render_headers_footers,
};
use crate::render::layout::page::PageConfig;
use crate::render::layout::section::layout_section_with_clearance;
use crate::render::resolve::ResolvedDocument;
use crate::render::resolve::header_footer::HeaderFooterSet;

/// Estimate where the content on `page` ends, so a `Continuous` section
/// (§17.6.22) knows where to resume on the same page.
///
/// The match is **exhaustive on purpose**: a `_ => continue` arm silently gives
/// any unlisted variant zero height, and the following section then paints over
/// it. That is how `EmojiCluster` and `Path` came to be ignored — a paragraph
/// containing only an emoji resumed at `margins.top`, drawing the next
/// section's text inside the emoji's box. Adding a variant should break this
/// build, not the output.
fn estimate_cursor_y(
    page: &layout::draw_command::LayoutedPage,
    config: &layout::page::PageConfig,
) -> dimension::Pt {
    use layout::draw_command::DrawCommand;
    let mut max_y = config.margins.top;
    for cmd in &page.commands {
        let bottom = match cmd {
            // Baseline plus a full font size approximates the descender.
            DrawCommand::Text {
                position,
                font_size,
                ..
            } => position.y + *font_size,
            // A segment may run in either direction; take the lower end.
            DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
                line.start.y.max(line.end.y)
            }
            DrawCommand::Image { rect, .. }
            | DrawCommand::EmojiCluster { rect, .. }
            | DrawCommand::Rect { rect, .. } => rect.origin.y + rect.size.height,
            // `extent` is the shape's unrotated bounding box; a rotation can
            // reach slightly past it, which is within this function's remit.
            DrawCommand::Path { origin, extent, .. } => origin.y + extent.height,
            // §17.3.1.19: draws nothing, so it consumes no vertical space.
            DrawCommand::Outline(_) => continue,
            // §20.1.7.6: a placement marker, and one this function never sees
            // — it runs over DOCX section pages, which carry no brackets.
            DrawCommand::Transform(_) => continue,
            // Pass routing, XLSX only — same story: no DOCX page carries one.
            DrawCommand::Float(_) => continue,
            // Annotations mark content that is already accounted for by the
            // command underneath them, and a named destination is a point.
            // Neither adds extent of its own.
            DrawCommand::LinkAnnotation { .. }
            | DrawCommand::InternalLink { .. }
            | DrawCommand::NamedDestination { .. } => continue,
        };
        if bottom > max_y {
            max_y = bottom;
        }
    }
    max_y
}

/// Resolve and lay out a document without painting to PDF.
/// Uses a real FontMgr for text measurement.
pub fn resolve_and_layout(doc: Document) -> (ResolvedDocument, Vec<LayoutedPage>) {
    let resolved = resolve::resolve(doc);
    // A debug/test helper: it always loads the host's fonts, so the font-less
    // case `build` guards against cannot arise on any host with fonts.
    let registry = fonts::FontRegistry::build(&resolved.embedded_fonts, &resolved.font_families)
        .expect("the host exposes at least one font face");
    let pages = layout_document(&resolved, &registry);
    (resolved, pages)
}

/// Lay out a resolved document using fontdb + skrifa font metrics resolved
/// through the supplied [`fonts::FontRegistry`].
pub fn layout_document(
    resolved: &ResolvedDocument,
    registry: &fonts::FontRegistry,
) -> Vec<LayoutedPage> {
    let measurer = layout::measurer::TextMeasurer::new(registry);
    let ctx = BuildContext {
        measurer: &measurer,
        resolved,
    };
    let mut state = BuildState::default();
    let dlh = default_line_height(&ctx);
    let mut all_pages = Vec::new();
    let mut last_config = PageConfig::default();
    // Per-section metadata for deferred header/footer rendering.
    // Carries the section's resolved slot sets, `<w:titlePg/>` flag,
    // and logical page number of the section's first page (§17.6.12);
    // the global `<w:evenAndOddHeaders/>` setting is read once below.
    struct SectionHfInfo<'a> {
        page_range: std::ops::Range<usize>,
        config: PageConfig,
        headers: &'a crate::render::resolve::header_footer::HeaderFooterSet<Vec<Block>>,
        footers: &'a crate::render::resolve::header_footer::HeaderFooterSet<Vec<Block>>,
        title_pg: bool,
        logical_page_base: usize,
    }
    let mut section_hf: Vec<SectionHfInfo> = Vec::new();
    // §17.6.12: logical PAGE numbering accumulates across sections,
    // resetting wherever a section sets `pgNumType.start`. Document
    // starts at logical 1 unless the first section overrides it.
    let mut next_logical: usize = 1;

    // §17.11.23: footnote separator indent from default paragraph style.
    let separator_indent = resolved
        .default_paragraph_style_id
        .as_ref()
        .and_then(|id| resolved.styles.get(id))
        .and_then(|s| s.paragraph.indentation)
        .and_then(|ind| ind.first_line)
        .map(|fl| match fl {
            crate::model::FirstLineIndent::FirstLine(v) => dimension::Pt::from(v),
            _ => dimension::Pt::ZERO,
        })
        .unwrap_or(dimension::Pt::ZERO);

    // §17.6.22: track continuation state for `Continuous` section breaks.
    let mut pending_continuation: Option<layout::section::ContinuationState> = None;
    let even_and_odd = resolved.even_and_odd_headers;

    // liteparse instrumentation: running offset turning each section's local
    // block indices into indices over the flattened concatenation of every
    // section's `blocks` — the same flattening the structure emitter walks.
    let mut body_base: usize = 0;

    // Phase 1: layout all sections to determine total page count.
    for (section_idx, section) in resolved.sections.iter().enumerate() {
        let config = PageConfig::from_section(&section.properties);
        state.page_config = config.clone();
        let logical_page_base = layout::header_footer::next_logical_page_base(
            next_logical,
            section.properties.page_number_type.as_ref(),
        );
        let clearance = measure_header_footer_clearance(
            &config,
            section,
            &ctx,
            &mut state,
            dlh,
            even_and_odd,
            logical_page_base,
        );

        let built = build_section_blocks(section, &config, &ctx, &mut state);
        let block_sources: Vec<usize> =
            built.source_indices.iter().map(|i| i + body_base).collect();
        body_base += section.blocks.len();
        let measure_fn = |text: &str,
                          font: &layout::fragment::FontProps|
         -> (dimension::Pt, layout::fragment::TextMetrics) {
            measurer.measure(text, font)
        };

        // §17.6.22: continuous sections continue on the current page.
        let continuation =
            if section.properties.section_type == Some(crate::model::SectionType::Continuous) {
                pending_continuation.take()
            } else {
                pending_continuation = None;
                None
            };

        let mut pages = layout_section_with_clearance(
            &built.blocks,
            &config,
            Some(&measure_fn),
            separator_indent,
            dlh,
            layout::section::SectionStart {
                continuation,
                clearance: &clearance,
                logical_page_base,
                block_sources: &block_sources,
            },
        );

        last_config = config.clone();

        // Check if the NEXT section is continuous — if so, save the last page
        // as continuation state instead of appending it.
        // (Peek ahead by checking the section index.)
        let next_is_continuous = resolved.sections.get(section_idx + 1).is_some_and(|next| {
            next.properties.section_type == Some(crate::model::SectionType::Continuous)
        });

        if next_is_continuous && !pages.is_empty() {
            let last_page = pages.pop().unwrap();
            let cursor_y = estimate_cursor_y(&last_page, &last_config);
            pending_continuation = Some(layout::section::ContinuationState {
                page: last_page,
                cursor_y,
            });
        }

        let page_start = all_pages.len();
        all_pages.append(&mut pages);
        let pages_in_section = all_pages.len() - page_start;
        next_logical = logical_page_base + pages_in_section;
        section_hf.push(SectionHfInfo {
            page_range: page_start..all_pages.len(),
            config,
            headers: &section.headers,
            footers: &section.footers,
            title_pg: section.properties.title_page.unwrap_or(false),
            logical_page_base,
        });
    }

    // §17.11.2: endnotes are document-scoped — built once, after every section,
    // so a multi-section document doesn't repeat them per section.
    let all_endnotes = build_document_endnotes(&ctx, &mut state);

    // Phase 2: render headers/footers with correct NUMPAGES (total page count).
    let total_pages = all_pages.len();
    for info in &section_hf {
        state.page_config = info.config.clone();
        render_headers_footers(
            &mut all_pages[info.page_range.clone()],
            &info.config,
            &HeaderFooterBlocks {
                headers: info.headers,
                footers: info.footers,
                title_pg: info.title_pg,
                even_and_odd,
            },
            &ctx,
            &mut state,
            dlh,
            &PageRange {
                page_base: info.page_range.start,
                logical_page_base: info.logical_page_base,
                total_pages,
            },
        );
    }

    // Render endnotes on a new page at the end of the document.
    if !all_endnotes.is_empty() {
        let measure_fn = |text: &str,
                          font: &layout::fragment::FontProps|
         -> (dimension::Pt, layout::fragment::TextMetrics) {
            measurer.measure(text, font)
        };
        let mut endnote_page = LayoutedPage::new(last_config.page_size);
        let content_width = last_config.content_width();
        let constraints =
            layout::BoxConstraints::tight_width(content_width, dimension::Pt::INFINITY);
        let mut cursor_y = last_config.margins.top;

        // Separator line.
        let sep_width = content_width * 0.33;
        let sep_x = last_config.margins.left + separator_indent;
        endnote_page
            .commands
            .push(layout::draw_command::DrawCommand::Line {
                line: crate::render::geometry::PtLineSegment::new(
                    crate::render::geometry::PtOffset::new(sep_x, cursor_y),
                    crate::render::geometry::PtOffset::new(sep_x + sep_width, cursor_y),
                ),
                color: crate::render::resolve::color::RgbColor::BLACK,
                width: dimension::Pt::new(0.5),
            });
        cursor_y += dimension::Pt::new(4.0);

        for (_, frags, style) in &all_endnotes {
            let para = layout::paragraph::layout_paragraph(
                frags,
                &constraints,
                style,
                dlh,
                Some(&measure_fn),
            );
            for mut cmd in para.commands {
                cmd.shift_y(cursor_y);
                cmd.shift_x(last_config.margins.left);
                endnote_page.commands.push(cmd);
            }
            cursor_y += para.size.height;
        }
        all_pages.push(endnote_page);
    }

    if all_pages.is_empty() {
        all_pages.push(LayoutedPage::new(PageConfig::default().page_size));
    }

    all_pages
}

/// Measure each populated header/footer slot independently so pagination can
/// reserve the slot selected for each physical page.
fn measure_header_footer_clearance(
    config: &PageConfig,
    section: &crate::render::resolve::sections::ResolvedSection,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
    even_and_odd: bool,
    logical_page_base: usize,
) -> HeaderFooterClearance {
    let headers =
        HeaderFooterSet {
            default: section.headers.default.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
            first: section.headers.first.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
            even: section.headers.even.as_deref().map(|blocks| {
                measure_header_bottom(blocks, config, ctx, state, default_line_height)
            }),
        };
    let footers =
        HeaderFooterSet {
            default: section.footers.default.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
            first: section.footers.first.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
            even: section.footers.even.as_deref().map(|blocks| {
                measure_footer_extent(blocks, config, ctx, state, default_line_height)
            }),
        };

    HeaderFooterClearance::new(
        config,
        headers,
        footers,
        section.properties.title_page.unwrap_or(false),
        even_and_odd,
        logical_page_base,
    )
}

fn measure_header_bottom(
    blocks: &[crate::model::Block],
    config: &PageConfig,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
) -> dimension::Pt {
    let hf = layout::build::build_header_footer_content(blocks, ctx, state);
    // Height only — no float x is read here, so the parity is immaterial.
    let result = layout::section::stack_blocks(
        &hf.blocks,
        config.content_width(),
        default_line_height,
        None,
        layout::section::PageParity::Odd,
    );
    let blocks_bottom = config.header_margin + result.height;
    let floats_bottom = hf
        .floating_images
        .iter()
        .filter(|fi| fi.is_wrap_top_and_bottom())
        .map(|fi| {
            let y = match fi.y {
                layout::section::FloatingImageY::Absolute(y) => y,
                layout::section::FloatingImageY::RelativeToParagraph(off) => {
                    config.header_margin + off
                }
            };
            y + fi.size.height
        })
        .fold(dimension::Pt::ZERO, |a, b| a.max(b));
    blocks_bottom.max(floats_bottom)
}

fn measure_footer_extent(
    blocks: &[crate::model::Block],
    config: &PageConfig,
    ctx: &layout::build::BuildContext,
    state: &mut BuildState,
    default_line_height: dimension::Pt,
) -> dimension::Pt {
    let hf = layout::build::build_header_footer_content(blocks, ctx, state);
    // Height only — no float x is read here, so the parity is immaterial.
    let result = layout::section::stack_blocks(
        &hf.blocks,
        config.content_width(),
        default_line_height,
        None,
        layout::section::PageParity::Odd,
    );
    let blocks_extent = config.footer_margin + result.height;
    let floats_extent = hf
        .floating_images
        .iter()
        .filter(|fi| fi.is_wrap_top_and_bottom())
        .map(|fi| match fi.y {
            layout::section::FloatingImageY::Absolute(y) => config.page_size.height - y,
            layout::section::FloatingImageY::RelativeToParagraph(off) => {
                config.footer_margin + off + fi.size.height
            }
        })
        .fold(dimension::Pt::ZERO, |a, b| a.max(b));
    blocks_extent.max(floats_extent)
}
