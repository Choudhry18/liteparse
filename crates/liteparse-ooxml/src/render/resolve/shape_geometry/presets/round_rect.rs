//! §20.1.9.18 `roundRect` preset — a rectangle whose four corners are
//! quarter-circle arcs of one shared radius.
//!
//! The radius is the shape's single adjustment, and it is *not* a length: the
//! spec's `avLst` carries `adj` as a fraction of the **shortest side**
//!
//! ```text
//! <gd name="a"  fmla="pin 0 adj 50000"/>
//! <gd name="x1" fmla="*/ ss a 100000"/>
//! ```
//!
//! so a wide banner and a tall panel with the same `adj` get the same corner,
//! and clamping at 50000 is what stops a large `adj` from turning the two arcs
//! of a side inside out. `adj` defaults to 16667 (≈1/6 of the short side) when
//! the file declares no `avLst`, which is the common case: PowerPoint only
//! writes one when the user has dragged the corner handle.

use crate::model::{GeomGuide, PathFillMode};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::guides::{GuideContext, evaluate_guides};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath, arc, turn};

/// The spec's default corner adjustment, used when the shape declares no
/// `<a:avLst>` — 16667 hundred-thousandths of the shortest side.
const DEFAULT_ADJ: f64 = 16_667.0;

/// §20.1.9.18's `pin 0 adj 50000`: half the shortest side is the largest
/// corner that still leaves a straight edge between two arcs.
const MAX_ADJ: f64 = 50_000.0;

pub fn build(adjust: &[GeomGuide], extent: PtSize) -> ShapePath {
    let (w, h) = (extent.width, extent.height);
    let r = corner_radius(adjust, extent);

    // Each corner starts where the previous edge ended and sweeps a quarter
    // turn clockwise, so the four `stAng`s are the spec's cd2 / 3cd4 / 0 / cd4
    // in that order. Angles are OOXML's own units, unconverted — the painter
    // consumes them directly.
    let verbs = vec![
        PathVerb::MoveTo(PtOffset::new(Pt::ZERO, r)),
        arc(r, r, turn::HALF, turn::QUARTER),
        PathVerb::LineTo(PtOffset::new(w - r, Pt::ZERO)),
        arc(r, r, turn::THREE_QUARTER, turn::QUARTER),
        PathVerb::LineTo(PtOffset::new(w, h - r)),
        arc(r, r, turn::NONE, turn::QUARTER),
        PathVerb::LineTo(PtOffset::new(r, h)),
        arc(r, r, turn::QUARTER, turn::QUARTER),
        PathVerb::Close,
    ];

    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: PathFillMode::Norm,
            stroked: true,
        }],
        // §20.1.9.18's `<rect l="x1" t="y1" r="x2" b="y2"/>`: text clears the
        // corners rather than the bounding box.
        text_rect: Some(PtRect::from_xywh(r, r, w - r - r, h - r - r)),
    }
}

/// `x1` — the corner radius in Pt.
///
/// Read through the guide evaluator rather than by pattern-matching the
/// formula string, so `<a:gd name="adj" fmla="val 25000"/>` and any equivalent
/// expression resolve the same way.
fn corner_radius(adjust: &[GeomGuide], extent: PtSize) -> Pt {
    let ctx = GuideContext::new(extent.width.raw() as f64, extent.height.raw() as f64);
    let adj = evaluate_guides(adjust, ctx)
        .get("adj")
        .copied()
        .unwrap_or(DEFAULT_ADJ)
        .clamp(0.0, MAX_ADJ);
    let shortest = extent.width.raw().min(extent.height.raw()).max(0.0);
    Pt::new(shortest * (adj as f32) / 100_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adj(value: i64) -> Vec<GeomGuide> {
        vec![GeomGuide {
            name: "adj".to_string(),
            formula: format!("val {value}"),
        }]
    }

    fn radius(path: &ShapePath) -> f32 {
        let PathVerb::MoveTo(p) = path.paths[0].verbs[0] else {
            panic!("a roundRect starts with a moveTo")
        };
        p.y.raw()
    }

    #[test]
    fn corners_are_a_fraction_of_the_shortest_side() {
        // A wide shape and a tall one with the same `adj` get the same corner:
        // `ss`, not `w` or `h`, is what the spec measures against — taking the
        // width would round a 400x40 banner into a lozenge.
        let wide = build(&adj(25_000), PtSize::new(Pt::new(400.0), Pt::new(40.0)));
        let tall = build(&adj(25_000), PtSize::new(Pt::new(40.0), Pt::new(400.0)));
        assert_eq!(radius(&wide), 10.0);
        assert_eq!(radius(&tall), 10.0);
    }

    #[test]
    fn no_adjust_list_takes_the_spec_default() {
        // The common case — PowerPoint writes an `avLst` only once the corner
        // handle has been dragged.
        let p = build(&[], PtSize::new(Pt::new(120.0), Pt::new(60.0)));
        assert!((radius(&p) - 60.0 * 0.16667).abs() < 0.01);
    }

    #[test]
    fn an_oversized_adjustment_is_pinned_at_half_the_short_side() {
        // `pin 0 adj 50000`. Past half, two corners of the same side would
        // overlap and the outline would fold through itself.
        let p = build(&adj(90_000), PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        assert_eq!(radius(&p), 50.0);
        let flat = build(&adj(-5_000), PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        assert_eq!(radius(&flat), 0.0);
    }

    #[test]
    fn the_outline_is_four_arcs_joined_by_four_edges() {
        let p = build(&adj(10_000), PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        let verbs = &p.paths[0].verbs;
        assert_eq!(verbs.len(), 9);
        let arcs = verbs
            .iter()
            .filter(|v| matches!(v, PathVerb::ArcTo { .. }))
            .count();
        assert_eq!(arcs, 4, "one quarter turn per corner");
        assert!(matches!(verbs[8], PathVerb::Close));
        // Every corner turns the same way, a quarter at a time — a sign slip on
        // one of them cuts the corner off instead of rounding it.
        for verb in verbs {
            if let PathVerb::ArcTo { swing_angle, .. } = verb {
                assert_eq!(swing_angle.raw(), 5_400_000);
            }
        }
    }

    #[test]
    fn the_text_rectangle_clears_the_corners() {
        let p = build(&adj(20_000), PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        let tr = p.text_rect.unwrap();
        assert_eq!(tr.origin, PtOffset::new(Pt::new(20.0), Pt::new(20.0)));
        assert_eq!(tr.size, PtSize::new(Pt::new(160.0), Pt::new(60.0)));
    }
}
