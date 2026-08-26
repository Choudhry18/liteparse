//! The placeholder cascade — inherited **text defaults** (§21.1.2.2.9).
//!
//! [`pptx::text`] lowers `a:pPr`/`a:rPr` but deliberately resolves nothing: a
//! run that declares no size is left `font_size: None`, because the value lives
//! several parts away. This module walks that chain.
//!
//! [`pptx::text`]: crate::pptx::text
//!
//! It is the sibling of [`pptx::cascade`], which does the same job for
//! geometry, and it reuses that module's [`MatchRule`] verbatim. It differs
//! from it in three ways that a shared implementation would have got wrong.
//!
//! [`pptx::cascade`]: crate::pptx::cascade
//!
//! ## 1. The merge is per-property, not whole-value
//!
//! Geometry inherits a whole `Transform2D` because a transform is never
//! partial. Text is the opposite: a layout `a:lstStyle` level routinely
//! supplies some properties but not others — bold and silence on size is
//! the common case — so every property resolves independently and the walk
//! continues past a rung that answered partially.
//!
//! That merge is [`merge_run_properties`], vendored for DOCX and field-complete
//! (its `merge_run_all_fields_covered` test is the guard). Text inheritance in
//! OOXML is the same "fill the `None`s from the parent" rule in both formats,
//! so this is reuse, not coincidence.
//!
//! ## 2. There are more rungs, and two of them are not shapes
//!
//! | # | rung | matched by |
//! |---|---|---|
//! | 1 | run `a:rPr` | — (direct) |
//! | 2 | paragraph `a:pPr/a:defRPr` | — (direct) |
//! | 3 | shape's own `a:lstStyle`[lvl] | — (direct) |
//! | 4 | **layout** placeholder `a:lstStyle`[lvl] | [`MatchRule::Idx`] |
//! | 5 | **master** placeholder `a:lstStyle`[lvl] | [`MatchRule::CollapsedKind`] |
//! | 6 | master `p:txStyles`/{title,body,other}[lvl] | placeholder *kind* |
//! | 7 | presentation `p:defaultTextStyle`[lvl] | — (deck-wide) |
//! | 8 | spec default | — |
//!
//! Rungs 6 and 7 are whole-part elements, not shapes, so no matcher reaches
//! them — kind routes to a `p:txStyles` child ([`TextStyleClass`]) and
//! `defaultTextStyle` is simply deck-wide.
//!
//! **Rung 5 is the one that would have been dropped.** It looks redundant
//! beside rung 6, and python-pptx's documented chain omits it. In practice
//! a master placeholder's own size routinely disagrees with what
//! `p:txStyles` says for the same kind — sometimes by a wide margin (e.g. a
//! `bodyStyle` reading 32pt against `p:txStyles`' 14pt) — so skipping this
//! rung would produce real, not just cosmetic, size errors.
//!
//! ## 3. Non-placeholder shapes are a large fraction of the need and no matcher can see them
//!
//! A great many size-less runs sit on shapes with **no `p:ph` at all**, so
//! rungs 4-6 do not exist for them. Their chain was traced end to end
//! rather than assumed, and closes on exactly three suppliers:
//!
//! - the shape's own `a:lstStyle` (rung 3)
//! - the presentation's `defaultTextStyle` (rung 7) — by far the largest
//!   supplier for this population
//! - nothing, landing on the spec default (rung 8) — [`SizeSource::SpecDefault`]
//!   is how that residue stays visible rather than a silent guess
//!
//! **The theme rung does not exist.** §20.1.6.7's `a:objectDefaults` is the
//! obvious place for a non-placeholder shape's text defaults, but decks that
//! declare one do not use it to supply a font size in practice. So it is
//! not implemented, and that is a deliberate, measured omission rather than
//! an oversight.
//!
//! ## The chain terminates
//!
//! For placeholders it terminates before the spec default ever applies:
//! every (kind, level) pair a slide actually uses is expected to be
//! covered by the master's `p:txStyles`, and every master is expected to
//! declare all three children with a size at level 1. As with geometry's
//! "no master placeholder lacks an `xfrm`", the top of the chain is real.

use std::collections::HashMap;

use crate::model::RunProperties;
use crate::model::dimension::{Dimension, HalfPoints};
use crate::render::resolve::properties::merge_run_properties;

use super::cascade::MatchRule;
use super::shapes::{Placeholder, PlaceholderKind, Shape};
use super::text::{Bullet, ListStyle, Spacing, TextParagraphProperties, TextStyles};

/// §21.1.2.2.9 — `a:defRPr/@sz` defaults to 1800, i.e. 18pt.
///
/// Only reachable when every rung above stays silent, which should be rare.
/// Emitting it is right, but a consumer that wants to know should read
/// [`SizeSource`] rather than compare against this constant.
pub const SPEC_DEFAULT_FONT_SIZE: Dimension<HalfPoints> = Dimension::new(36);

/// Which `p:txStyles` child a placeholder kind reads (rung 6).
///
/// Distinct from [`PlaceholderKind::collapsed_for_master`], which collapses
/// onto the *kinds a master's shape tree declares*. This collapses onto the
/// three whole-part styles, and the two maps genuinely differ: `dt`, `ftr` and
/// `sldNum` stay separate for geometry because a master declares a shape for
/// each, but all three read `p:otherStyle` here because that is all there is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyleClass {
    Title,
    Body,
    Other,
}

impl TextStyleClass {
    pub fn of(kind: PlaceholderKind) -> Self {
        use PlaceholderKind::*;
        match kind {
            Title | CtrTitle => Self::Title,
            // The chrome placeholders. `sldImg` and `hdr` are notes-slide
            // furniture and belong here too.
            Dt | Ftr | SldNum | SldImg | Hdr => Self::Other,
            Body | SubTitle | Obj | Chart | Tbl | ClipArt | Dgm | Media | Pic | Unknown => {
                Self::Body
            }
        }
    }

    fn pick(self, styles: &TextStyles) -> &ListStyle {
        match self {
            Self::Title => &styles.title,
            Self::Body => &styles.body,
            Self::Other => &styles.other,
        }
    }
}

/// The `a:lstStyle`s a layout or master supplies, indexed for one-probe lookup.
///
/// Mirrors [`PlaceholderGeometry`] in shape and for the same reason — layouts
/// and masters repeat across every slide in a deck, so the index is built once
/// and borrowed per slide.
///
/// It does **not** pre-flatten the two rungs the way `PlaceholderGeometry`
/// does. Flattening is only sound when a rung either answers or does not;
/// here a rung answers *some* properties, so folding the master into the
/// layout would lose which of the two supplied what — and rung 5's
/// disagreements with `p:txStyles` are exactly the values that would be
/// silently misattributed. The walk visits both in order instead.
///
/// [`PlaceholderGeometry`]: crate::pptx::cascade::PlaceholderGeometry
#[derive(Clone, Debug, Default)]
pub struct PlaceholderTextStyles {
    by_idx: HashMap<u32, ListStyle>,
    by_kind: HashMap<PlaceholderKind, ListStyle>,
}

impl PlaceholderTextStyles {
    /// Index a layout's or master's top-level placeholder shapes.
    ///
    /// Top-level only, matching the geometry cascade: no corpus placeholder is
    /// nested in a `p:grpSp`, and a nested one is safer ignored than resolved.
    pub fn from_part(shapes: &[Shape]) -> Self {
        let mut out = Self::default();
        for shape in shapes {
            let Some(ph) = &shape.placeholder else {
                continue;
            };
            let Some(style) = shape.text().map(|t| &t.list_style) else {
                continue;
            };
            if style.is_empty() {
                continue;
            }
            // First in document order wins, as in the geometry cascade: a
            // layout can declare the same `@idx` twice, and a stable rule
            // beats a `HashMap`-iteration coin flip.
            out.by_idx.entry(ph.idx).or_insert_with(|| style.clone());
            out.by_kind
                .entry(ph.kind.collapsed_for_master())
                .or_insert_with(|| style.clone());
        }
        out
    }

    fn lookup(&self, ph: &Placeholder, rule: MatchRule) -> Option<&ListStyle> {
        match rule {
            MatchRule::Idx => self.by_idx.get(&ph.idx),
            MatchRule::CollapsedKind => self.by_kind.get(&ph.kind.collapsed_for_master()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_idx.is_empty() && self.by_kind.is_empty()
    }
}

/// Everything a deck supplies that does not vary per slide: rungs 6 and 7.
///
/// `master` is per-master in principle; decks with several masters build one
/// of these per master. `default_text_style` is genuinely deck-wide.
#[derive(Clone, Debug, Default)]
pub struct DeckTextDefaults {
    /// Rung 6 — the master's `p:txStyles`.
    pub master_styles: TextStyles,
    /// Rung 7 — the presentation's `p:defaultTextStyle`.
    pub default_text_style: ListStyle,
}

/// The rungs in scope for one shape, borrowed rather than owned.
///
/// Every field is `Option` and an empty source is indistinguishable from a
/// missing one: a slide whose layout is unreachable still resolves against
/// whatever remains, which is the fail-open posture the rest of this vendor
/// takes.
#[derive(Clone, Copy, Debug)]
pub struct TextCascade<'a> {
    /// Rung 3 — this shape's own `a:lstStyle`.
    pub shape: Option<&'a ListStyle>,
    /// Rung 4 — the layout's placeholder styles.
    pub layout: Option<&'a PlaceholderTextStyles>,
    /// Rung 5 — the master's placeholder styles. The rung that looks redundant
    /// and is not; see the module docs.
    pub master: Option<&'a PlaceholderTextStyles>,
    /// Rungs 6 and 7.
    pub deck: Option<&'a DeckTextDefaults>,
}

/// Where the resolved font size came from, so a probe can prove the chain
/// closes rather than assert it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeSource {
    /// Rung 1 or 2 — the run or its paragraph said so directly.
    Direct,
    /// Rung 3.
    ShapeListStyle,
    /// Rung 4.
    LayoutPlaceholder,
    /// Rung 5.
    MasterPlaceholder,
    /// Rung 6.
    MasterTextStyles,
    /// Rung 7.
    DefaultTextStyle,
    /// Rung 8 — nothing in the document supplied one. Should be rare; a
    /// non-trivial count here means a rung is not being reached.
    SpecDefault,
}

/// One paragraph's fully-resolved properties.
///
/// Paragraph-level fields stay `Option` where the spec gives no default and
/// "unspecified" is a real answer a renderer must decide for itself. The run
/// defaults do not: [`run_defaults`](Self::run_defaults) always carries a
/// `font_size`, because every downstream stage — measurement, line breaking,
/// `TextItem` height — needs one and has no better guess to make than the spec
/// default this module already applied.
#[derive(Clone, Debug)]
pub struct ResolvedTextStyle {
    /// Zero-based `@lvl` the resolution was performed at.
    pub level: u8,
    pub alignment: Option<crate::model::Alignment>,
    pub margin_left: Option<Dimension<crate::model::dimension::Emu>>,
    pub margin_right: Option<Dimension<crate::model::dimension::Emu>>,
    pub indent: Option<Dimension<crate::model::dimension::Emu>>,
    pub default_tab_size: Option<Dimension<crate::model::dimension::Emu>>,
    pub rtl: Option<bool>,
    pub line_spacing: Option<Spacing>,
    pub space_before: Option<Spacing>,
    pub space_after: Option<Spacing>,
    /// `Some(Bullet::None)` is an explicit *un*-bullet that overrides an
    /// inherited one, and is why this cannot collapse to `Option<Bullet>`
    /// meaning "no bullet".
    pub bullet: Option<Bullet>,
    /// Rung 2's `a:defRPr` merged with everything below it. `font_size` is
    /// always present.
    pub run_defaults: RunProperties,
    pub size_source: SizeSource,
}

impl ResolvedTextStyle {
    /// Apply these defaults to one run's own `a:rPr` (rung 1 over rungs 2-8).
    ///
    /// Fills only the run's `None` fields, so direct formatting always wins and
    /// an explicit `b="0"` still overrides an inherited bold — the distinction
    /// [`opt_ooxml_bool`] exists to preserve.
    ///
    /// [`opt_ooxml_bool`]: crate::pptx::text
    pub fn apply_to_run(&self, run: &mut RunProperties) {
        merge_run_properties(run, &self.run_defaults);
    }
}

impl<'a> TextCascade<'a> {
    /// Resolve one paragraph's properties by walking rungs 2 through 8.
    ///
    /// `placeholder` is `None` for a shape with no `p:ph`, which skips rungs
    /// 4-6 entirely — 41% of the corpus need, and the reason those rungs are
    /// not simply always consulted.
    ///
    /// The level comes from `direct.level` (`a:pPr/@lvl`), defaulting to 0.
    /// Levels beyond `a:lvl9pPr` resolve to nothing at every rung and land on
    /// the spec default rather than panicking; `@lvl` is author-supplied.
    pub fn resolve(
        &self,
        direct: &TextParagraphProperties,
        placeholder: Option<&Placeholder>,
    ) -> ResolvedTextStyle {
        let level = direct.level.unwrap_or(0);

        // Rung 2 is the innermost and seeds the accumulator; each subsequent
        // rung only fills what is still `None`.
        let mut out = ResolvedTextStyle {
            level,
            alignment: direct.alignment,
            margin_left: direct.margin_left,
            margin_right: direct.margin_right,
            indent: direct.indent,
            default_tab_size: direct.default_tab_size,
            rtl: direct.rtl,
            line_spacing: direct.line_spacing,
            space_before: direct.space_before,
            space_after: direct.space_after,
            bullet: direct.bullet.clone(),
            run_defaults: direct.default_run_properties.clone().unwrap_or_default(),
            size_source: SizeSource::Direct,
        };
        let mut size_source = out.run_defaults.font_size.map(|_| SizeSource::Direct);

        // Rungs 3-7, innermost first. `absorb` records which rung first
        // supplied a size, which is what makes the probe's residue provable.
        let mut absorb = |props: Option<&TextParagraphProperties>, source: SizeSource| {
            let Some(props) = props else { return };
            out.absorb(props);
            if size_source.is_none() && out.run_defaults.font_size.is_some() {
                size_source = Some(source);
            }
        };

        absorb(
            self.shape.and_then(|s| s.level(level)),
            SizeSource::ShapeListStyle,
        );

        if let Some(ph) = placeholder {
            absorb(
                self.layout
                    .and_then(|l| l.lookup(ph, MatchRule::Idx))
                    .and_then(|s| s.level(level)),
                SizeSource::LayoutPlaceholder,
            );
            absorb(
                self.master
                    .and_then(|m| m.lookup(ph, MatchRule::CollapsedKind))
                    .and_then(|s| s.level(level)),
                SizeSource::MasterPlaceholder,
            );
            absorb(
                self.deck
                    .map(|d| TextStyleClass::of(ph.kind).pick(&d.master_styles))
                    .and_then(|s| s.level(level)),
                SizeSource::MasterTextStyles,
            );
        }

        absorb(
            self.deck.and_then(|d| d.default_text_style.level(level)),
            SizeSource::DefaultTextStyle,
        );

        // Rung 8. Applied last and recorded, never silently.
        out.size_source = size_source.unwrap_or(SizeSource::SpecDefault);
        if out.run_defaults.font_size.is_none() {
            out.run_defaults.font_size = Some(SPEC_DEFAULT_FONT_SIZE);
        }
        out
    }
}

impl ResolvedTextStyle {
    /// Fill this style's still-unset fields from one outer rung.
    fn absorb(&mut self, base: &TextParagraphProperties) {
        fill(&mut self.alignment, base.alignment);
        fill(&mut self.margin_left, base.margin_left);
        fill(&mut self.margin_right, base.margin_right);
        fill(&mut self.indent, base.indent);
        fill(&mut self.default_tab_size, base.default_tab_size);
        fill(&mut self.rtl, base.rtl);
        fill(&mut self.line_spacing, base.line_spacing);
        fill(&mut self.space_before, base.space_before);
        fill(&mut self.space_after, base.space_after);
        if self.bullet.is_none() {
            self.bullet = base.bullet.clone();
        }
        if let Some(run) = &base.default_run_properties {
            merge_run_properties(&mut self.run_defaults, run);
        }
    }
}

fn fill<T: Copy>(slot: &mut Option<T>, base: Option<T>) {
    if slot.is_none() {
        *slot = base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Alignment;
    use crate::pptx::text::{parse_default_text_style, parse_text_body, parse_text_styles};
    use crate::pptx::{parse_shape_tree, shapes::PlaceholderKind};

    fn pt(half_points: i64) -> Dimension<HalfPoints> {
        Dimension::new(half_points)
    }

    fn size_of(r: &ResolvedTextStyle) -> i64 {
        r.run_defaults.font_size.expect("always resolved").raw()
    }

    /// An `a:lstStyle` with one `lvl1pPr`, wrapped in a standalone body.
    fn shape_style(inner: &str) -> ListStyle {
        parse_text_body(
            format!(
                r#"<p:txBody xmlns:p="p" xmlns:a="a"><a:lstStyle><a:lvl1pPr>{inner}</a:lvl1pPr></a:lstStyle></p:txBody>"#
            )
            .as_bytes(),
        )
        .expect("parses")
        .list_style
    }

    fn part(shapes: &[(&str, &str, &str)]) -> Vec<Shape> {
        let body: String = shapes
            .iter()
            .map(|(ty, idx, inner)| {
                format!(
                    r#"<p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/>
                       <p:nvPr><p:ph type="{ty}" idx="{idx}"/></p:nvPr></p:nvSpPr>
                       <p:spPr/><p:txBody><a:bodyPr/>
                       <a:lstStyle><a:lvl1pPr>{inner}</a:lvl1pPr></a:lstStyle></p:txBody></p:sp>"#
                )
            })
            .collect();
        parse_shape_tree(
            format!(
                r#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>{body}</p:spTree></p:cSld></p:sld>"#
            )
            .as_bytes(),
        )
        .expect("parses")
    }

    fn ph(kind: PlaceholderKind, idx: u32) -> Placeholder {
        Placeholder { kind, idx }
    }

    fn empty() -> TextParagraphProperties {
        TextParagraphProperties::default()
    }

    /// Nothing anywhere: the spec default applies, and says so.
    #[test]
    fn an_empty_chain_lands_on_the_spec_default_and_records_it() {
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: None,
        };
        let r = cascade.resolve(&empty(), None);
        assert_eq!(r.run_defaults.font_size, Some(SPEC_DEFAULT_FONT_SIZE));
        assert_eq!(size_of(&r), 36, "18pt in half-points");
        assert_eq!(r.size_source, SizeSource::SpecDefault);
    }

    /// The common non-placeholder path: a shape reaches `defaultTextStyle`
    /// and nothing between.
    #[test]
    fn a_non_placeholder_shape_reaches_default_text_style() {
        let deck = DeckTextDefaults {
            master_styles: TextStyles::default(),
            default_text_style: parse_default_text_style(
                br#"<p:presentation xmlns:p="p" xmlns:a="a"><p:defaultTextStyle>
                      <a:lvl1pPr><a:defRPr sz="1400"/></a:lvl1pPr>
                    </p:defaultTextStyle></p:presentation>"#,
            )
            .unwrap(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let r = cascade.resolve(&empty(), None);
        assert_eq!(size_of(&r), 28, "14pt");
        assert_eq!(r.size_source, SizeSource::DefaultTextStyle);
    }

    /// Rungs 4-6 must not be consulted for a shape with no `p:ph`, or a
    /// non-placeholder textbox would silently take the title's 40pt.
    #[test]
    fn placeholder_rungs_are_invisible_to_a_non_placeholder_shape() {
        let layout_shapes = part(&[("title", "0", r#"<a:defRPr sz="4000"/>"#)]);
        let layout = PlaceholderTextStyles::from_part(&layout_shapes);
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
                      <p:bodyStyle><a:lvl1pPr><a:defRPr sz="9900"/></a:lvl1pPr></p:bodyStyle>
                    </p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: parse_default_text_style(
                br#"<p:presentation xmlns:p="p" xmlns:a="a"><p:defaultTextStyle>
                      <a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr>
                    </p:defaultTextStyle></p:presentation>"#,
            )
            .unwrap(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: Some(&layout),
            master: None,
            deck: Some(&deck),
        };
        let r = cascade.resolve(&empty(), None);
        assert_eq!(
            size_of(&r),
            36,
            "took defaultTextStyle, not the placeholders"
        );
        assert_eq!(r.size_source, SizeSource::DefaultTextStyle);
    }

    /// The layout rung matches on `@idx` and ignores `@type`, exactly as the
    /// geometry cascade does — the same bare-`<p:ph/>` case.
    #[test]
    fn the_layout_rung_matches_idx_not_type() {
        let layout_shapes = part(&[
            ("title", "0", r#"<a:defRPr sz="4400"/>"#),
            ("body", "1", r#"<a:defRPr sz="1800"/>"#),
        ]);
        let layout = PlaceholderTextStyles::from_part(&layout_shapes);
        let cascade = TextCascade {
            shape: None,
            layout: Some(&layout),
            master: None,
            deck: None,
        };
        // A bare `<p:ph/>` materializes as body/idx 0 but points at the title.
        let r = cascade.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 0)));
        assert_eq!(size_of(&r), 88, "22pt — the title's, reached by idx");
        assert_eq!(r.size_source, SizeSource::LayoutPlaceholder);
    }

    /// Rung 5, the one that looks redundant. A master placeholder's own
    /// size can disagree with `p:txStyles`; the master placeholder must win.
    #[test]
    fn the_master_placeholder_rung_beats_tx_styles() {
        let master_shapes = part(&[("body", "1", r#"<a:defRPr sz="3200"/>"#)]);
        let master = PlaceholderTextStyles::from_part(&master_shapes);
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
                      <p:bodyStyle><a:lvl1pPr><a:defRPr sz="1400"/></a:lvl1pPr></p:bodyStyle>
                    </p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: Some(&master),
            deck: Some(&deck),
        };
        // Note the idx differs: this rung matches on collapsed kind, so a
        // master `body` at idx 1 answers a slide `body` at idx 7.
        let r = cascade.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 7)));
        assert_eq!(size_of(&r), 64, "32pt from the master shape, not 14pt");
        assert_eq!(r.size_source, SizeSource::MasterPlaceholder);
    }

    /// The `subTitle`/`obj`/`tbl` family collapses onto the master's single
    /// `body` placeholder, as it does for geometry.
    #[test]
    fn body_ish_kinds_collapse_at_the_master_rung() {
        let master_shapes = part(&[("body", "1", r#"<a:defRPr sz="2000"/>"#)]);
        let master = PlaceholderTextStyles::from_part(&master_shapes);
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: Some(&master),
            deck: None,
        };
        for kind in [
            PlaceholderKind::SubTitle,
            PlaceholderKind::Obj,
            PlaceholderKind::Tbl,
            PlaceholderKind::Chart,
            PlaceholderKind::Pic,
        ] {
            let r = cascade.resolve(&empty(), Some(&ph(kind, 5)));
            assert_eq!(size_of(&r), 40, "{kind:?} should collapse onto body");
        }
    }

    /// Rung 6 routes by kind, and the three classes are distinct. `dt`/`ftr`/
    /// `sldNum` all read `otherStyle` even though geometry keeps them apart.
    #[test]
    fn tx_styles_routes_title_body_and_chrome_separately() {
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
                      <p:titleStyle><a:lvl1pPr><a:defRPr sz="4400"/></a:lvl1pPr></p:titleStyle>
                      <p:bodyStyle><a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr></p:bodyStyle>
                      <p:otherStyle><a:lvl1pPr><a:defRPr sz="1200"/></a:lvl1pPr></p:otherStyle>
                    </p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let cases = [
            (PlaceholderKind::Title, 88),
            (PlaceholderKind::CtrTitle, 88),
            (PlaceholderKind::Body, 56),
            (PlaceholderKind::SubTitle, 56),
            (PlaceholderKind::Dt, 24),
            (PlaceholderKind::Ftr, 24),
            (PlaceholderKind::SldNum, 24),
        ];
        for (kind, expected) in cases {
            let r = cascade.resolve(&empty(), Some(&ph(kind, 0)));
            assert_eq!(size_of(&r), expected, "{kind:?}");
            assert_eq!(r.size_source, SizeSource::MasterTextStyles);
        }
    }

    /// The finding that separates this cascade from the geometry one: a rung
    /// answers *some* properties and the walk continues past it for the
    /// rest. A layout level supplying properties but no size is common.
    #[test]
    fn a_partial_rung_does_not_stop_the_walk() {
        // Layout says bold and centred, but nothing about size.
        let layout_shapes = part(&[("body", "1", r#"<a:defRPr b="1"/>"#)]);
        let layout = PlaceholderTextStyles::from_part(&layout_shapes);
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
                      <p:bodyStyle><a:lvl1pPr algn="ctr"><a:defRPr sz="2800" i="1"/></a:lvl1pPr></p:bodyStyle>
                    </p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: Some(&layout),
            master: None,
            deck: Some(&deck),
        };
        let r = cascade.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 1)));
        assert_eq!(r.run_defaults.bold, Some(true), "from the layout");
        assert_eq!(
            size_of(&r),
            56,
            "size continued past the layout to txStyles"
        );
        assert_eq!(r.run_defaults.italic, Some(true), "also from txStyles");
        assert_eq!(r.alignment, Some(Alignment::Center));
        assert_eq!(r.size_source, SizeSource::MasterTextStyles);
    }

    /// An explicit `b="0"` on an inner rung must override an inherited bold —
    /// the distinction `opt_ooxml_bool` exists to keep, all the way through.
    #[test]
    fn an_explicit_off_overrides_an_inherited_on() {
        let shape = shape_style(r#"<a:defRPr b="0"/>"#);
        let deck = DeckTextDefaults {
            master_styles: TextStyles::default(),
            default_text_style: parse_default_text_style(
                br#"<p:presentation xmlns:p="p" xmlns:a="a"><p:defaultTextStyle>
                      <a:lvl1pPr><a:defRPr sz="1800" b="1"/></a:lvl1pPr>
                    </p:defaultTextStyle></p:presentation>"#,
            )
            .unwrap(),
        };
        let cascade = TextCascade {
            shape: Some(&shape),
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let r = cascade.resolve(&empty(), None);
        assert_eq!(r.run_defaults.bold, Some(false), "not re-inherited as true");
        assert_eq!(size_of(&r), 36, "size still came from the outer rung");
    }

    /// A run's own `a:rPr` wins over everything the cascade resolved, and only
    /// its `None`s are filled.
    #[test]
    fn apply_to_run_fills_only_what_the_run_left_unset() {
        let deck = DeckTextDefaults {
            master_styles: TextStyles::default(),
            default_text_style: parse_default_text_style(
                br#"<p:presentation xmlns:p="p" xmlns:a="a"><p:defaultTextStyle>
                      <a:lvl1pPr><a:defRPr sz="1800" b="1" i="1"/></a:lvl1pPr>
                    </p:defaultTextStyle></p:presentation>"#,
            )
            .unwrap(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let resolved = cascade.resolve(&empty(), None);

        let mut run = RunProperties {
            font_size: Some(pt(48)),
            bold: Some(false),
            ..Default::default()
        };
        resolved.apply_to_run(&mut run);
        assert_eq!(run.font_size, Some(pt(48)), "direct size untouched");
        assert_eq!(run.bold, Some(false), "explicit off untouched");
        assert_eq!(run.italic, Some(true), "unset field inherited");
    }

    /// Levels are per-level: `@lvl="1"` reads `a:lvl2pPr`, not `a:lvl1pPr`.
    #[test]
    fn each_level_resolves_independently() {
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles><p:bodyStyle>
                      <a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr>
                      <a:lvl2pPr><a:defRPr sz="2400"/></a:lvl2pPr>
                      <a:lvl3pPr><a:defRPr sz="2000"/></a:lvl3pPr>
                    </p:bodyStyle></p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        for (level, expected) in [(0u8, 56), (1, 48), (2, 40)] {
            let props = TextParagraphProperties {
                level: Some(level),
                ..Default::default()
            };
            let r = cascade.resolve(&props, Some(&ph(PlaceholderKind::Body, 1)));
            assert_eq!(size_of(&r), expected, "lvl {level}");
            assert_eq!(r.level, level);
        }
    }

    /// `@lvl` is author-supplied and DrawingML only addresses nine levels. An
    /// out-of-range one must degrade, not panic.
    #[test]
    fn an_out_of_range_level_falls_through_to_the_spec_default() {
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles>
                      <p:bodyStyle><a:lvl1pPr><a:defRPr sz="2800"/></a:lvl1pPr></p:bodyStyle>
                    </p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let props = TextParagraphProperties {
            level: Some(200),
            ..Default::default()
        };
        let r = cascade.resolve(&props, Some(&ph(PlaceholderKind::Body, 1)));
        assert_eq!(r.size_source, SizeSource::SpecDefault);
        assert_eq!(size_of(&r), 36);
    }

    /// `Some(Bullet::None)` is an explicit un-bullet and must not be replaced
    /// by an inherited bullet — the reason `bullet` is not `Option`-collapsed.
    #[test]
    fn an_explicit_bu_none_overrides_an_inherited_bullet() {
        let shape = shape_style("<a:buNone/>");
        let deck = DeckTextDefaults {
            master_styles: parse_text_styles(
                br#"<p:sldMaster xmlns:p="p" xmlns:a="a"><p:txStyles><p:bodyStyle>
                      <a:lvl1pPr><a:buChar char="&#8226;"/><a:defRPr sz="1800"/></a:lvl1pPr>
                    </p:bodyStyle></p:txStyles></p:sldMaster>"#,
            )
            .unwrap(),
            default_text_style: ListStyle::default(),
        };
        let cascade = TextCascade {
            shape: Some(&shape),
            layout: None,
            master: None,
            deck: Some(&deck),
        };
        let r = cascade.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 1)));
        assert_eq!(r.bullet, Some(Bullet::None), "explicit un-bullet held");
    }

    /// The empty-source case, as with geometry: absent and present-but-empty
    /// must behave identically and neither may panic.
    #[test]
    fn an_empty_source_behaves_as_an_absent_one() {
        let empty_index = PlaceholderTextStyles::from_part(&[]);
        assert!(empty_index.is_empty());
        let empty_deck = DeckTextDefaults::default();

        let with = TextCascade {
            shape: None,
            layout: Some(&empty_index),
            master: Some(&empty_index),
            deck: Some(&empty_deck),
        };
        let without = TextCascade {
            shape: None,
            layout: None,
            master: None,
            deck: None,
        };
        let a = with.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 1)));
        let b = without.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 1)));
        assert_eq!(a.size_source, b.size_source);
        assert_eq!(size_of(&a), size_of(&b));
    }

    /// A placeholder shape carrying no `a:lstStyle` must not shadow the rung
    /// above it with an empty entry.
    #[test]
    fn a_placeholder_without_a_list_style_is_not_indexed() {
        let shapes = parse_shape_tree(
            br#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                 <p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/>
                 <p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
                 <p:spPr/><p:txBody><a:bodyPr/><a:p/></p:txBody></p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        )
        .unwrap();
        assert!(PlaceholderTextStyles::from_part(&shapes).is_empty());
    }

    /// Duplicate `@idx`: first in document order wins, matching the geometry
    /// cascade's rule.
    #[test]
    fn duplicate_idx_resolves_first_in_document_order() {
        let shapes = part(&[
            ("body", "1", r#"<a:defRPr sz="1100"/>"#),
            ("body", "1", r#"<a:defRPr sz="2200"/>"#),
        ]);
        let layout = PlaceholderTextStyles::from_part(&shapes);
        let cascade = TextCascade {
            shape: None,
            layout: Some(&layout),
            master: None,
            deck: None,
        };
        let r = cascade.resolve(&empty(), Some(&ph(PlaceholderKind::Body, 1)));
        assert_eq!(size_of(&r), 22);
    }
}
