//! List label injection — prepend bullet/number labels to paragraph fragments.
//!
//! §17.9.22: when a paragraph carries a numbering reference, resolve the label
//! text (or picture bullet) and inject it as the first fragment, followed by a
//! tab that advances to the body text indent position.

use std::rc::Rc;

use crate::model::{self, ParagraphProperties};
use crate::render::dimension::Pt;
use crate::render::layout::fragment::Fragment;

use super::convert::{pic_bullet_size, remap_legacy_font_chars, resolve_paragraph_defaults};
use super::{BuildContext, BuildState};

/// Inject list label fragments into a paragraph if it has a numbering reference.
///
/// Updates `fragments` (prepends label + tab), `merged_props` (overrides indentation
/// from the numbering level), and `state.list_counters` (increments/resets counters).
pub(super) fn inject_list_label(
    para: &model::Paragraph,
    fragments: &mut Vec<Fragment>,
    merged_props: &mut ParagraphProperties,
    ctx: &BuildContext,
    state: &mut BuildState,
) {
    let num_ref = match merged_props.numbering {
        Some(ref nr) => nr,
        None => return,
    };

    let num_id = model::NumId::new(num_ref.num_id);
    let level = num_ref.level;

    let levels = match ctx.resolved.numbering.get(&num_id) {
        Some(levels) => levels,
        None => return,
    };

    // Update counters: increment this level, reset deeper levels.
    //
    // The map holds the *current* (last-emitted) value, which is what
    // `format_list_label` reads. §17.9.28 permits `w:start="0"` (and
    // §17.9.27 `w:startOverride="0"`), so the first item must seed the
    // counter with `start` directly — seeding `start - 1` and adding one
    // underflows `u32` at zero, panicking debug builds and silently
    // wrapping in release.
    {
        use std::collections::hash_map::Entry;
        let counters = &mut state.list_counters;
        match counters.entry((num_id, level)) {
            Entry::Vacant(slot) => {
                slot.insert(levels.get(level as usize).map(|l| l.start).unwrap_or(1));
            }
            Entry::Occupied(mut slot) => {
                let next = slot.get().saturating_add(1);
                slot.insert(next);
            }
        }
        // Reset deeper levels.
        let max_level = levels.len() as u8;
        for deeper in (level + 1)..max_level {
            counters.remove(&(num_id, deeper));
        }
    }

    let level_def = levels.get(level as usize);

    // §17.9.10: check for picture bullet before text label.
    let pic_bullet_injected = level_def
        .and_then(|l| l.lvl_pic_bullet_id)
        .and_then(|pic_id| ctx.resolved.pic_bullets.get(&pic_id))
        .and_then(|bullet| {
            let rel_id = bullet
                .pict
                .as_ref()?
                .shapes()
                .next()?
                .common
                .image_data
                .as_ref()?
                .rel_id
                .as_ref()?;
            let image_bytes = ctx.media().get(rel_id)?;
            // Size from VML shape style (width/height), default 9pt.
            let size = pic_bullet_size(bullet);
            let label_frag = Fragment::Image {
                size,
                rel_id: rel_id.as_str().to_string(),
                image_data: Some(image_bytes.clone()),
                src_rect: None,
            };
            Some((label_frag, size.height))
        });

    if let Some((label_frag, label_height)) = pic_bullet_injected {
        let hanging = extract_hanging(level_def);
        // §17.9.29: `Nothing` drops the separator entirely. `Tab` and `Space`
        // both advance via a tab here — a picture bullet has no text font to emit
        // a literal space with, and image-bullet + space is vanishingly rare.
        let drop_separator =
            level_def.map(|l| l.suffix) == Some(crate::model::LevelSuffix::Nothing);
        if !drop_separator {
            // §17.3.1.38: a leader on this separator is drawn in the
            // formatting in effect at the tab. A picture bullet has no text
            // run of its own, so the paragraph defaults *are* that formatting.
            let (fam, size, color, _, _) =
                resolve_paragraph_defaults(para, ctx.resolved, false, None, None);
            let sep_font = crate::render::layout::fragment::font_props_from_run(
                &model::RunProperties::default(),
                &fam,
                size,
                state.shape_auto_fit,
            );
            let tab_frag = Fragment::Tab {
                line_height: label_height,
                font: Rc::new(sep_font),
                color,
                fitting_width: Some(hanging),
            };
            fragments.insert(0, tab_frag);
        }
        fragments.insert(0, label_frag);

        if !drop_separator {
            if let Some(lvl_left) = level_def
                .and_then(|l| l.indentation.as_ref())
                .and_then(|ind| ind.start)
            {
                merged_props.tabs.insert(
                    0,
                    crate::model::TabStop {
                        position: lvl_left,
                        alignment: crate::model::TabAlignment::Left,
                        leader: crate::model::TabLeader::None,
                    },
                );
            }
        }
    } else {
        inject_text_label(
            para,
            fragments,
            merged_props,
            ctx,
            &state.list_counters,
            levels,
            level,
            level_def,
            state.shape_auto_fit,
        );
    }

    // §17.9.23: numbering level pPr overrides the paragraph style.
    // Only the paragraph's direct ind overrides the numbering level.
    if let Some(lvl_ind) = levels
        .get(level as usize)
        .and_then(|l| l.indentation.as_ref())
    {
        let mut ind = *lvl_ind;
        if let Some(direct) = para.properties.indentation {
            if let Some(start) = direct.start {
                ind.start = Some(start);
            }
            if let Some(end) = direct.end {
                ind.end = Some(end);
            }
            if let Some(first_line) = direct.first_line {
                ind.first_line = Some(first_line);
            }
        }
        merged_props.indentation = Some(ind);
    }
}

/// Inject a text label (non-picture bullet) into the paragraph fragments.
#[allow(clippy::too_many_arguments)]
fn inject_text_label(
    para: &model::Paragraph,
    fragments: &mut Vec<Fragment>,
    merged_props: &mut ParagraphProperties,
    ctx: &BuildContext,
    counters: &std::collections::HashMap<(model::NumId, u8), u32>,
    levels: &[crate::render::resolve::numbering::ResolvedNumberingLevel],
    level: u8,
    level_def: Option<&crate::render::resolve::numbering::ResolvedNumberingLevel>,
    auto_fit: crate::render::layout::ShapeAutoFit,
) {
    let num_id = model::NumId::new(merged_props.numbering.as_ref().unwrap().num_id);

    let (default_family, default_size, default_color, _, paragraph_style_run) =
        resolve_paragraph_defaults(para, ctx.resolved, false, None, None);

    // §17.9.23 / §17.3.1.29: assemble the label's character-property
    // cascade. Order matters — level rPr beats paragraph-mark rPr beats
    // paragraph-style run defaults. Document defaults arrive below via
    // `default_family`/`default_size` scalars consumed by
    // `font_props_from_run`.
    let cascade = ListLabelRunPropertyCascade {
        level: level_def.and_then(|l| l.run_properties.as_ref()),
        paragraph_mark: para.mark_run_properties.as_ref(),
        paragraph_style: Some(&paragraph_style_run),
    };

    // §17.3.2.20: the label's language, read from the label's *own* cascade
    // rather than the paragraph's — a `<w:lvl><w:rPr><w:lang>` is as much a
    // property of the level as its font is, and §17.9.27's three text formats
    // are the only thing that consults it. Document defaults close the cascade,
    // the layer `default_family`/`default_size` carry for the other fields.
    let locale = crate::render::resolve::locale::Locale::from_cascade(
        cascade
            .iter()
            .chain(std::iter::once(&ctx.resolved.doc_defaults_run)),
    );

    let label_text = match crate::render::resolve::numbering::format_list_label(
        levels, level, counters, num_id, locale,
    ) {
        Some(t) => t,
        None => return,
    };

    // §17.3.2.6: color follows the same cascade as the font fields,
    // resolved as a separate scalar because it isn't part of
    // `FontProps`.
    let label_color = cascade
        .pick(|rp| rp.color)
        .map(|c| {
            crate::render::resolve::color::resolve_color(
                c,
                crate::render::resolve::color::ColorContext::Text,
            )
        })
        .unwrap_or(default_color);

    // Legacy Symbol/Wingdings remapping needs the cascade-resolved
    // family before deciding whether to remap PUA codepoints back to
    // their bullet glyphs.
    let cascade_family = cascade
        .iter()
        .find_map(|rp| crate::render::resolve::fonts::effective_font(&rp.fonts))
        .unwrap_or("");
    let (label_text, label_family) =
        remap_legacy_font_chars(&label_text, cascade_family, &default_family);

    // Build the label font through the canonical path so every
    // label-relevant field (underline, char_spacing, text_scale, etc.)
    // flows from cascade.resolve() into `font_props_from_run`. After
    // remapping, override the family if it changed.
    let mut label_font = build_label_font_props(&cascade, &default_family, default_size, auto_fit);
    if label_family != *label_font.family {
        label_font.family = Rc::from(label_family.as_str());
    }
    // §17.3.2.40: populate underline metrics from font metrics now that
    // the bool is settled — `populate_underline_metrics` (used by
    // `build_fragments`) ran before label injection, so this fragment
    // must populate its own metrics.
    populate_label_underline_metrics(&mut label_font, ctx.measurer);

    let (w, m) = ctx.measurer.measure(&label_text, &label_font);
    let h = m.height();

    let hanging = extract_hanging(level_def);
    // §17.9.7: lvlJc controls label justification within the hanging indent area.
    let jc = level_def.and_then(|l| l.justification);
    let text_offset = match jc {
        Some(crate::model::Alignment::End) => -w,
        Some(crate::model::Alignment::Center) => w * -0.5,
        _ => Pt::ZERO,
    };
    let label_width = w;
    let label_frag = Fragment::Text {
        text: Rc::from(label_text.as_str()),
        font: Rc::new(label_font.clone()),
        color: label_color,
        shading: None,
        border: None,
        width: label_width,
        trimmed_width: label_width,
        metrics: m,
        hyperlink_url: None,
        baseline_offset: Pt::ZERO,
        text_offset,
        is_footnote_ref: false,
    };
    // §17.9.29: the separator between the label and the body text depends on the
    // level's `suff`. Tab (default) advances to the body-text indent via a tab
    // stop; Space emits a single space; Nothing puts the text flush against the
    // label.
    use crate::model::LevelSuffix;
    match level_def.map(|l| l.suffix).unwrap_or_default() {
        LevelSuffix::Tab => {
            let tab_fitting = (hanging - label_width).max(Pt::ZERO);
            fragments.insert(
                0,
                Fragment::Tab {
                    line_height: h,
                    // §17.3.1.38: the label's own formatting is what a leader
                    // on this separator is drawn in.
                    font: Rc::new(label_font.clone()),
                    color: label_color,
                    fitting_width: Some(tab_fitting),
                },
            );
            fragments.insert(0, label_frag);

            // Implicit tab stop at numLvl.left so the tab lands at body text.
            if let Some(lvl_left) = level_def
                .and_then(|l| l.indentation.as_ref())
                .and_then(|ind| ind.start)
            {
                merged_props.tabs.insert(
                    0,
                    crate::model::TabStop {
                        position: lvl_left,
                        alignment: crate::model::TabAlignment::Left,
                        leader: crate::model::TabLeader::None,
                    },
                );
            }
        }
        LevelSuffix::Space => {
            let (sw, sm) = ctx.measurer.measure(" ", &label_font);
            fragments.insert(
                0,
                Fragment::Text {
                    text: Rc::from(" "),
                    font: Rc::new(label_font.clone()),
                    color: label_color,
                    shading: None,
                    border: None,
                    width: sw,
                    trimmed_width: sw,
                    metrics: sm,
                    hyperlink_url: None,
                    baseline_offset: Pt::ZERO,
                    text_offset: Pt::ZERO,
                    is_footnote_ref: false,
                },
            );
            fragments.insert(0, label_frag);
        }
        LevelSuffix::Nothing => {
            fragments.insert(0, label_frag);
        }
    }
}

/// Extract the hanging indent from a numbering level definition.
fn extract_hanging(
    level_def: Option<&crate::render::resolve::numbering::ResolvedNumberingLevel>,
) -> Pt {
    level_def
        .and_then(|l| l.indentation.as_ref())
        .and_then(|ind| ind.first_line)
        .map(|fl| match fl {
            model::FirstLineIndent::Hanging(v) => Pt::from(v),
            _ => Pt::ZERO,
        })
        .unwrap_or(Pt::ZERO)
}

/// §17.9.23 — derive a label's [`FontProps`] from the resolved
/// character-property cascade. Delegates to [`font_props_from_run`]
/// so every label-relevant field (bold, italic, underline,
/// char_spacing, text_scale, font family, font size) takes the same
/// code path as ordinary text-run formatting. The returned
/// [`FontProps`] still has `underline_position`/`underline_thickness`
/// at zero — those come from font metrics and require a measurer.
/// Use [`populate_label_underline_metrics`] to fill them.
pub(super) fn build_label_font_props(
    cascade: &ListLabelRunPropertyCascade<'_>,
    default_family: &str,
    default_size: Pt,
    auto_fit: crate::render::layout::ShapeAutoFit,
) -> crate::render::layout::fragment::FontProps {
    let effective = cascade.resolve();
    crate::render::layout::fragment::font_props_from_run(
        &effective,
        default_family,
        default_size,
        auto_fit,
    )
}

/// Populate `underline_position` and `underline_thickness` on a
/// label's [`FontProps`] from the measurer's font metrics. No-op when
/// the label is not underlined. Idempotent.
pub(super) fn populate_label_underline_metrics(
    font: &mut crate::render::layout::fragment::FontProps,
    measurer: &crate::render::layout::measurer::TextMeasurer,
) {
    if font.underline {
        let (pos, thickness) = measurer.underline_metrics(font);
        font.underline_position = pos;
        font.underline_thickness = thickness;
    }
}

/// §17.9.23 / §17.3.1.29 — character-property cascade for a list
/// label's text. Sources are listed in priority order (highest first);
/// each field of the resolved properties is the topmost `Some` value
/// across `level → paragraph_mark → paragraph_style`.
///
/// The cascade is the spec-defined chain: a `<w:lvl><w:rPr>` overrides
/// the paragraph-mark `<w:pPr><w:rPr>`, which in turn overrides the
/// paragraph style's run defaults. Document defaults and theme
/// fallbacks come in below as scalar `default_family`/`default_size`
/// values supplied to `font_props_from_run`.
pub(super) struct ListLabelRunPropertyCascade<'a> {
    /// `<w:lvl><w:rPr>` — top of the cascade (§17.9.23).
    pub level: Option<&'a model::RunProperties>,
    /// `<w:pPr><w:rPr>` — formatting on the paragraph mark, applied
    /// where the level layer doesn't set a given field (§17.3.1.29).
    pub paragraph_mark: Option<&'a model::RunProperties>,
    /// Paragraph-style run defaults — bottom of the cascade.
    pub paragraph_style: Option<&'a model::RunProperties>,
}

impl<'a> ListLabelRunPropertyCascade<'a> {
    /// Iterate cascade sources in priority order, skipping `None`
    /// layers. Used by `pick` and by callers that need direct access
    /// to the cascade for non-`Copy` fields like `FontSet`.
    pub(super) fn iter(&self) -> impl Iterator<Item = &'a model::RunProperties> + '_ {
        [self.level, self.paragraph_mark, self.paragraph_style]
            .into_iter()
            .flatten()
    }

    /// Return the first `Some` value of a given field across the
    /// cascade, or `None` when no layer sets it. Restricted to `Copy`
    /// fields so the lookup is allocation-free; use `iter()` for fields
    /// that require cloning (e.g. `FontSet`).
    pub(super) fn pick<T: Copy>(
        &self,
        get: impl Fn(&model::RunProperties) -> Option<T>,
    ) -> Option<T> {
        self.iter().find_map(get)
    }

    /// Materialize a single effective `RunProperties` whose label-
    /// relevant fields (those consumed by `font_props_from_run` plus
    /// `color`) are picked from the highest-priority cascade source
    /// that sets each field. Fields no source sets are left at their
    /// default (`None` / empty `FontSet`) so downstream code can apply
    /// paragraph-level fallbacks.
    pub(super) fn resolve(&self) -> model::RunProperties {
        // `FontSet` is non-`Copy`. Pick from the first cascade source
        // whose font is explicitly set (any slot resolves under
        // `effective_font`); otherwise leave empty so downstream
        // `font_props_from_run` falls back to `default_family`.
        let fonts = self
            .iter()
            .find(|rp| crate::render::resolve::fonts::effective_font(&rp.fonts).is_some())
            .map(|rp| rp.fonts.clone())
            .unwrap_or_default();

        model::RunProperties {
            fonts,
            font_size: self.pick(|rp| rp.font_size),
            bold: self.pick(|rp| rp.bold),
            italic: self.pick(|rp| rp.italic),
            underline: self.pick(|rp| rp.underline),
            color: self.pick(|rp| rp.color),
            spacing: self.pick(|rp| rp.spacing),
            text_scale: self.pick(|rp| rp.text_scale),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::{Dimension, HalfPoints, Twips};
    use crate::model::{FontSet, FontSlot, RunProperties, TextScale, UnderlineStyle};

    // ── §17.9.22 label emission ──────────────────────────────────────────
    //
    // The 16 tests below this block all exercise the property cascade. These
    // exercise the path that *uses* it: counter arithmetic, the §17.9.29
    // separator switch, §17.9.7 justification, and the hanging indent.

    use crate::model::{
        Alignment, FirstLineIndent, Indentation, LevelSuffix, NumId, NumberFormat,
        NumberingReference,
    };
    use crate::render::fonts::FontRegistry;
    use crate::render::layout::measurer::TextMeasurer;
    use crate::render::resolve::ResolvedDocument;
    use crate::render::resolve::numbering::ResolvedNumberingLevel;
    use std::collections::HashMap;

    fn level(format: NumberFormat, level_text: &str) -> ResolvedNumberingLevel {
        ResolvedNumberingLevel {
            format,
            level_text: level_text.into(),
            start: 1,
            run_properties: None,
            indentation: None,
            justification: None,
            lvl_pic_bullet_id: None,
            suffix: LevelSuffix::Tab,
            is_legal: false,
        }
    }

    /// A decimal level whose label is just its own counter.
    fn decimal_level() -> ResolvedNumberingLevel {
        level(NumberFormat::Decimal, "%1")
    }

    fn resolved_with(levels: Vec<ResolvedNumberingLevel>) -> ResolvedDocument {
        let mut numbering = HashMap::new();
        numbering.insert(NumId::new(7), levels);
        ResolvedDocument {
            sections: Vec::new(),
            styles: HashMap::new(),
            numbering,
            font_families: Vec::new(),
            media: HashMap::new(),
            embedded_fonts: Vec::new(),
            pic_bullets: HashMap::new(),
            theme: None,
            doc_defaults_paragraph: ParagraphProperties::default(),
            doc_defaults_run: RunProperties::default(),
            default_paragraph_style_id: None,
            footnotes: HashMap::new(),
            endnotes: HashMap::new(),
            even_and_odd_headers: false,
            default_tab_stop: Dimension::new(720),
        }
    }

    fn numbered_para() -> model::Paragraph {
        model::Paragraph {
            style_id: None,
            properties: ParagraphProperties::default(),
            mark_run_properties: None,
            content: Vec::new(),
            rsids: model::ParagraphRevisionIds::default(),
        }
    }

    fn props_at(level: u8) -> ParagraphProperties {
        ParagraphProperties {
            numbering: Some(NumberingReference { num_id: 7, level }),
            ..Default::default()
        }
    }

    /// Run `inject_list_label` over a live measurer, returning the fragments it
    /// prepended and the properties it rewrote.
    fn inject(
        resolved: &ResolvedDocument,
        state: &mut BuildState,
        level: u8,
    ) -> (Vec<Fragment>, ParagraphProperties) {
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let ctx = BuildContext {
            measurer: &measurer,
            resolved,
        };
        let mut fragments = Vec::new();
        let mut props = props_at(level);
        inject_list_label(&numbered_para(), &mut fragments, &mut props, &ctx, state);
        (fragments, props)
    }

    fn label_text(fragments: &[Fragment]) -> String {
        match fragments.first() {
            Some(Fragment::Text { text, .. }) => text.to_string(),
            other => panic!("expected a label fragment, got {other:?}"),
        }
    }

    /// §17.9.22: the counter seeds at `start` on the first item and advances by
    /// one thereafter. Seeding `start - 1` and adding one is the shape that
    /// underflows on `w:start="0"`, so this pins the seed as well as the step.
    #[test]
    fn the_counter_seeds_at_start_then_increments() {
        let resolved = resolved_with(vec![decimal_level()]);
        let mut state = BuildState::default();
        let labels: Vec<String> = (0..3)
            .map(|_| label_text(&inject(&resolved, &mut state, 0).0))
            .collect();
        assert_eq!(labels, ["1", "2", "3"]);
    }

    /// §17.9.28 permits `w:start="0"`, which the seed must survive — it
    /// underflows `u32` if the counter is seeded one below `start`.
    #[test]
    fn a_zero_start_does_not_underflow() {
        let resolved = resolved_with(vec![ResolvedNumberingLevel {
            start: 0,
            ..decimal_level()
        }]);
        let mut state = BuildState::default();
        let labels: Vec<String> = (0..2)
            .map(|_| label_text(&inject(&resolved, &mut state, 0).0))
            .collect();
        assert_eq!(labels, ["0", "1"]);
    }

    /// §17.9.22: an item at one level restarts every level *below* it, so a
    /// nested list numbers `1 / 1 / 2 / 1` rather than carrying the sub-counter
    /// across. Levels *above* are untouched.
    #[test]
    fn a_deeper_level_restarts_when_its_parent_advances() {
        // `%N` names level N−1's counter, not "this level's" — so the level-1
        // template has to be `%2` to show its own number. With `%1` the test
        // reads level 0's counter and passes against a broken reset.
        let resolved = resolved_with(vec![decimal_level(), level(NumberFormat::Decimal, "%2")]);
        let mut state = BuildState::default();

        // Evaluated in order, each call advancing `state` — the sequence is
        // the assertion.
        let seen = vec![
            label_text(&inject(&resolved, &mut state, 0).0), // "1"
            label_text(&inject(&resolved, &mut state, 1).0), // "1" — first sub-item
            label_text(&inject(&resolved, &mut state, 1).0), // "2"
            label_text(&inject(&resolved, &mut state, 0).0), // "2" — resets level 1
            label_text(&inject(&resolved, &mut state, 1).0), // "1" again
        ];

        assert_eq!(seen, ["1", "1", "2", "2", "1"]);
    }

    /// §17.9.29: `Tab` (the default) puts a tab after the label, `Space` a
    /// literal space, `Nothing` neither.
    #[test]
    fn the_suffix_switch_picks_the_separator() {
        for (suffix, expect) in [
            (LevelSuffix::Tab, "tab"),
            (LevelSuffix::Space, "space"),
            (LevelSuffix::Nothing, "none"),
        ] {
            let resolved = resolved_with(vec![ResolvedNumberingLevel {
                suffix,
                ..decimal_level()
            }]);
            let (fragments, _) = inject(&resolved, &mut BuildState::default(), 0);
            assert_eq!(label_text(&fragments), "1", "{suffix:?}");

            let got = match fragments.get(1) {
                None => "none",
                Some(Fragment::Tab { .. }) => "tab",
                Some(Fragment::Text { text, .. }) if &**text == " " => "space",
                other => panic!("{suffix:?} emitted {other:?}"),
            };
            assert_eq!(got, expect, "{suffix:?}");
        }
    }

    /// §17.9.29: only the `Tab` suffix installs the implicit tab stop at the
    /// level's `start` indent — a `Space`/`Nothing` label has no tab to land.
    #[test]
    fn only_a_tab_suffix_installs_the_implicit_tab_stop() {
        for (suffix, expect_stop) in [
            (LevelSuffix::Tab, true),
            (LevelSuffix::Space, false),
            (LevelSuffix::Nothing, false),
        ] {
            let resolved = resolved_with(vec![ResolvedNumberingLevel {
                suffix,
                indentation: Some(Indentation {
                    start: Some(Dimension::new(720)),
                    ..Default::default()
                }),
                ..decimal_level()
            }]);
            let (_, props) = inject(&resolved, &mut BuildState::default(), 0);
            assert_eq!(
                props.tabs.iter().any(|t| t.position == Dimension::new(720)),
                expect_stop,
                "{suffix:?}"
            );
        }
    }

    /// §17.9.7 `lvlJc`: the label is placed by shifting it within the hanging
    /// indent area — `start` not at all, `end` by its whole width, `center` by
    /// half. Expressed as a ratio so the host font's metrics cancel.
    #[test]
    fn lvl_jc_offsets_the_label_by_its_own_width() {
        let offset_for = |jc: Option<Alignment>| {
            let resolved = resolved_with(vec![ResolvedNumberingLevel {
                justification: jc,
                ..decimal_level()
            }]);
            let (fragments, _) = inject(&resolved, &mut BuildState::default(), 0);
            match fragments.first() {
                Some(Fragment::Text {
                    text_offset, width, ..
                }) => (text_offset.raw(), width.raw()),
                other => panic!("expected a label, got {other:?}"),
            }
        };

        let (start, w) = offset_for(Some(Alignment::Start));
        assert_eq!(start, 0.0, "start-justified labels are not shifted");
        assert!(w > 0.0, "the label has a measured width");

        let (end, _) = offset_for(Some(Alignment::End));
        assert!((end + w).abs() < 1e-3, "end shifts left by the full width");

        let (centre, _) = offset_for(Some(Alignment::Center));
        assert!((centre + w * 0.5).abs() < 1e-3, "center shifts by half");

        let (absent, _) = offset_for(None);
        assert_eq!(absent, 0.0, "an absent lvlJc behaves as start");
    }

    /// §17.9.3: the tab after the label is asked to span the hanging indent
    /// *less* what the label already occupies, so the body text lands at the
    /// level's indent rather than one label-width past it.
    #[test]
    fn the_separator_tab_spans_the_hanging_indent_less_the_label() {
        let resolved = resolved_with(vec![ResolvedNumberingLevel {
            indentation: Some(Indentation {
                start: Some(Dimension::new(720)),
                first_line: Some(FirstLineIndent::Hanging(Dimension::new(360))), // 18pt
                ..Default::default()
            }),
            ..decimal_level()
        }]);
        let (fragments, _) = inject(&resolved, &mut BuildState::default(), 0);

        let label_width = match &fragments[0] {
            Fragment::Text { width, .. } => width.raw(),
            other => panic!("expected a label, got {other:?}"),
        };
        match &fragments[1] {
            Fragment::Tab { fitting_width, .. } => {
                let got = fitting_width
                    .expect("the label tab carries a fitting width")
                    .raw();
                assert!(
                    (got - (18.0 - label_width)).abs() < 1e-3,
                    "18pt hanging indent less a {label_width}pt label, got {got}"
                );
            }
            other => panic!("expected the separator tab, got {other:?}"),
        }
    }

    /// `extract_hanging` reads only the hanging case: a *first-line* indent is
    /// not a hanging indent, and neither is an absent one. Reached through the
    /// tab's fitting width, which is the only thing that consumes it.
    #[test]
    fn only_a_hanging_first_line_indent_is_a_hanging_indent() {
        let fitting_for = |first_line: Option<FirstLineIndent>| {
            let resolved = resolved_with(vec![ResolvedNumberingLevel {
                indentation: Some(Indentation {
                    first_line,
                    ..Default::default()
                }),
                ..decimal_level()
            }]);
            let (fragments, _) = inject(&resolved, &mut BuildState::default(), 0);
            match &fragments[1] {
                Fragment::Tab { fitting_width, .. } => fitting_width.unwrap().raw(),
                other => panic!("expected the separator tab, got {other:?}"),
            }
        };

        // A hanging indent gives the tab something to span; the other two
        // clamp to zero, because `hanging - label_width` goes negative.
        assert!(fitting_for(Some(FirstLineIndent::Hanging(Dimension::new(720)))) > 0.0);
        assert_eq!(
            fitting_for(Some(FirstLineIndent::FirstLine(Dimension::new(720)))),
            0.0
        );
        assert_eq!(fitting_for(Some(FirstLineIndent::None)), 0.0);
        assert_eq!(fitting_for(None), 0.0);
    }

    /// §17.9.22: a paragraph with no numbering reference is left completely
    /// alone — no label, no rewritten indentation, no counter touched.
    #[test]
    fn a_paragraph_without_numbering_is_untouched() {
        let resolved = resolved_with(vec![decimal_level()]);
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let ctx = BuildContext {
            measurer: &measurer,
            resolved: &resolved,
        };
        let mut state = BuildState::default();
        let mut fragments = Vec::new();
        let mut props = ParagraphProperties::default();
        inject_list_label(
            &numbered_para(),
            &mut fragments,
            &mut props,
            &ctx,
            &mut state,
        );

        assert!(fragments.is_empty(), "no label injected");
        assert!(props.indentation.is_none(), "indentation untouched");
        assert!(state.list_counters.is_empty(), "no counter advanced");
    }

    /// §17.9.2: a `numId` with no definition is a dangling reference. It must
    /// not inject a label *or* advance a counter — the paragraph simply is not
    /// numbered.
    #[test]
    fn an_unknown_num_id_injects_nothing() {
        let resolved = resolved_with(vec![decimal_level()]);
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let ctx = BuildContext {
            measurer: &measurer,
            resolved: &resolved,
        };
        let mut state = BuildState::default();
        let mut fragments = Vec::new();
        let mut props = ParagraphProperties {
            numbering: Some(NumberingReference {
                num_id: 999,
                level: 0,
            }),
            ..Default::default()
        };
        inject_list_label(
            &numbered_para(),
            &mut fragments,
            &mut props,
            &ctx,
            &mut state,
        );

        assert!(fragments.is_empty(), "no label injected");
        assert!(state.list_counters.is_empty(), "no counter advanced");
    }

    /// §17.9.23: the level's indentation replaces the paragraph's, but the
    /// paragraph's *direct* indentation still wins field by field.
    #[test]
    fn direct_paragraph_indentation_beats_the_level() {
        let resolved = resolved_with(vec![ResolvedNumberingLevel {
            indentation: Some(Indentation {
                start: Some(Dimension::new(720)),
                end: Some(Dimension::new(100)),
                ..Default::default()
            }),
            ..decimal_level()
        }]);
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let ctx = BuildContext {
            measurer: &measurer,
            resolved: &resolved,
        };
        let mut para = numbered_para();
        para.properties.indentation = Some(Indentation {
            start: Some(Dimension::new(1440)),
            ..Default::default()
        });
        let mut fragments = Vec::new();
        let mut props = props_at(0);
        inject_list_label(
            &para,
            &mut fragments,
            &mut props,
            &ctx,
            &mut BuildState::default(),
        );

        let ind = props.indentation.expect("the level supplies indentation");
        assert_eq!(ind.start, Some(Dimension::new(1440)), "direct start wins");
        assert_eq!(ind.end, Some(Dimension::new(100)), "level end survives");
    }

    fn rp_with_bold(b: bool) -> RunProperties {
        RunProperties {
            bold: Some(b),
            ..Default::default()
        }
    }

    fn rp_with_underline(u: UnderlineStyle) -> RunProperties {
        RunProperties {
            underline: Some(u),
            ..Default::default()
        }
    }

    fn rp_with_font(name: &str) -> RunProperties {
        RunProperties {
            fonts: FontSet {
                ascii: FontSlot::from_name(name),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Highest-priority source wins when more than one layer sets the
    /// same field.
    #[test]
    fn cascade_pick_level_overrides_mark_for_bold() {
        let level = rp_with_bold(true);
        let mark = rp_with_bold(false);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: Some(&mark),
            paragraph_style: None,
        };
        assert_eq!(cascade.pick(|rp| rp.bold), Some(true));
    }

    /// When the level layer doesn't set a field, the next layer down
    /// supplies it. §17.3.1.29.
    #[test]
    fn cascade_pick_falls_through_to_mark_when_level_field_absent() {
        let level = rp_with_bold(true); // sets bold but NOT underline
        let mark = rp_with_underline(UnderlineStyle::Single);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: Some(&mark),
            paragraph_style: None,
        };
        assert_eq!(cascade.pick(|rp| rp.bold), Some(true));
        assert_eq!(
            cascade.pick(|rp| rp.underline),
            Some(UnderlineStyle::Single)
        );
    }

    /// Paragraph-style run defaults are the bottom layer — used only
    /// when neither level nor mark sets a field.
    #[test]
    fn cascade_pick_falls_through_to_paragraph_style_when_others_absent() {
        let style = rp_with_underline(UnderlineStyle::Double);
        let cascade = ListLabelRunPropertyCascade {
            level: None,
            paragraph_mark: None,
            paragraph_style: Some(&style),
        };
        assert_eq!(
            cascade.pick(|rp| rp.underline),
            Some(UnderlineStyle::Double)
        );
    }

    /// No layer sets the field → `None`. Caller decides the default.
    #[test]
    fn cascade_pick_returns_none_when_no_source_sets_field() {
        let level = rp_with_bold(true);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        assert_eq!(cascade.pick(|rp| rp.italic), None);
    }

    /// `iter()` skips `None` layers and preserves priority order.
    #[test]
    fn cascade_iter_skips_none_layers_in_order() {
        let level = rp_with_bold(true);
        let style = rp_with_bold(false);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None, // skipped
            paragraph_style: Some(&style),
        };
        let bolds: Vec<_> = cascade.iter().map(|rp| rp.bold).collect();
        assert_eq!(bolds, vec![Some(true), Some(false)]);
    }

    /// The key correctness property for the bug at hand: a level rPr
    /// underline must surface in the resolved properties so
    /// `font_props_from_run` reads it.
    #[test]
    fn cascade_resolve_materializes_underline_from_level() {
        let level = rp_with_underline(UnderlineStyle::Single);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        let effective = cascade.resolve();
        assert_eq!(effective.underline, Some(UnderlineStyle::Single));
    }

    /// `FontSet` (non-`Copy`) cascade rule: the resolved `fonts` is
    /// the first cascade source that has an explicitly set font slot.
    #[test]
    fn cascade_resolve_fonts_picks_first_explicit_source() {
        let level = RunProperties::default(); // no fonts
        let mark = rp_with_font("Verdana");
        let style = rp_with_font("Arial");
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: Some(&mark),
            paragraph_style: Some(&style),
        };
        let effective = cascade.resolve();
        assert_eq!(effective.fonts.ascii.explicit.as_deref(), Some("Verdana"));
    }

    // ── §17.9.23 — `build_label_font_props` ──────────────────────────────
    //
    // The label's `FontProps` is built by passing the cascade-resolved
    // `RunProperties` through `font_props_from_run`. These tests pin
    // the spec-driven invariants the previous hand-rolled construction
    // violated (notably: dropped underline, char_spacing, text_scale).

    /// THE BUG: an underline in the level rPr (`<w:lvl><w:rPr><w:u/>`)
    /// must surface as `font.underline = true`. Previously this code
    /// path hardcoded `underline: false`.
    #[test]
    fn label_font_inherits_underline_from_level() {
        let level = rp_with_underline(UnderlineStyle::Single);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(
            font.underline,
            "level rPr <w:u/> must become font.underline"
        );
    }

    /// Cascade depth: when the level layer doesn't set underline but
    /// the paragraph-mark rPr does, the label inherits the underline.
    /// §17.3.1.29. (Previously broken: hand-rolled code never read
    /// either layer's underline.)
    #[test]
    fn label_font_inherits_underline_from_mark_when_level_absent() {
        let mark = rp_with_underline(UnderlineStyle::Single);
        let cascade = ListLabelRunPropertyCascade {
            level: None,
            paragraph_mark: Some(&mark),
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(font.underline);
    }

    /// Explicit `<w:u w:val="none"/>` in the level must turn underline
    /// OFF even if a lower cascade layer has it on — per spec, "none"
    /// is an explicit override, not an "inherit" signal. The cascade
    /// picks the level's `Some(None)` over the mark's `Some(Single)`,
    /// and `font_props_from_run` collapses it to `underline = false`.
    #[test]
    fn label_font_underline_false_when_level_explicitly_none() {
        let level = rp_with_underline(UnderlineStyle::None);
        let mark = rp_with_underline(UnderlineStyle::Single);
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: Some(&mark),
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(
            !font.underline,
            "explicit UnderlineStyle::None must override lower layers"
        );
    }

    /// `<w:spacing>` (char spacing) was silently dropped by the
    /// hand-rolled construction. With cascade routing it must flow
    /// into `FontProps::char_spacing`.
    #[test]
    fn label_font_inherits_char_spacing_from_level() {
        let level = RunProperties {
            spacing: Some(Dimension::<Twips>::new(40)),
            ..Default::default()
        };
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        // 40 twips = 2 pt.
        assert!((font.char_spacing.raw() - 2.0).abs() < 1e-4);
    }

    /// `<w:w>` (text scale) was silently fixed to 1.0 in the
    /// hand-rolled construction.
    #[test]
    fn label_font_inherits_text_scale_from_level() {
        let level = RunProperties {
            text_scale: Some(TextScale::new(150)),
            ..Default::default()
        };
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!((font.text_scale - 1.5).abs() < 1e-4);
    }

    /// Bold/italic/size pass through, with the cascade-resolved font
    /// family taking precedence over `default_family`.
    #[test]
    fn label_font_pass_through_basic_fields() {
        let level = RunProperties {
            bold: Some(true),
            italic: Some(true),
            font_size: Some(Dimension::<HalfPoints>::new(24)), // 12 pt
            fonts: FontSet {
                ascii: FontSlot::from_name("Verdana"),
                ..Default::default()
            },
            ..Default::default()
        };
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: None,
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(10.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(font.bold);
        assert!(font.italic);
        assert_eq!(font.size.raw(), 12.0);
        assert_eq!(&*font.family, "Verdana");
    }

    /// When no cascade layer sets a font, `default_family` wins.
    #[test]
    fn label_font_falls_back_to_default_family() {
        let cascade = ListLabelRunPropertyCascade {
            level: None,
            paragraph_mark: None,
            paragraph_style: None,
        };
        let font = build_label_font_props(
            &cascade,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert_eq!(&*font.family, "Helvetica");
    }

    /// Empty cascade — resolve returns default (all `None` / empty).
    #[test]
    fn cascade_resolve_returns_defaults_when_cascade_empty() {
        let cascade = ListLabelRunPropertyCascade {
            level: None,
            paragraph_mark: None,
            paragraph_style: None,
        };
        let effective = cascade.resolve();
        assert_eq!(effective, RunProperties::default());
    }

    /// All label-relevant fields composed across all three layers.
    /// Documents the full set of fields the resolver pulls (any
    /// future field becoming label-relevant should fail this test
    /// until added).
    #[test]
    fn cascade_resolve_composes_all_label_fields() {
        use crate::model::Color;

        let level = RunProperties {
            bold: Some(true),
            underline: Some(UnderlineStyle::Single),
            ..Default::default()
        };
        let mark = RunProperties {
            italic: Some(true),
            font_size: Some(Dimension::<HalfPoints>::new(24)),
            spacing: Some(Dimension::<Twips>::new(40)),
            ..Default::default()
        };
        let style = RunProperties {
            color: Some(Color::Rgb(0x112233)),
            text_scale: Some(TextScale::new(120)),
            fonts: FontSet {
                ascii: FontSlot::from_name("Calibri"),
                ..Default::default()
            },
            ..Default::default()
        };
        let cascade = ListLabelRunPropertyCascade {
            level: Some(&level),
            paragraph_mark: Some(&mark),
            paragraph_style: Some(&style),
        };
        let effective = cascade.resolve();
        assert_eq!(effective.bold, Some(true), "from level");
        assert_eq!(
            effective.underline,
            Some(UnderlineStyle::Single),
            "from level"
        );
        assert_eq!(effective.italic, Some(true), "from mark");
        assert_eq!(
            effective.font_size,
            Some(Dimension::<HalfPoints>::new(24)),
            "from mark"
        );
        assert_eq!(
            effective.spacing,
            Some(Dimension::<Twips>::new(40)),
            "from mark"
        );
        assert_eq!(effective.color, Some(Color::Rgb(0x112233)), "from style");
        assert_eq!(
            effective.text_scale,
            Some(TextScale::new(120)),
            "from style"
        );
        assert_eq!(
            effective.fonts.ascii.explicit.as_deref(),
            Some("Calibri"),
            "from style (lowest source that sets fonts)"
        );
    }
}
