use std::rc::Rc;

use crate::model::{self, Block, Paragraph};
use crate::render::dimension::Pt;
use crate::render::layout::fragment::{FontProps, Fragment, FragmentCtx, collect_fragments};
use crate::render::layout::paragraph::DropCapInfo;
use crate::render::layout::section::LayoutBlock;
use crate::render::resolve::color::{ColorContext, RgbColor, resolve_color};
use crate::render::resolve::conditional::CellConditionalFormatting;
use crate::render::resolve::properties::{merge_paragraph_properties, merge_run_properties};
use crate::render::resolve::styles::ResolvedStyle;

use super::convert::{
    paragraph_style_from_props, populate_image_data, populate_underline_metrics,
    resolve_paragraph_defaults,
};
use super::floating::{AnchorFrame, extract_floating_images};
use super::table::build_table;
use super::{BuildContext, BuildState};

/// Recursively process a single model block into a layout block.
///
/// Returns `None` for drop cap paragraphs (consumed by the next paragraph)
/// and section breaks (already handled by resolve).
pub(super) fn build_block(
    block: &Block,
    available_width: Pt,
    ctx: &BuildContext,
    state: &mut BuildState,
    pending_dropcap: &mut Option<DropCapInfo>,
) -> Option<LayoutBlock> {
    match block {
        Block::Paragraph(p) => build_paragraph_block(p, ctx, state, pending_dropcap, None, None),
        Block::Table(t) => {
            let built = build_table(t, available_width, ctx, state);
            Some(LayoutBlock::Table {
                rows: built.rows,
                col_widths: built.col_widths,
                cell_spacing: built.cell_spacing,
                border_config: built.border_config,
                indent: built.indent,
                alignment: built.alignment,
                float_info: built.float_info,
                style_id: t.properties.style_id.clone(),
            })
        }
        Block::SectionBreak(_) => None,
    }
}

// ── Paragraph building ──────────────────────────────────────────────────────

/// Build a paragraph into a layout block.
/// Handles drop cap detection (§17.3.1.11), list labels, floating images.
/// For table cells, pass `table_style` and `cond` to apply table formatting cascade.
pub(super) fn build_paragraph_block(
    p: &Paragraph,
    ctx: &BuildContext,
    state: &mut BuildState,
    pending_dropcap: &mut Option<DropCapInfo>,
    table_style: Option<&ResolvedStyle>,
    cond: Option<&CellConditionalFormatting>,
) -> Option<LayoutBlock> {
    let (mut fragments, mut merged_props) = build_fragments(p, ctx, state, table_style, cond);
    // Drain immediately: this paragraph owns exactly the references its own
    // fragment collection recorded. Draining before the drop-cap early return
    // below keeps them from leaking into the next paragraph's batch, and
    // before `build_note_content` re-enters `build_fragments` (a footnote body
    // may itself carry references) from mixing the two levels together.
    let fn_refs = state.footnotes.take_pending();

    // §17.9.22: inject list label if paragraph has a numbering reference.
    super::list_label::inject_list_label(p, &mut fragments, &mut merged_props, ctx, state);

    // §17.3.1.29: a paragraph with no runs still occupies one line — the
    // paragraph mark (¶) has a font-sized line height. Inject a LineBreak
    // so the layout phase treats it as a real line. Without this, an empty
    // paragraph's fragments are split by `split_at_page_breaks` into a
    // single empty page-chunk, which `section::layout_section` drops,
    // collapsing the line to zero height.
    //
    // Table cells use §17.4.66 (trailing-empty-after-table is structural
    // and suppressed) in `build_cell_blocks` — that skip runs before this
    // function, so genuinely structural terminators never reach us.
    // Headers/footers have their own injection in
    // `build_header_footer_content` (§17.10.1) and do not call this.
    if fragments.is_empty() {
        let (family, mut size, ..) = resolve_paragraph_defaults(
            p,
            ctx.resolved,
            table_style.is_some(),
            state.shape_default_text_color,
            state.shape_default_font_family.as_deref(),
        );
        if let Some(ref mrp) = p.mark_run_properties {
            if let Some(fs) = mrp.font_size {
                size = Pt::from(fs);
            }
        }
        let line_height = ctx.measurer.default_line_height(&family, size);
        fragments.push(Fragment::LineBreak { line_height });
    }

    // Word suppresses Hyperlink character style (blue/underline) for ToC
    // entries in print view. Strip visual hyperlink styling but keep the
    // click annotation URL.
    //
    // §17.7.4.9: identified by the resolved style's *primary style name*
    // (`toc 1` … `toc 9`, locale-independent), not by the `w:styleId`
    // spelling — a `starts_with("TOC")` test both over-matches (an unrelated
    // user style `TOCustom`) and under-matches (producers that don't spell
    // their ToC style IDs `TOC1`).
    let is_toc_entry = p
        .style_id
        .as_ref()
        .and_then(|id| ctx.resolved.styles.get(id))
        .is_some_and(|s| s.is_toc_entry);
    if is_toc_entry {
        for frag in &mut fragments {
            if let Fragment::Text {
                font,
                color,
                hyperlink_url,
                ..
            } = frag
            {
                if hyperlink_url.is_some() {
                    *color = RgbColor::BLACK;
                    // Rare (TOC hyperlinks only); clones this fragment's shared
                    // font on write.
                    Rc::make_mut(font).underline = false;
                }
            }
        }
    }

    // §17.3.1.11: detect drop cap paragraph.
    if let Some(model::FrameKind::DropCap {
        style,
        lines,
        h_space: dc_h_space,
    }) = merged_props.frame_properties
    {
        let drop_cap_lines = lines;
        let width: Pt = fragments.iter().map(|f| f.width()).sum();
        let height: Pt = fragments.iter().map(|f| f.height()).fold(Pt::ZERO, Pt::max);
        let ascent: Pt = fragments
            .iter()
            .map(|f| match f {
                Fragment::Text { metrics, .. } => metrics.ascent,
                _ => Pt::ZERO,
            })
            .fold(Pt::ZERO, Pt::max);
        let h_space = dc_h_space.map(Pt::from).unwrap_or(Pt::ZERO);
        let margin_mode = matches!(style, model::DropCap::Margin);
        // The drop cap paragraph's own indent determines the x position.
        // This includes indent_left + indent_first_line from the cascade.
        let dc_indent_left = merged_props
            .indentation
            .and_then(|i| i.start)
            .map(Pt::from)
            .unwrap_or(Pt::ZERO);
        let dc_indent_first = merged_props
            .indentation
            .and_then(|i| i.first_line)
            .map(|fl| match fl {
                model::FirstLineIndent::FirstLine(v) => Pt::from(v),
                model::FirstLineIndent::Hanging(v) => -Pt::from(v),
                model::FirstLineIndent::None => Pt::ZERO,
            })
            .unwrap_or(Pt::ZERO);
        // §17.3.1.33: frame height from drop cap paragraph's exact line spacing.
        let frame_height = merged_props
            .spacing
            .and_then(|s| s.line)
            .and_then(|ls| match ls {
                model::LineSpacing::Exact(v) => Some(Pt::from(v)),
                _ => None,
            });
        // §17.3.2.19: position offset from the drop cap run.
        let position_offset = fragments
            .first()
            .and_then(|f| match f {
                Fragment::Text {
                    baseline_offset, ..
                } => Some(*baseline_offset),
                _ => None,
            })
            .unwrap_or(Pt::ZERO);
        *pending_dropcap = Some(DropCapInfo {
            fragments,
            lines: drop_cap_lines,
            ascent,
            h_space,
            width,
            height,
            margin_mode,
            indent: dc_indent_left + dc_indent_first,
            frame_height,
            position_offset,
        });
        return None;
    }

    let outline = super::convert::paragraph_outline(p, &merged_props, state);
    let mut style = paragraph_style_from_props(
        &merged_props,
        Pt::from(ctx.resolved.default_tab_stop),
        state.shape_auto_fit,
        super::convert::paragraph_locale(p, ctx.resolved),
        outline,
    );
    style.style_id = p.style_id.clone();

    // Attach pending drop cap to this paragraph.
    if let Some(dc) = pending_dropcap.take() {
        style.drop_cap = Some(dc);
    }

    let page_break_before = merged_props.page_break_before.unwrap_or(false);

    // §17.11.12: render a body for each footnote this paragraph referenced.
    // `collect_fragments` recorded them — id *and* the display number it
    // emitted as the superscript — as it walked, so the body and the mark
    // cannot disagree. Draining here also covers references nested inside
    // hyperlinks, fields, and text boxes, which the previous flat scan of
    // `p.content` missed entirely.
    let mut para_footnotes = Vec::new();
    for note in fn_refs {
        if let Some(content) = ctx.resolved.footnotes.get(&note.id) {
            let display = format!("{}", note.display);
            let notes = build_note_content(&display, content, ctx, state);
            for (_, frags, style) in notes {
                para_footnotes.push((frags, style));
            }
        }
    }

    // §20.4.2.3: extract floating (anchor) images and shapes from this
    // paragraph. Table cells emit their commands through `stack_blocks`,
    // which shifts them into page coordinates, so anchors inside a cell use
    // the stack frame. Body paragraphs emit in page-absolute coordinates.
    let frame = if table_style.is_some() {
        AnchorFrame::Stack
    } else {
        AnchorFrame::Page
    };
    let floating_images = extract_floating_images(p, ctx, state, frame);
    let floating_shapes = super::floating::extract_floating_shapes(
        p,
        ctx,
        state,
        frame,
        super::floating::ShapeAnchorClass::All,
    );

    Some(LayoutBlock::Paragraph {
        fragments,
        style,
        page_break_before,
        footnotes: para_footnotes,
        floating_images,
        floating_shapes,
    })
}

/// Build note content (footnotes or endnotes) with a display number prefix.
pub(super) fn build_note_content(
    display_num: &str,
    content: &[Block],
    ctx: &BuildContext,
    state: &mut BuildState,
) -> Vec<(
    String,
    Vec<Fragment>,
    crate::render::layout::paragraph::ParagraphStyle,
)> {
    // §17.3.1.19: a footnote or endnote body is not the document's main story,
    // so nothing in it is an outline position. Suspended for the whole call and
    // restored, for the same reason `build_header_footer_content` does it there
    // rather than at the paragraph — a `Block::Table` in a note reaches the
    // ordinary body builders.
    let outer = std::mem::replace(
        &mut state.outline,
        crate::render::layout::build::OutlineCollector::Excluded,
    );
    let results = build_note_blocks(display_num, content, ctx, state);
    state.outline = outer;
    results
}

fn build_note_blocks(
    display_num: &str,
    content: &[Block],
    ctx: &BuildContext,
    state: &mut BuildState,
) -> Vec<(
    String,
    Vec<Fragment>,
    crate::render::layout::paragraph::ParagraphStyle,
)> {
    let mut results = Vec::new();
    for (i, block) in content.iter().enumerate() {
        if let model::Block::Paragraph(p) = block {
            let (mut frags, merged_props) = build_fragments(p, ctx, state, None, None);
            // §17.11.12: a footnote body may itself carry references. We don't
            // render nested footnote bodies (matching the previous behaviour),
            // but the references must be drained so they aren't attributed to
            // the paragraph that hosts this note.
            let _ = state.footnotes.take_pending();

            // Prepend display number to the first paragraph.
            if i == 0 && !frags.is_empty() {
                let num_text = format!("{}  ", display_num);
                // §17.8.3.2 / §17.3.2.14: fall back to the document-level spec
                // defaults rather than restating a font name here.
                let font = frags[0].font_props().cloned().unwrap_or_else(|| FontProps {
                    family: std::rc::Rc::from(super::SPEC_FALLBACK_FONT),
                    size: super::SPEC_DEFAULT_FONT_SIZE,
                    bold: false,
                    italic: false,
                    underline: false,
                    char_spacing: Pt::ZERO,
                    text_scale: 1.0,
                    underline_position: Pt::ZERO,
                    underline_thickness: Pt::ZERO,
                });
                let ref_size =
                    font.size * crate::render::layout::fragment::SUPERSCRIPT_FONT_SIZE_RATIO;
                let ref_font = FontProps {
                    size: ref_size,
                    ..font
                };
                let (w, m) = ctx.measurer.measure(&num_text, &ref_font);
                frags.insert(
                    0,
                    Fragment::Text {
                        text: Rc::from(num_text.as_str()),
                        font: Rc::new(ref_font),
                        color: RgbColor::BLACK,
                        shading: None,
                        border: None,
                        width: w,
                        trimmed_width: w,
                        metrics: m,
                        hyperlink_url: None,
                        baseline_offset: -(font.size
                            * crate::render::layout::fragment::NOTE_REF_BASELINE_OFFSET_RATIO),
                        text_offset: Pt::ZERO,
                        is_footnote_ref: false,
                    },
                );
            }
            let style = paragraph_style_from_props(
                &merged_props,
                Pt::from(ctx.resolved.default_tab_stop),
                state.shape_auto_fit,
                super::convert::paragraph_locale(p, ctx.resolved),
                // §17.3.1.19: always `None` — the collector is suspended for
                // this whole call. Asking rather than passing `None` keeps the
                // heading decision in one place for every path.
                super::convert::paragraph_outline(p, &merged_props, state),
            );
            results.push((display_num.to_string(), frags, style));
        }
    }
    results
}

/// Collect endnotes from the resolved document.
pub(super) fn collect_endnotes(
    ctx: &BuildContext,
    state: &mut BuildState,
    endnotes: &mut Vec<(
        String,
        Vec<Fragment>,
        crate::render::layout::paragraph::ParagraphStyle,
    )>,
) {
    // IDs 0 and 1 are reserved for separator and continuation separator.
    let mut en_ids: Vec<_> = ctx
        .resolved
        .endnotes
        .keys()
        .filter(|id| id.value() > 1)
        .collect();
    en_ids.sort_by_key(|id| id.value());
    for (i, note_id) in en_ids.iter().enumerate() {
        let display = crate::render::layout::fragment::to_roman_lower((i + 1) as u32);
        if let Some(content) = ctx.resolved.endnotes.get(note_id) {
            endnotes.extend(build_note_content(&display, content, ctx, state));
        }
    }
}

/// Build fragments and resolved paragraph properties for a paragraph.
///
/// Handles the full cascade: table style → conditional → paragraph style →
/// doc defaults → fragment collection → image/underline population.
pub(super) fn build_fragments(
    para: &Paragraph,
    ctx: &BuildContext,
    state: &mut BuildState,
    table_style: Option<&ResolvedStyle>,
    cond: Option<&CellConditionalFormatting>,
) -> (Vec<Fragment>, model::ParagraphProperties) {
    // §17.7.2: resolve paragraph defaults (direct → paragraph style).
    // Doc defaults are deferred so table style/conditional can be inserted
    // between paragraph style and doc defaults in the cascade.
    let (default_family, mut default_size, mut default_color, mut merged_props, mut run_defaults) =
        resolve_paragraph_defaults(
            para,
            ctx.resolved,
            table_style.is_some(),
            state.shape_default_text_color,
            state.shape_default_font_family.as_deref(),
        );

    // §17.7.2: table conditional formatting — lower priority than paragraph style.
    if let Some(c) = cond {
        if let Some(ref pp) = c.paragraph_properties {
            merge_paragraph_properties(&mut merged_props, pp);
        }
    }
    // §17.7.2: table style paragraph properties — lower priority than conditional.
    if let Some(ts) = table_style {
        merge_paragraph_properties(&mut merged_props, &ts.paragraph);
    }
    // §17.7.2: doc defaults — lowest priority, deferred from resolve_paragraph_defaults.
    if table_style.is_some() {
        merge_paragraph_properties(&mut merged_props, &ctx.resolved.doc_defaults_paragraph);
    }

    // §17.7.2: table style run properties override Normal.
    if let Some(ts) = table_style {
        if let Some(fs) = ts.run.font_size {
            default_size = Pt::from(fs);
            run_defaults.font_size = Some(fs);
        }
    }

    // §17.7.6: conditional run property overrides — higher priority than
    // table style and paragraph style. Overlay (not merge): conditional
    // values replace existing ones.
    if let Some(c) = cond {
        if let Some(ref rp) = c.run_properties {
            // Overlay: for each Some field in rp, replace in run_defaults.
            let mut overlay = rp.clone();
            merge_run_properties(&mut overlay, &run_defaults);
            run_defaults = overlay;
            if let Some(fs) = run_defaults.font_size {
                default_size = Pt::from(fs);
            }
            if let Some(color) = run_defaults.color {
                default_color = resolve_color(color, ColorContext::Text);
            }
        }
    }

    let measure =
        |text: &str, font: &FontProps| -> (Pt, crate::render::layout::fragment::TextMetrics) {
            ctx.measurer.measure(text, font)
        };

    let frag_ctx = FragmentCtx {
        default_family: &default_family,
        default_size,
        default_color,
        resolved_styles: Some(&ctx.resolved.styles),
        paragraph_run_defaults: Some(&run_defaults),
        theme: ctx.resolved.theme.as_ref(),
        measurer: Some(ctx.measurer),
        auto_fit: state.shape_auto_fit,
    };
    let mut fragments = collect_fragments(
        &para.content,
        &frag_ctx,
        None,
        &measure,
        &mut state.footnotes,
        &mut state.endnote_counter,
        state.field_ctx,
    );
    populate_image_data(&mut fragments, ctx.media());
    populate_underline_metrics(&mut fragments, ctx.measurer);

    (fragments, merged_props)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::model::dimension::Dimension;
    use crate::render::fonts::FontRegistry;
    use crate::render::layout::measurer::TextMeasurer;
    use crate::render::resolve::ResolvedDocument;

    fn empty_resolved() -> ResolvedDocument {
        ResolvedDocument {
            sections: Vec::new(),
            styles: HashMap::new(),
            numbering: HashMap::new(),
            font_families: Vec::new(),
            media: HashMap::new(),
            embedded_fonts: Vec::new(),
            pic_bullets: HashMap::new(),
            theme: None,
            doc_defaults_paragraph: model::ParagraphProperties::default(),
            doc_defaults_run: model::RunProperties::default(),
            default_paragraph_style_id: None,
            footnotes: HashMap::new(),
            endnotes: HashMap::new(),
            even_and_odd_headers: false,
            default_tab_stop: Dimension::new(720),
        }
    }

    fn para(content: Vec<model::Inline>) -> Paragraph {
        Paragraph {
            style_id: None,
            properties: model::ParagraphProperties::default(),
            mark_run_properties: None,
            content,
            rsids: model::ParagraphRevisionIds::default(),
        }
    }

    fn text_run(s: &str) -> model::Inline {
        model::Inline::TextRun(Box::new(model::TextRun {
            style_id: None,
            properties: model::RunProperties::default(),
            content: vec![model::RunElement::Text(s.to_string())],
            rsids: model::RevisionIds::default(),
        }))
    }

    /// Run a closure with a live `BuildContext` + `BuildState`. A real Skia
    /// measurer is used, so assertions below are structural — never on
    /// platform-dependent metric values.
    fn with_ctx<R>(
        resolved: &ResolvedDocument,
        f: impl FnOnce(&BuildContext, &mut BuildState) -> R,
    ) -> R {
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let ctx = BuildContext {
            measurer: &measurer,
            resolved,
        };
        let mut state = BuildState::default();
        f(&ctx, &mut state)
    }

    fn resolved_style(paragraph: model::ParagraphProperties) -> ResolvedStyle {
        ResolvedStyle {
            paragraph,
            run: model::RunProperties::default(),
            table: None,
            table_style_overrides: Vec::new(),
            is_toc_entry: false,
        }
    }

    #[test]
    fn section_break_produces_no_layout_block() {
        let resolved = empty_resolved();
        with_ctx(&resolved, |ctx, state| {
            let block = Block::SectionBreak(Box::default());
            let mut pending = None;
            assert!(
                build_block(&block, Pt::new(400.0), ctx, state, &mut pending).is_none(),
                "section breaks are consumed by resolve, not laid out"
            );
        });
    }

    /// §17.3.1.29: a paragraph with no runs still occupies one line. Without an
    /// injected `LineBreak` the fragment list is empty and `layout_section`
    /// drops the paragraph, collapsing it to zero height.
    #[test]
    fn empty_paragraph_gets_a_line_break_fragment() {
        let resolved = empty_resolved();
        with_ctx(&resolved, |ctx, state| {
            let mut pending = None;
            let block = build_paragraph_block(&para(vec![]), ctx, state, &mut pending, None, None)
                .expect("empty paragraph still lays out");
            let LayoutBlock::Paragraph { fragments, .. } = block else {
                panic!("expected a paragraph block");
            };
            assert!(
                matches!(fragments.as_slice(), [Fragment::LineBreak { line_height }] if line_height.raw() > 0.0),
                "exactly one LineBreak with a real height"
            );
        });
    }

    #[test]
    fn paragraph_with_content_gets_no_injected_line_break() {
        let resolved = empty_resolved();
        with_ctx(&resolved, |ctx, state| {
            let mut pending = None;
            let block = build_paragraph_block(
                &para(vec![text_run("hi")]),
                ctx,
                state,
                &mut pending,
                None,
                None,
            )
            .expect("lays out");
            let LayoutBlock::Paragraph { fragments, .. } = block else {
                panic!("expected a paragraph block");
            };
            assert!(
                !fragments
                    .iter()
                    .any(|f| matches!(f, Fragment::LineBreak { .. })),
                "no LineBreak injected when the paragraph has runs"
            );
        });
    }

    /// §17.3.1.11: a drop-cap paragraph emits no block of its own — it is held
    /// aside and attached to the *following* paragraph.
    #[test]
    fn drop_cap_paragraph_is_deferred_onto_the_next_paragraph() {
        let resolved = empty_resolved();
        with_ctx(&resolved, |ctx, state| {
            let mut cap = para(vec![text_run("D")]);
            cap.properties.frame_properties = Some(model::FrameKind::DropCap {
                style: model::DropCap::Drop,
                lines: 3,
                h_space: None,
            });

            let mut pending = None;
            assert!(
                build_paragraph_block(&cap, ctx, state, &mut pending, None, None).is_none(),
                "the drop-cap paragraph itself produces no block"
            );
            let held = pending
                .as_ref()
                .expect("drop cap held for the next paragraph");
            assert_eq!(held.lines, 3, "line span carried through");

            // The next paragraph consumes it.
            let next = build_paragraph_block(
                &para(vec![text_run("body")]),
                ctx,
                state,
                &mut pending,
                None,
                None,
            )
            .expect("lays out");
            let LayoutBlock::Paragraph { style, .. } = next else {
                panic!("expected a paragraph block");
            };
            assert!(
                style.drop_cap.is_some(),
                "attached to the following paragraph"
            );
            assert!(pending.is_none(), "and taken, so it attaches only once");
        });
    }

    /// §17.7.2 precedence inside a table cell: conditional formatting outranks
    /// the table style, which outranks document defaults. Asserted on the
    /// merged paragraph properties returned by `build_fragments`.
    #[test]
    fn conditional_formatting_outranks_table_style() {
        let mut resolved = empty_resolved();
        resolved.doc_defaults_paragraph.alignment = Some(model::Alignment::Start);
        let table_style = resolved_style(model::ParagraphProperties {
            alignment: Some(model::Alignment::Center),
            ..Default::default()
        });

        with_ctx(&resolved, |ctx, state| {
            // Table style alone.
            let (_, props) = build_fragments(
                &para(vec![text_run("x")]),
                ctx,
                state,
                Some(&table_style),
                None,
            );
            assert_eq!(
                props.alignment,
                Some(model::Alignment::Center),
                "table style beats doc defaults"
            );

            // Conditional formatting on top of it.
            let cond = CellConditionalFormatting {
                cell_properties: None,
                run_properties: None,
                paragraph_properties: Some(model::ParagraphProperties {
                    alignment: Some(model::Alignment::End),
                    ..Default::default()
                }),
            };
            let (_, props) = build_fragments(
                &para(vec![text_run("x")]),
                ctx,
                state,
                Some(&table_style),
                Some(&cond),
            );
            assert_eq!(
                props.alignment,
                Some(model::Alignment::End),
                "conditional formatting beats the table style"
            );
        });
    }

    /// The paragraph's own direct formatting outranks every table-level layer.
    #[test]
    fn direct_paragraph_properties_outrank_conditional_formatting() {
        let resolved = empty_resolved();
        let table_style = resolved_style(model::ParagraphProperties {
            alignment: Some(model::Alignment::Center),
            ..Default::default()
        });
        let cond = CellConditionalFormatting {
            cell_properties: None,
            run_properties: None,
            paragraph_properties: Some(model::ParagraphProperties {
                alignment: Some(model::Alignment::End),
                ..Default::default()
            }),
        };

        with_ctx(&resolved, |ctx, state| {
            let mut p = para(vec![text_run("x")]);
            p.properties.alignment = Some(model::Alignment::Both);
            let (_, props) = build_fragments(&p, ctx, state, Some(&table_style), Some(&cond));
            assert_eq!(
                props.alignment,
                Some(model::Alignment::Both),
                "direct pPr wins over every table layer"
            );
        });
    }
}
