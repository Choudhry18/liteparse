//! `LayoutedPage` draw commands → [`Page`]/[`TextItem`] geometry.
//!
//! The stage-2 tap (see `NATIVE_OFFICE_PLAN.md`): the vendored layout engine
//! ends at per-page [`DrawCommand`] streams in Pt with a top-left origin —
//! already liteparse viewport space, no unit conversion and no y-flip. What a
//! `Text` command does *not* carry is a bounding box: `position` is the
//! baseline origin and there is no width field. Both come from re-measuring
//! with the same [`TextMeasurer`]/[`FontRegistry`] the layout ran with, which
//! reproduces the engine's own advances bit-for-bit.
//!
//! Facts that are geometric inferences on the PDF path arrive here as data:
//! `LinkAnnotation` → [`TextItem::link`], `Outline` marks →
//! [`OutlineTarget`]s. Two deliberate v1 gaps, both documented in the plan:
//! `TextItem::strike` stays `false` (the vendored layout never draws
//! strikethrough lines — markdown strike comes from the block emitter's
//! cascade, which reads the source instead of geometry), and `Image`/`Path`
//! commands feed nothing (no Figure blocks exist on the native markdown path
//! yet; `ParseResult.images` stays empty).

use std::collections::HashMap;

use liteparse_docx::render::fonts::FontRegistry;
use liteparse_docx::render::layout::draw_command::{DrawCommand, LayoutedPage, OutlineMark};
use liteparse_docx::render::layout::fragment::FontProps;
use liteparse_docx::render::layout::measurer::TextMeasurer;

use crate::types::{OutlineTarget, Page, Rect, TextItem, WordBox};

/// Everything the native pipeline taps out of a laid-out document.
pub struct NativeLayout {
    /// One [`Page`] per physical page, in order, with real geometry.
    pub pages: Vec<Page>,
    /// Outline entries in reading order, `page_index` zero-based, `y_pdf` in
    /// PDF user space (bottom-left origin) per the [`OutlineTarget`] contract.
    pub outline: Vec<OutlineTarget>,
    /// Flattened body-block index → zero-based physical page where the
    /// block's first content landed (min page on the rare duplicate).
    pub block_pages: HashMap<usize, usize>,
}

/// Horizontal/vertical fraction of a link rectangle an item must cover for
/// the link to attach. Link rects are per-word, so a run-level item normally
/// covers its words' rects fully; the threshold only guards against grazing
/// overlaps from neighbouring lines.
const LINK_MIN_COVER_FRACTION: f32 = 0.5;

/// Convert laid-out pages into liteparse [`Page`]s + outline + block→page map.
///
/// `registry` must be the same registry `layout_document` ran with — the
/// re-measure only reproduces the engine's line breaks against identical font
/// resolution. `emit_word_boxes` mirrors `LiteParseConfig::emit_word_boxes`:
/// when false, `TextItem::words` stays empty and the per-word prefix measures
/// are skipped entirely.
pub fn layout_to_pages(
    layouted: &[LayoutedPage],
    registry: &FontRegistry,
    emit_word_boxes: bool,
) -> NativeLayout {
    let measurer = TextMeasurer::new(registry);
    let mut pages = Vec::with_capacity(layouted.len());
    let mut outline: Vec<OutlineTarget> = Vec::new();
    let mut block_pages: HashMap<usize, usize> = HashMap::new();

    for (page_idx, lp) in layouted.iter().enumerate() {
        let page_height = lp.page_size.height.raw();
        let mut text_items: Vec<TextItem> = Vec::new();
        let mut links: Vec<(Rect, String)> = Vec::new();
        // Indices into `outline` whose heading bracket is still open and has
        // no y yet — resolved by the first Text command inside the bracket.
        let mut open_headings: Vec<usize> = Vec::new();

        for cmd in &lp.commands {
            match cmd {
                DrawCommand::Text {
                    position,
                    text,
                    font_family,
                    char_spacing,
                    font_size,
                    bold,
                    italic,
                    color,
                    text_scale,
                } => {
                    let props = FontProps {
                        family: font_family.clone(),
                        size: *font_size,
                        bold: *bold,
                        italic: *italic,
                        underline: false,
                        char_spacing: *char_spacing,
                        text_scale: *text_scale,
                        underline_position: liteparse_docx::render::dimension::Pt::ZERO,
                        underline_thickness: liteparse_docx::render::dimension::Pt::ZERO,
                    };
                    let (advance, metrics) = measurer.measure(text, &props);
                    let ascent = metrics.ascent.raw();
                    let descent = metrics.descent.raw();
                    let x = position.x.raw();
                    let top = position.y.raw() - ascent;
                    let width = advance.raw();
                    let height = ascent + descent;

                    let words = if emit_word_boxes {
                        word_boxes(text, &props, &measurer, x, top, height)
                    } else {
                        Vec::new()
                    };

                    let item = TextItem {
                        text: text.to_string(),
                        x,
                        y: top,
                        width,
                        height,
                        rotation: 0.0,
                        font_name: Some(font_family.to_string()),
                        font_size: Some(font_size.raw()),
                        // The PDF path's font_height is font_size × text-matrix
                        // y-scale; the native path has no CTM, so scale is 1.
                        font_height: Some(font_size.raw()),
                        font_ascent: Some(ascent),
                        // PDFium's convention: descent is negative below the
                        // baseline. `TextMetrics.descent` is positive-down.
                        font_descent: Some(-descent),
                        font_weight: Some(if *bold { 700 } else { 400 }),
                        text_width: Some(width),
                        fill_color: Some(format!(
                            "ff{:02x}{:02x}{:02x}",
                            color.r, color.g, color.b
                        )),
                        words,
                        ..TextItem::default()
                    };

                    for &oi in &open_headings {
                        if outline[oi].y_pdf.is_none() {
                            outline[oi].y_pdf = Some(page_height - top);
                        }
                    }

                    text_items.push(item);
                }
                DrawCommand::EmojiCluster {
                    rect, text, size, ..
                } => {
                    text_items.push(TextItem {
                        text: text.clone(),
                        x: rect.origin.x.raw(),
                        y: rect.origin.y.raw(),
                        width: rect.size.width.raw(),
                        height: rect.size.height.raw(),
                        font_size: Some(size.raw()),
                        ..TextItem::default()
                    });
                }
                DrawCommand::LinkAnnotation { rect, url } => {
                    links.push((
                        Rect {
                            x: rect.origin.x.raw(),
                            y: rect.origin.y.raw(),
                            width: rect.size.width.raw(),
                            height: rect.size.height.raw(),
                        },
                        url.to_string(),
                    ));
                }
                DrawCommand::Outline(mark) => match mark {
                    OutlineMark::Begin(h) => {
                        outline.push(OutlineTarget {
                            level: h.level.value(),
                            title: h.title.to_string(),
                            page_index: page_idx as i32,
                            y_pdf: None,
                        });
                        open_headings.push(outline.len() - 1);
                    }
                    OutlineMark::End => {
                        open_headings.pop();
                    }
                },
                // Underline/strike: the vendored layout emits underlines but
                // never strikethrough; TextItem has no underline field and
                // strike detection over border/separator lines would only
                // false-positive. Images/paths/rects: deferred (module docs).
                DrawCommand::Underline { .. }
                | DrawCommand::Line { .. }
                | DrawCommand::Rect { .. }
                | DrawCommand::Image { .. }
                | DrawCommand::Path { .. }
                | DrawCommand::InternalLink { .. }
                | DrawCommand::NamedDestination { .. } => {}
            }
        }

        assign_links(&mut text_items, &links);

        for &b in &lp.block_starts {
            block_pages
                .entry(b)
                .and_modify(|p| *p = (*p).min(page_idx))
                .or_insert(page_idx);
        }

        let content_bounds = union_bounds(&text_items);
        pages.push(Page {
            page_number: page_idx + 1,
            page_width: lp.page_size.width.raw(),
            page_height,
            content_bounds,
            text_items,
            graphics: Vec::new(),
            vector_graphics: None,
            struct_nodes: Vec::new(),
            image_refs: Vec::new(),
            annotations: None,
            form_fields: None,
            structure_tree: None,
        });
    }

    NativeLayout {
        pages,
        outline,
        block_pages,
    }
}

/// Per-word boxes via prefix re-measure. The measurer's advance arithmetic is
/// linear in the string (cmap advances + per-char spacing, no shaping), so a
/// prefix measure is exact — `width(a+b) == width(a) + width(b)`.
fn word_boxes(
    text: &str,
    props: &FontProps,
    measurer: &TextMeasurer<'_>,
    item_x: f32,
    item_y: f32,
    item_height: f32,
) -> Vec<WordBox> {
    let mut words = Vec::new();
    let mut search_from = 0usize;
    for word in text.split_whitespace() {
        // Locate this word's byte range (split_whitespace loses offsets).
        let rel = match text[search_from..].find(word) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let start = search_from + rel;
        let end = start + word.len();
        search_from = end;
        let x0 = measurer.measure(&text[..start], props).0.raw();
        let x1 = measurer.measure(&text[..end], props).0.raw();
        words.push(WordBox {
            text: word.to_string(),
            x: item_x + x0,
            y: item_y,
            width: x1 - x0,
            height: item_height,
        });
    }
    words
}

/// Attach link URLs to the items covering each link rectangle. Rects come one
/// per word from the layout engine; an item takes the first rect it covers.
fn assign_links(items: &mut [TextItem], links: &[(Rect, String)]) {
    if links.is_empty() {
        return;
    }
    for item in items.iter_mut() {
        if item.link.is_some() {
            continue;
        }
        for (r, url) in links {
            let ox = (item.x + item.width).min(r.x + r.width) - item.x.max(r.x);
            let oy = (item.y + item.height).min(r.y + r.height) - item.y.max(r.y);
            if ox >= r.width * LINK_MIN_COVER_FRACTION && oy >= r.height * LINK_MIN_COVER_FRACTION {
                item.link = Some(url.clone());
                break;
            }
        }
    }
}

fn union_bounds(items: &[TextItem]) -> Option<Rect> {
    let mut it = items.iter();
    let first = it.next()?;
    let (mut x0, mut y0) = (first.x, first.y);
    let (mut x1, mut y1) = (first.x + first.width, first.y + first.height);
    for i in it {
        x0 = x0.min(i.x);
        y0 = y0.min(i.y);
        x1 = x1.max(i.x + i.width);
        y1 = y1.max(i.y + i.height);
    }
    Some(Rect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use liteparse_docx::render::dimension::Pt;
    use liteparse_docx::render::geometry::{PtOffset, PtRect, PtSize};
    use liteparse_docx::render::layout::draw_command::OutlineHeading;
    use liteparse_docx::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn registry() -> FontRegistry {
        FontRegistry::new()
    }

    fn text_cmd(x: f32, y: f32, text: &str, size: f32) -> DrawCommand {
        DrawCommand::Text {
            position: PtOffset::new(Pt::new(x), Pt::new(y)),
            text: Rc::from(text),
            font_family: Rc::from("Arial"),
            char_spacing: Pt::ZERO,
            font_size: Pt::new(size),
            bold: false,
            italic: false,
            color: RgbColor::BLACK,
            text_scale: 1.0,
        }
    }

    fn page(commands: Vec<DrawCommand>) -> LayoutedPage {
        LayoutedPage {
            commands,
            page_size: PtSize::new(Pt::new(612.0), Pt::new(792.0)),
            block_starts: Vec::new(),
        }
    }

    #[test]
    fn text_bbox_hangs_from_the_baseline_by_ascent() {
        let reg = registry();
        let measurer = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let (advance, metrics) = measurer.measure("Hello", &props);

        let out = layout_to_pages(
            &[page(vec![text_cmd(72.0, 100.0, "Hello", 12.0)])],
            &reg,
            true,
        );
        let item = &out.pages[0].text_items[0];
        assert_eq!(item.x, 72.0);
        assert_eq!(item.y, 100.0 - metrics.ascent.raw());
        assert_eq!(item.width, advance.raw());
        assert_eq!(item.height, metrics.ascent.raw() + metrics.descent.raw());
        assert_eq!(item.font_descent, Some(-metrics.descent.raw()));
        assert_eq!(out.pages[0].page_number, 1);
        // Word boxes partition the advance: two words, gap between them.
        let out2 = layout_to_pages(
            &[page(vec![text_cmd(0.0, 100.0, "ab cd", 12.0)])],
            &reg,
            true,
        );
        let words = &out2.pages[0].text_items[0].words;
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].x, 0.0);
        assert!(
            words[1].x > words[0].x + words[0].width,
            "gap for the space"
        );
    }

    #[test]
    fn links_attach_to_covered_items_only() {
        let reg = registry();
        let m = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let (w, metrics) = m.measure("click", &props);
        let link_rect = PtRect::from_xywh(
            Pt::new(72.0),
            Pt::new(100.0 - metrics.ascent.raw()),
            w,
            Pt::new(metrics.ascent.raw() + metrics.descent.raw()),
        );
        let out = layout_to_pages(
            &[page(vec![
                text_cmd(72.0, 100.0, "click", 12.0),
                text_cmd(72.0, 300.0, "plain", 12.0),
                DrawCommand::LinkAnnotation {
                    rect: link_rect,
                    url: Rc::from("https://example.com"),
                },
            ])],
            &reg,
            false,
        );
        let items = &out.pages[0].text_items;
        assert_eq!(items[0].link.as_deref(), Some("https://example.com"));
        assert_eq!(items[1].link, None);
    }

    #[test]
    fn outline_marks_become_targets_with_pdf_space_y() {
        let reg = registry();
        let m = TextMeasurer::new(&reg);
        let props = FontProps {
            family: Rc::from("Arial"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: 1.0,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        };
        let ascent = m.measure("Title", &props).1.ascent.raw();
        let out = layout_to_pages(
            &[page(vec![
                DrawCommand::Outline(OutlineMark::Begin(OutlineHeading {
                    node_id: 1,
                    level: liteparse_docx::model::OutlineLevel::new(2),
                    title: Rc::from("Title"),
                })),
                text_cmd(72.0, 100.0, "Title", 12.0),
                DrawCommand::Outline(OutlineMark::End),
            ])],
            &reg,
            false,
        );
        assert_eq!(out.outline.len(), 1);
        let t = &out.outline[0];
        assert_eq!((t.level, t.page_index), (2, 0));
        assert_eq!(t.title, "Title");
        // Bottom-left PDF space: page_height − viewport top of the heading.
        assert_eq!(t.y_pdf, Some(792.0 - (100.0 - ascent)));
    }

    #[test]
    fn block_starts_become_a_min_page_map() {
        let reg = registry();
        let mut p0 = page(vec![]);
        p0.block_starts = vec![3, 4];
        let mut p1 = page(vec![]);
        p1.block_starts = vec![4, 5];
        let out = layout_to_pages(&[p0, p1], &reg, false);
        assert_eq!(out.block_pages[&3], 0);
        assert_eq!(out.block_pages[&4], 0, "duplicate takes the earliest page");
        assert_eq!(out.block_pages[&5], 1);
    }
}
