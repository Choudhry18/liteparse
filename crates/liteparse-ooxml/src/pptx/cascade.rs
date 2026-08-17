//! The placeholder cascade — inherited geometry (§19.3.1.36 `p:ph`).
//!
//! A slide shape that omits `<a:xfrm>` is not at the origin; it is a
//! **placeholder** taking its rectangle from the corresponding placeholder on
//! its layout, which may in turn take it from the master. [`pptx::shapes`]
//! deliberately leaves that as `transform: None`, and this module fills it in.
//!
//! [`pptx::shapes`]: crate::pptx::shapes
//!
//! ## The match rule is not the same at each rung
//!
//! This is the one thing to get right, and the corpus is unambiguous about it
//! (census over all 45 decks, 1,804 shapes declaring no `xfrm`):
//!
//! | rung | rule | result |
//! |---|---|---|
//! | slide → layout | **`@idx`** | **737 / 737** matched |
//! | layout → master | **collapsed `@type`** | 192 / 207 matched |
//! | notes slide → notes master | **collapsed `@type`** | 800 / 860 matched |
//!
//! `@idx` at the first rung is not merely sufficient, it is *right where type
//! would be wrong*: on 2 of the 737 the slide declares a bare `<p:ph idx="0"/>`
//! — which materializes as `body` per §19.7.10's default — while the layout
//! placeholder it points at is a `title`. The author's shape *is* the title;
//! matching on type would have missed it or, worse, matched some other body
//! placeholder. **Match on idx and do not second-guess it with type.**
//!
//! At the second and third rungs `@idx` is not usable — a master holds one
//! `body` placeholder for every body-ish layout placeholder, and their `idx`
//! values are unrelated — so the type is collapsed through
//! [`PlaceholderKind::collapsed_for_master`] first.
//!
//! ## The misses are properties of the documents, not of the rule
//!
//! Both were chased rather than assumed, because a match rate below 1.0 is
//! exactly how a wrong rule would look:
//!
//! - **15 layout misses** are all `p:hf`-family placeholders — `dt`, `ftr`,
//!   `sldNum` (§19.3.1.28) — on layouts whose master declares no counterpart
//!   at all. A master that turns those chrome elements off simply has no such
//!   shape, so there is nothing to inherit from and no rectangle we could
//!   invent that would not be a guess.
//! - **60 notes misses** are all from one deck whose `notesMaster1.xml` has a
//!   literally empty `spTree` — no shapes of any kind. Same conclusion.
//!
//! So the cascade is a *total* function over everything the file gives it
//! something to resolve against, and the residue is honest `None` rather than
//! a guessed rectangle. That is the same rule the rest of this vendor follows:
//! an invented value is silent corruption.
//!
//! ## Three simplifications, each measured rather than assumed
//!
//! - **Transforms are never partial.** Across every layout and master
//!   placeholder on the corpus, `<a:off>` and `<a:ext>` either both appear or
//!   neither does — 0 counterexamples. So inheritance is whole-transform, and
//!   the per-property walk (`left`/`top`/`width`/`height` each resolving
//!   independently, as python-pptx does it) buys nothing here.
//! - **No placeholder is ever nested inside a `p:grpSp`** — 0 across all 2,980
//!   corpus shape trees, on both sides of every rung. That matters more than it
//!   looks: a nested placeholder's transform lives in its group's child
//!   coordinate space, so inheriting one into or out of a group would silently
//!   mix coordinate spaces. This module therefore only ever reads and writes
//!   top-level shapes, and a nested placeholder is left untouched rather than
//!   resolved into the wrong space.
//! - **Rotation must ride along.** 195 inheritable placeholder transforms carry
//!   a non-`None` `@rot`, so dropping rotation on inheritance would silently
//!   un-rotate them. Whole-transform inheritance gets this for free; a
//!   hand-rolled offset/extent copy would not have.

use std::collections::HashMap;

use crate::model::Transform2D;

use super::shapes::{Placeholder, PlaceholderKind, Shape};

/// How a placeholder finds its counterpart on the part above it.
///
/// The two rungs genuinely differ; see the module docs for the measurement
/// that establishes each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchRule {
    /// Slide → layout, and notes slide → its own layout if one ever exists:
    /// match `@idx` exactly, ignoring `@type`.
    Idx,
    /// Layout → master, notes slide → notes master: match `@type` after
    /// collapsing through [`PlaceholderKind::collapsed_for_master`].
    CollapsedKind,
}

/// The inheritable geometry of one part, with that part's *own* inheritance
/// already folded in.
///
/// Build the master's first and pass it to the layout's, so that a slide
/// placeholder pointing at a layout placeholder that itself declares no
/// `xfrm` still resolves in one lookup. Both rungs are pre-flattened here
/// rather than walked per shape.
#[derive(Clone, Debug, Default)]
pub struct PlaceholderGeometry {
    by_idx: HashMap<u32, Transform2D>,
    by_kind: HashMap<PlaceholderKind, Transform2D>,
}

impl PlaceholderGeometry {
    /// Index a master or notes master — the top of the chain, with nothing
    /// above it to inherit from.
    ///
    /// On the corpus **no master placeholder lacks an `xfrm`**, so the chain
    /// always terminates in a real rectangle rather than running off the top.
    pub fn from_master(shapes: &[Shape]) -> Self {
        Self::index(shapes, None, MatchRule::CollapsedKind)
    }

    /// Index a layout, resolving each of its own no-`xfrm` placeholders against
    /// `master` first.
    ///
    /// `master` is `Option` because [`SlideParts::master`] is — a slide whose
    /// master is unreachable still has a usable layout, and the fail-open
    /// posture applies here as everywhere else in this vendor.
    ///
    /// [`SlideParts::master`]: crate::pptx::SlideParts::master
    pub fn from_layout(shapes: &[Shape], master: Option<&PlaceholderGeometry>) -> Self {
        Self::index(shapes, master, MatchRule::CollapsedKind)
    }

    /// The transform a placeholder with these properties should inherit.
    pub fn lookup(&self, ph: &Placeholder, rule: MatchRule) -> Option<Transform2D> {
        match rule {
            MatchRule::Idx => self.by_idx.get(&ph.idx).copied(),
            MatchRule::CollapsedKind => self.by_kind.get(&ph.kind.collapsed_for_master()).copied(),
        }
    }

    /// True when this part supplies no inheritable geometry at all — an empty
    /// `spTree`, or one with no placeholders. Real: one corpus notes master is
    /// exactly this.
    pub fn is_empty(&self) -> bool {
        self.by_idx.is_empty() && self.by_kind.is_empty()
    }

    fn index(shapes: &[Shape], parent: Option<&PlaceholderGeometry>, rule: MatchRule) -> Self {
        let mut out = Self::default();
        // Top-level only, deliberately: see the module docs on coordinate
        // spaces. Nested placeholders do not occur on the corpus, and if one
        // ever does, ignoring it is the safe failure.
        for shape in shapes {
            let Some(ph) = &shape.placeholder else {
                continue;
            };
            let Some(transform) = shape
                .transform
                .or_else(|| parent.and_then(|p| p.lookup(ph, rule)))
            else {
                continue;
            };
            // First in document order wins. 4 corpus layouts declare the same
            // `@idx` twice; PowerPoint's own resolution is first-wins, and more
            // to the point a stable rule beats a HashMap-order coin flip.
            out.by_idx.entry(ph.idx).or_insert(transform);
            out.by_kind
                .entry(ph.kind.collapsed_for_master())
                .or_insert(transform);
        }
        out
    }
}

/// What one [`apply_inherited_geometry`] pass did, for probes and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CascadeStats {
    /// Shapes that already declared their own `xfrm` and were left alone.
    pub declared: usize,
    /// Placeholders that had no `xfrm` and got one from the part above.
    pub inherited: usize,
    /// Placeholders that had no `xfrm` and found no counterpart. Stays `None`
    /// rather than becoming a guessed rectangle.
    pub unresolved: usize,
    /// Non-placeholder shapes with no `xfrm`. The shape walk measured this as
    /// **0 on the whole corpus** and `pptx_shape_probe` fails the build if it
    /// ever stops being 0, so a non-zero count here means that invariant broke
    /// and the cascade is not the thing to fix.
    pub orphans: usize,
}

impl CascadeStats {
    fn absorb(&mut self, other: CascadeStats) {
        self.declared += other.declared;
        self.inherited += other.inherited;
        self.unresolved += other.unresolved;
        self.orphans += other.orphans;
    }
}

/// Fill in `transform` for every top-level placeholder in `shapes` that
/// declares none, from `source` under `rule`.
///
/// Mutates in place and sets [`Shape::transform_inherited`] on everything it
/// touches, so a later consumer can still tell an inherited rectangle from a
/// declared one.
///
/// `source` is `Option` so the caller does not have to special-case a missing
/// layout, master or notes master; `None` resolves nothing and reports it,
/// which is the correct outcome for a deck whose notes master is empty.
pub fn apply_inherited_geometry(
    shapes: &mut [Shape],
    source: Option<&PlaceholderGeometry>,
    rule: MatchRule,
) -> CascadeStats {
    let mut stats = CascadeStats::default();
    for shape in shapes.iter_mut() {
        if shape.transform.is_some() {
            stats.declared += 1;
            continue;
        }
        let Some(ph) = shape.placeholder.clone() else {
            stats.orphans += 1;
            continue;
        };
        match source.and_then(|s| s.lookup(&ph, rule)) {
            Some(transform) => {
                shape.transform = Some(transform);
                shape.transform_inherited = true;
                stats.inherited += 1;
            }
            None => stats.unresolved += 1,
        }
    }
    stats
}

/// Resolve a slide's shapes against its layout and master in one call, and its
/// notes' shapes against the notes master.
///
/// The indexes are the caller's to cache: layouts and masters repeat across
/// every slide in a deck, so rebuilding them per slide is the one obvious way
/// to make this quadratic. Hence the primitives above are public and this is a
/// convenience over already-built indexes rather than over raw parts.
pub fn apply_slide(
    slide_shapes: &mut [Shape],
    layout: Option<&PlaceholderGeometry>,
    notes_shapes: &mut [Shape],
    notes_master: Option<&PlaceholderGeometry>,
) -> CascadeStats {
    let mut stats = apply_inherited_geometry(slide_shapes, layout, MatchRule::Idx);
    stats.absorb(apply_inherited_geometry(
        notes_shapes,
        notes_master,
        MatchRule::CollapsedKind,
    ));
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::{Dimension, Emu};
    use crate::model::geometry::{Offset, Size};
    use crate::pptx::parse_shape_tree;

    fn xfrm(x: i64, y: i64, cx: i64, cy: i64) -> String {
        format!(r#"<a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>"#)
    }

    /// A part holding one `p:sp` per `(type, idx, xfrm)` triple.
    fn part(shapes: &[(&str, &str, Option<String>)]) -> Vec<Shape> {
        let body: String = shapes
            .iter()
            .map(|(ty, idx, x)| {
                let ph = match (*ty, *idx) {
                    ("", "") => "<p:ph/>".to_string(),
                    ("", i) => format!(r#"<p:ph idx="{i}"/>"#),
                    (t, "") => format!(r#"<p:ph type="{t}"/>"#),
                    (t, i) => format!(r#"<p:ph type="{t}" idx="{i}"/>"#),
                };
                format!(
                    r#"<p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/><p:nvPr>{ph}</p:nvPr></p:nvSpPr>
                       <p:spPr>{}</p:spPr></p:sp>"#,
                    x.clone().unwrap_or_default()
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

    fn offset_of(s: &Shape) -> Option<Offset<Emu>> {
        s.transform.and_then(|t| t.offset)
    }

    fn emu(v: i64) -> Dimension<Emu> {
        Dimension::new(v)
    }

    #[test]
    fn slide_inherits_from_layout_by_idx() {
        let layout = part(&[
            ("title", "0", Some(xfrm(100, 200, 10, 20))),
            ("body", "1", Some(xfrm(300, 400, 30, 40))),
        ]);
        let index = PlaceholderGeometry::from_layout(&layout, None);

        let mut slide = part(&[("body", "1", None)]);
        let stats = apply_inherited_geometry(&mut slide, Some(&index), MatchRule::Idx);

        assert_eq!(stats.inherited, 1);
        assert_eq!(stats.unresolved, 0);
        assert_eq!(offset_of(&slide[0]).unwrap().x, emu(300));
        assert!(slide[0].transform_inherited);
    }

    /// The 2-of-737 case: a bare `<p:ph/>` materializes as `body` idx 0 but
    /// points at the layout's *title*. Matching on type instead of idx would
    /// get this wrong, which is why the rule ignores type entirely.
    #[test]
    fn idx_match_ignores_a_disagreeing_type() {
        let layout = part(&[
            ("title", "0", Some(xfrm(100, 200, 10, 20))),
            ("body", "1", Some(xfrm(300, 400, 30, 40))),
        ]);
        let index = PlaceholderGeometry::from_layout(&layout, None);

        let mut slide = part(&[("", "", None)]);
        assert_eq!(
            slide[0].placeholder.as_ref().unwrap().kind,
            PlaceholderKind::Body
        );
        assert_eq!(slide[0].placeholder.as_ref().unwrap().idx, 0);

        apply_inherited_geometry(&mut slide, Some(&index), MatchRule::Idx);
        assert_eq!(
            offset_of(&slide[0]).unwrap().x,
            emu(100),
            "took the title's box"
        );
    }

    /// Two rungs in one lookup: the layout placeholder itself has no `xfrm`.
    #[test]
    fn layout_folds_the_master_in_before_the_slide_asks() {
        let master = part(&[("body", "9", Some(xfrm(700, 800, 70, 80)))]);
        let master_index = PlaceholderGeometry::from_master(&master);

        // Layout body placeholder: no xfrm of its own, and idx 1 — unrelated
        // to the master's idx 9, which is why this rung matches on type.
        let layout = part(&[("body", "1", None)]);
        let layout_index = PlaceholderGeometry::from_layout(&layout, Some(&master_index));

        let mut slide = part(&[("body", "1", None)]);
        let stats = apply_inherited_geometry(&mut slide, Some(&layout_index), MatchRule::Idx);

        assert_eq!(stats.inherited, 1);
        assert_eq!(offset_of(&slide[0]).unwrap().y, emu(800));
    }

    /// `subTitle`, `obj`, `tbl` and friends all collapse onto the master's
    /// single `body`; without the collapse this rung finds nothing.
    #[test]
    fn layout_to_master_collapses_body_ish_types() {
        let master = part(&[("body", "1", Some(xfrm(700, 800, 70, 80)))]);
        let master_index = PlaceholderGeometry::from_master(&master);

        for ty in ["subTitle", "obj", "tbl", "chart", "pic", "dgm"] {
            let mut layout = part(&[(ty, "5", None)]);
            let stats = apply_inherited_geometry(
                &mut layout,
                Some(&master_index),
                MatchRule::CollapsedKind,
            );
            assert_eq!(stats.inherited, 1, "{ty} should collapse onto body");
            assert_eq!(offset_of(&layout[0]).unwrap().x, emu(700));
        }
    }

    /// The 15 `dt`/`ftr` layout placeholders: nothing to match, so nothing is
    /// invented. A guessed rectangle would be silent corruption.
    #[test]
    fn an_unmatched_placeholder_stays_none() {
        let master = part(&[("title", "0", Some(xfrm(100, 200, 10, 20)))]);
        let master_index = PlaceholderGeometry::from_master(&master);

        let mut layout = part(&[("ftr", "11", None)]);
        let stats =
            apply_inherited_geometry(&mut layout, Some(&master_index), MatchRule::CollapsedKind);

        assert_eq!(stats.unresolved, 1);
        assert_eq!(stats.inherited, 0);
        assert!(layout[0].transform.is_none());
        assert!(!layout[0].transform_inherited);
    }

    /// The empty-notesMaster deck. `None` and "present but empty" must behave
    /// the same, and neither may panic.
    #[test]
    fn an_empty_or_absent_source_resolves_nothing() {
        let empty = PlaceholderGeometry::from_master(&[]);
        assert!(empty.is_empty());

        for source in [Some(&empty), None] {
            let mut notes = part(&[("sldImg", "0", None), ("body", "1", None)]);
            let stats = apply_inherited_geometry(&mut notes, source, MatchRule::CollapsedKind);
            assert_eq!(stats.unresolved, 2);
            assert!(notes.iter().all(|s| s.transform.is_none()));
        }
    }

    /// A declared transform always wins; the cascade only fills holes.
    #[test]
    fn a_declared_transform_is_never_overwritten() {
        let layout = part(&[("body", "1", Some(xfrm(100, 200, 10, 20)))]);
        let index = PlaceholderGeometry::from_layout(&layout, None);

        let mut slide = part(&[("body", "1", Some(xfrm(999, 999, 99, 99)))]);
        let stats = apply_inherited_geometry(&mut slide, Some(&index), MatchRule::Idx);

        assert_eq!(stats.declared, 1);
        assert_eq!(stats.inherited, 0);
        assert_eq!(offset_of(&slide[0]).unwrap().x, emu(999));
        assert!(!slide[0].transform_inherited);
    }

    /// 4 corpus layouts declare `@idx` twice. First in document order wins —
    /// a stable rule, not a HashMap-iteration coin flip.
    #[test]
    fn duplicate_idx_resolves_first_in_document_order() {
        let layout = part(&[
            ("body", "1", Some(xfrm(111, 111, 11, 11))),
            ("body", "1", Some(xfrm(222, 222, 22, 22))),
        ]);
        let index = PlaceholderGeometry::from_layout(&layout, None);

        let mut slide = part(&[("body", "1", None)]);
        apply_inherited_geometry(&mut slide, Some(&index), MatchRule::Idx);
        assert_eq!(offset_of(&slide[0]).unwrap().x, emu(111));
    }

    /// Rotation is part of the inherited rectangle — 195 corpus placeholders
    /// carry one, and dropping it silently un-rotates them.
    #[test]
    fn rotation_rides_along_with_the_inherited_transform() {
        let shapes = parse_shape_tree(
            br#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                 <p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr>
                 <p:spPr><a:xfrm rot="-4320000"><a:off x="1" y="2"/><a:ext cx="3" cy="4"/></a:xfrm></p:spPr></p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        )
        .unwrap();
        let index = PlaceholderGeometry::from_layout(&shapes, None);

        let mut slide = part(&[("body", "1", None)]);
        apply_inherited_geometry(&mut slide, Some(&index), MatchRule::Idx);
        let t = slide[0].transform.unwrap();
        assert_eq!(t.rotation.map(|r| r.raw()), Some(-4320000));
        assert_eq!(t.extent.map(|e: Size<Emu>| e.width), Some(emu(3)));
    }

    /// A non-placeholder with no geometry is counted, not silently resolved:
    /// the shape walk asserts there are none on the corpus, so a non-zero
    /// count here points at that invariant, not at this module.
    #[test]
    fn a_non_placeholder_without_geometry_is_an_orphan() {
        let mut shapes = parse_shape_tree(
            br#"<p:sld xmlns:p="p" xmlns:a="a"><p:cSld><p:spTree>
                 <p:sp><p:nvSpPr><p:cNvPr id="1" name="s"/></p:nvSpPr><p:spPr/></p:sp>
               </p:spTree></p:cSld></p:sld>"#,
        )
        .unwrap();
        let stats = apply_inherited_geometry(&mut shapes, None, MatchRule::Idx);
        assert_eq!(stats.orphans, 1);
        assert_eq!(stats.unresolved, 0);
    }
}
