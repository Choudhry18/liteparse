//! Style inheritance resolution — walk `basedOn` chains and merge properties.

use std::collections::{HashMap, HashSet};

use crate::model::{
    ParagraphProperties, RunProperties, StyleId, StyleSheet, TableProperties, Theme,
};

use super::properties::{merge_paragraph_properties, merge_run_properties, merge_table_properties};

/// A fully resolved style — all `basedOn` inheritance has been applied.
#[derive(Clone, Debug)]
pub struct ResolvedStyle {
    pub paragraph: ParagraphProperties,
    pub run: RunProperties,
    pub table: Option<TableProperties>,
    /// §17.7.6.6: table style conditional formatting overrides.
    pub table_style_overrides: Vec<crate::model::TableStyleOverride>,
    /// §17.7.4.9: this style is a table-of-contents *entry* style (`toc 1` …
    /// `toc 9`), either directly or through its `basedOn` chain.
    ///
    /// Derived here rather than tested at the render boundary because the
    /// signal is the **primary style name**, not the `w:styleId` spelling —
    /// see the private `is_toc_entry_name` helper below.
    pub is_toc_entry: bool,
}

/// §17.7.4.9: recognize the built-in primary style names for table-of-contents
/// **entries** — `toc 1` … `toc 9`.
///
/// `<w:name>` carries the *built-in* style name, which is locale-independent:
/// Word writes `toc 1` whatever the UI language, while the `w:styleId` is an
/// arbitrary identifier a producer may spell however it likes. Matching on the
/// name is therefore both narrower and wider than matching the ID — it does not
/// catch an unrelated user style called `TOCustom`, and it does catch a
/// localized document whose ToC style IDs are not spelled `TOC1`.
///
/// Deliberately excludes `TOC Heading` (the heading *above* a ToC, not an
/// entry) and `toc` with no level, neither of which is an entry style.
fn is_toc_entry_name(name: &str) -> bool {
    // OOXML style names compare case-insensitively.
    let rest = if name.len() >= 4 && name[..4].eq_ignore_ascii_case("toc ") {
        &name[4..]
    } else {
        return false;
    };
    matches!(rest.parse::<u8>(), Ok(1..=9))
}

/// Resolve all styles in the stylesheet by walking `basedOn` chains.
/// The theme is used to resolve `asciiTheme` / `hAnsiTheme` font references
/// on style-level run properties (§17.3.2.26).
pub fn resolve_styles(
    sheet: &StyleSheet,
    theme: Option<&Theme>,
) -> HashMap<StyleId, ResolvedStyle> {
    let mut resolved: HashMap<StyleId, ResolvedStyle> = HashMap::new();

    for id in sheet.styles.keys() {
        if !resolved.contains_key(id) {
            resolve_one(id, sheet, theme, &mut resolved, &mut HashSet::new());
        }
    }

    resolved
}

/// Recursively resolve a single style, memoizing results.
/// `visiting` tracks the current chain for cycle detection.
fn resolve_one(
    id: &StyleId,
    sheet: &StyleSheet,
    theme: Option<&Theme>,
    resolved: &mut HashMap<StyleId, ResolvedStyle>,
    visiting: &mut HashSet<StyleId>,
) {
    if resolved.contains_key(id) {
        return;
    }

    let style = match sheet.styles.get(id) {
        Some(s) => s,
        None => return,
    };

    // Cycle detection: if we're already visiting this style, stop recursion.
    if !visiting.insert(id.clone()) {
        // Break the cycle — resolve with just own properties + doc defaults.
        let mut para = style.paragraph_properties.clone().unwrap_or_default();
        let mut run = style.run_properties.clone().unwrap_or_default();
        let table = fold_whole_table(style.table_properties.clone(), &style.table_style_overrides);
        merge_paragraph_properties(&mut para, &sheet.doc_defaults_paragraph);
        merge_run_properties(&mut run, &sheet.doc_defaults_run);
        let is_toc_entry = style.name.as_deref().is_some_and(is_toc_entry_name);
        resolved.insert(
            id.clone(),
            ResolvedStyle {
                paragraph: para,
                run,
                table,
                table_style_overrides: style.table_style_overrides.clone(),
                is_toc_entry,
            },
        );
        return;
    }

    // Resolve parent first (if any).
    if let Some(ref parent_id) = style.based_on {
        if !resolved.contains_key(parent_id) {
            resolve_one(parent_id, sheet, theme, resolved, visiting);
        }
    }

    // Start with own properties.
    let mut para = style.paragraph_properties.clone().unwrap_or_default();
    let mut run = style.run_properties.clone().unwrap_or_default();
    // §17.3.2.26: resolve theme font references on the style's own run properties.
    if let Some(th) = theme {
        super::fonts::resolve_font_set_themes(&mut run.fonts, th);
    }

    // §17.7.2: table property inheritance — cell margins from parent table styles.
    let mut table = style.table_properties.clone();
    // Merge from resolved parent (if it exists and was successfully resolved).
    if let Some(ref parent_id) = style.based_on {
        if let Some(parent_resolved) = resolved.get(parent_id) {
            merge_paragraph_properties(&mut para, &parent_resolved.paragraph);
            merge_run_properties(&mut run, &parent_resolved.run);
            merge_table_properties(&mut table, &parent_resolved.table);
        }
    }

    // §17.7.2: doc defaults are NOT merged here — they are merged by the
    // caller at the correct cascade level. For table cell paragraphs, the
    // table style must be able to override doc defaults, which requires
    // doc defaults to be the lowest priority in the merge chain.
    // Character styles inherit only from their basedOn chain.
    // Run defaults from docDefaults still apply for font resolution.
    if style.style_type != crate::model::StyleType::Character {
        merge_run_properties(&mut run, &sheet.doc_defaults_run);
    }

    visiting.remove(id);

    // §17.7.4.9: ToC-entry-ness follows the `basedOn` chain, so a user style
    // derived from `toc 2` is still a ToC entry.
    let is_toc_entry = style.name.as_deref().is_some_and(is_toc_entry_name)
        || style
            .based_on
            .as_ref()
            .and_then(|parent| resolved.get(parent))
            .is_some_and(|parent| parent.is_toc_entry);

    resolved.insert(
        id.clone(),
        ResolvedStyle {
            paragraph: para,
            run,
            // §17.7.6: applied after the parent merge, so `wholeTable` overrides
            // both this style's own `tblPr` and anything inherited.
            table: fold_whole_table(table, &style.table_style_overrides),
            table_style_overrides: style.table_style_overrides.clone(),
            is_toc_entry,
        },
    );
}

/// §17.7.6: fold a table style's `wholeTable` conditional layer into its own
/// table properties.
///
/// `wholeTable` sits above the style's own `tblPr` and below every positional
/// override. Folding it here — rather than at the two places `build_table` reads
/// `ResolvedStyle::table` — means the table-level cascade needs no second
/// lookup, and a style that inherits from another sees the parent's already
/// folded (the parent resolves first).
///
/// The cell-level half (`tcPr`/`rPr`/`pPr`) is seeded separately, in
/// [`resolve_cell_conditional`](super::conditional::resolve_cell_conditional).
fn fold_whole_table(
    table: Option<crate::model::TableProperties>,
    overrides: &[crate::model::TableStyleOverride],
) -> Option<crate::model::TableProperties> {
    let whole = overrides
        .iter()
        .find(|o| o.override_type == crate::model::TableStyleOverrideType::WholeTable)
        .and_then(|o| o.table_properties.as_ref());
    match whole {
        Some(overlay) => Some(super::properties::overlay_table_properties(table, overlay)),
        None => table,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::{Dimension, HalfPoints};
    use crate::model::*;

    fn make_sheet(styles: Vec<(StyleId, Style)>) -> StyleSheet {
        StyleSheet {
            styles: styles.into_iter().collect(),
            ..Default::default()
        }
    }

    fn style(
        based_on: Option<&str>,
        para: Option<ParagraphProperties>,
        run: Option<RunProperties>,
    ) -> Style {
        Style {
            name: None,
            style_type: StyleType::Paragraph,
            based_on: based_on.map(StyleId::new),
            is_default: false,
            paragraph_properties: para,
            run_properties: run,
            table_properties: None,
            table_style_overrides: vec![],
        }
    }

    #[test]
    fn single_style_no_inheritance() {
        let sheet = make_sheet(vec![(
            StyleId::new("Normal"),
            style(
                None,
                Some(ParagraphProperties {
                    alignment: Some(Alignment::Start),
                    ..Default::default()
                }),
                Some(RunProperties {
                    bold: Some(false),
                    font_size: Some(Dimension::<HalfPoints>::new(24)),
                    ..Default::default()
                }),
            ),
        )]);

        let resolved = resolve_styles(&sheet, None);
        let normal = resolved.get(&StyleId::new("Normal")).unwrap();

        assert_eq!(normal.paragraph.alignment, Some(Alignment::Start));
        assert_eq!(normal.run.bold, Some(false));
        assert_eq!(normal.run.font_size, Some(Dimension::<HalfPoints>::new(24)));
    }

    #[test]
    fn child_inherits_from_parent() {
        let sheet = make_sheet(vec![
            (
                StyleId::new("Normal"),
                style(
                    None,
                    Some(ParagraphProperties {
                        alignment: Some(Alignment::Start),
                        ..Default::default()
                    }),
                    Some(RunProperties {
                        font_size: Some(Dimension::<HalfPoints>::new(24)),
                        bold: Some(false),
                        ..Default::default()
                    }),
                ),
            ),
            (
                StyleId::new("Heading1"),
                style(
                    Some("Normal"),
                    Some(ParagraphProperties {
                        alignment: Some(Alignment::Center),
                        ..Default::default()
                    }),
                    Some(RunProperties {
                        bold: Some(true),
                        ..Default::default()
                    }),
                ),
            ),
        ]);

        let resolved = resolve_styles(&sheet, None);
        let h1 = resolved.get(&StyleId::new("Heading1")).unwrap();

        assert_eq!(
            h1.paragraph.alignment,
            Some(Alignment::Center),
            "child overrides parent"
        );
        assert_eq!(h1.run.bold, Some(true), "child overrides parent");
        assert_eq!(
            h1.run.font_size,
            Some(Dimension::<HalfPoints>::new(24)),
            "inherited from Normal"
        );
    }

    #[test]
    fn three_level_chain() {
        let sheet = make_sheet(vec![
            (
                StyleId::new("Base"),
                style(
                    None,
                    None,
                    Some(RunProperties {
                        font_size: Some(Dimension::<HalfPoints>::new(20)),
                        bold: Some(false),
                        italic: Some(false),
                        ..Default::default()
                    }),
                ),
            ),
            (
                StyleId::new("Mid"),
                style(
                    Some("Base"),
                    None,
                    Some(RunProperties {
                        bold: Some(true),
                        ..Default::default()
                    }),
                ),
            ),
            (
                StyleId::new("Leaf"),
                style(
                    Some("Mid"),
                    None,
                    Some(RunProperties {
                        italic: Some(true),
                        ..Default::default()
                    }),
                ),
            ),
        ]);

        let resolved = resolve_styles(&sheet, None);
        let leaf = resolved.get(&StyleId::new("Leaf")).unwrap();

        assert_eq!(leaf.run.italic, Some(true), "own value");
        assert_eq!(leaf.run.bold, Some(true), "from Mid");
        assert_eq!(
            leaf.run.font_size,
            Some(Dimension::<HalfPoints>::new(20)),
            "from Base"
        );
    }

    #[test]
    fn cycle_does_not_panic() {
        let sheet = make_sheet(vec![
            (
                StyleId::new("A"),
                style(
                    Some("B"),
                    None,
                    Some(RunProperties {
                        bold: Some(true),
                        ..Default::default()
                    }),
                ),
            ),
            (
                StyleId::new("B"),
                style(
                    Some("A"),
                    None,
                    Some(RunProperties {
                        italic: Some(true),
                        ..Default::default()
                    }),
                ),
            ),
        ]);

        // Should not infinite loop or panic
        let resolved = resolve_styles(&sheet, None);
        assert!(resolved.contains_key(&StyleId::new("A")));
        assert!(resolved.contains_key(&StyleId::new("B")));
    }

    #[test]
    fn missing_based_on_target_is_harmless() {
        let sheet = make_sheet(vec![(
            StyleId::new("Orphan"),
            style(
                Some("DoesNotExist"),
                None,
                Some(RunProperties {
                    bold: Some(true),
                    ..Default::default()
                }),
            ),
        )]);

        let resolved = resolve_styles(&sheet, None);
        let orphan = resolved.get(&StyleId::new("Orphan")).unwrap();
        assert_eq!(orphan.run.bold, Some(true));
    }

    #[test]
    fn doc_defaults_are_applied_as_base() {
        // §17.7.2: doc defaults paragraph properties are NOT merged during
        // style resolution — they are merged by the caller at the correct
        // cascade level. Only run defaults are merged into resolved styles.
        let sheet = StyleSheet {
            doc_defaults_paragraph: ParagraphProperties {
                alignment: Some(Alignment::Both),
                ..Default::default()
            },
            doc_defaults_run: RunProperties {
                font_size: Some(Dimension::<HalfPoints>::new(22)),
                ..Default::default()
            },
            styles: [(StyleId::new("Normal"), style(None, None, None))]
                .into_iter()
                .collect(),
            latent_styles: None,
        };

        let resolved = resolve_styles(&sheet, None);
        let normal = resolved.get(&StyleId::new("Normal")).unwrap();

        // Paragraph doc defaults are deferred to the caller.
        assert_eq!(
            normal.paragraph.alignment, None,
            "paragraph doc defaults are not merged into resolved styles"
        );
        // Run doc defaults ARE merged during resolution.
        assert_eq!(
            normal.run.font_size,
            Some(Dimension::<HalfPoints>::new(22)),
            "should inherit from doc defaults"
        );
    }

    #[test]
    fn style_overrides_doc_defaults() {
        let sheet = StyleSheet {
            doc_defaults_run: RunProperties {
                font_size: Some(Dimension::<HalfPoints>::new(22)),
                bold: Some(false),
                ..Default::default()
            },
            styles: [(
                StyleId::new("Strong"),
                style(
                    None,
                    None,
                    Some(RunProperties {
                        bold: Some(true),
                        ..Default::default()
                    }),
                ),
            )]
            .into_iter()
            .collect(),
            ..Default::default()
        };

        let resolved = resolve_styles(&sheet, None);
        let strong = resolved.get(&StyleId::new("Strong")).unwrap();

        assert_eq!(strong.run.bold, Some(true), "style overrides doc default");
        assert_eq!(
            strong.run.font_size,
            Some(Dimension::<HalfPoints>::new(22)),
            "inherited from doc default"
        );
    }

    // ── §17.7.4.9 ToC entry detection ────────────────────────────────────

    /// A paragraph style carrying a primary style name.
    fn named_style(name: &str, based_on: Option<&str>) -> Style {
        Style {
            name: Some(name.to_string()),
            ..style(based_on, None, None)
        }
    }

    #[test]
    fn toc_entry_names_are_levels_one_through_nine() {
        for level in 1..=9 {
            assert!(
                is_toc_entry_name(&format!("toc {level}")),
                "toc {level} is an entry style"
            );
        }
        // OOXML style names compare case-insensitively.
        assert!(is_toc_entry_name("TOC 3"));
        assert!(is_toc_entry_name("Toc 3"));
    }

    #[test]
    fn non_entry_toc_names_are_rejected() {
        for name in [
            "TOC Heading", // the heading above a ToC, not an entry
            "toc",         // no level
            "toc 0",       // levels are 1..=9
            "toc 10",
            "toc 1x",
            "table of contents",
            "TOCustom", // the false positive the old styleId prefix test hit
            "",
        ] {
            assert!(
                !is_toc_entry_name(name),
                "{name:?} is not a ToC entry style"
            );
        }
    }

    /// The signal is the primary style **name**, not the `w:styleId` spelling:
    /// a producer may use any ID, and Word writes the locale-independent
    /// built-in name regardless of UI language.
    #[test]
    fn toc_entry_is_detected_from_name_not_style_id() {
        let sheet = make_sheet(vec![
            // Localized/arbitrary ID, built-in ToC name → is an entry.
            (StyleId::new("Verzeichnis1"), named_style("toc 1", None)),
            // TOC-prefixed ID, unrelated name → is NOT an entry.
            (StyleId::new("TOCustom"), named_style("My Custom", None)),
            // The ToC heading is not an entry.
            (StyleId::new("TOCHeading"), named_style("TOC Heading", None)),
        ]);
        let resolved = resolve_styles(&sheet, None);

        assert!(
            resolved[&StyleId::new("Verzeichnis1")].is_toc_entry,
            "a localized style ID with the built-in toc name is an entry"
        );
        assert!(
            !resolved[&StyleId::new("TOCustom")].is_toc_entry,
            "a TOC-prefixed ID with an unrelated name is not an entry"
        );
        assert!(
            !resolved[&StyleId::new("TOCHeading")].is_toc_entry,
            "the ToC heading is not an entry"
        );
    }

    #[test]
    fn toc_entry_is_inherited_through_based_on() {
        let sheet = make_sheet(vec![
            (StyleId::new("TOC2"), named_style("toc 2", None)),
            (StyleId::new("MyToc"), named_style("My ToC", Some("TOC2"))),
            (StyleId::new("Deeper"), named_style("Deeper", Some("MyToc"))),
            (StyleId::new("Unrelated"), named_style("Body", None)),
        ]);
        let resolved = resolve_styles(&sheet, None);

        assert!(resolved[&StyleId::new("MyToc")].is_toc_entry, "one hop");
        assert!(resolved[&StyleId::new("Deeper")].is_toc_entry, "two hops");
        assert!(!resolved[&StyleId::new("Unrelated")].is_toc_entry);
    }
}
