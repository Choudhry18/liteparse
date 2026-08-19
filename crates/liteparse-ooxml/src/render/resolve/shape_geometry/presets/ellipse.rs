//! §20.1.9.18 `ellipse` preset — the ellipse inscribed in the shape's
//! bounding box, drawn as two half-turn arcs.
//!
//! Two arcs rather than one full sweep because the spec writes it that way,
//! and because a 360° `swAng` starting and ending at the same point is exactly
//! the case an arc flattener has to special-case. Splitting at the left and
//! right quadrants leaves two unambiguous half turns.
//!
//! The shape has **no adjustment**: an ellipse is fully determined by its box.

use crate::model::PathFillMode;
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath, arc, turn};

/// cos 45° = sin 45°, the half-diagonal factor the spec's `idx`/`idy` guides
/// evaluate to (`cos wd2 2700000`, `sin hd2 2700000`).
const COS_45: f32 = std::f32::consts::FRAC_1_SQRT_2;

pub fn build(extent: PtSize) -> ShapePath {
    let (rx, ry) = (
        Pt::new(extent.width.raw() * 0.5),
        Pt::new(extent.height.raw() * 0.5),
    );

    let verbs = vec![
        // Start at the left quadrant (9 o'clock), sweep over the top to the
        // right quadrant, then back underneath.
        PathVerb::MoveTo(PtOffset::new(Pt::ZERO, ry)),
        arc(rx, ry, turn::HALF, turn::HALF),
        arc(rx, ry, turn::NONE, turn::HALF),
        PathVerb::Close,
    ];

    // §20.1.9.18's `<rect l="il" t="it" r="ir" b="ib"/>`: the rectangle
    // inscribed at 45°, not the bounding box — text in an ellipse has to clear
    // the curve on all four sides.
    let (ix, iy) = (rx.raw() * COS_45, ry.raw() * COS_45);
    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: PathFillMode::Norm,
            stroked: true,
        }],
        text_rect: Some(PtRect::from_xywh(
            Pt::new(rx.raw() - ix),
            Pt::new(ry.raw() - iy),
            Pt::new(ix * 2.0),
            Pt::new(iy * 2.0),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outline_is_two_half_turns_from_the_left_quadrant() {
        let p = build(PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        let verbs = &p.paths[0].verbs;
        assert_eq!(verbs.len(), 4);
        let PathVerb::MoveTo(start) = verbs[0] else {
            panic!("an ellipse starts with a moveTo")
        };
        // The pen starts at 9 o'clock: x = 0, y = half the height.
        assert_eq!(start, PtOffset::new(Pt::ZERO, Pt::new(50.0)));
        let mut swept = 0i64;
        for verb in verbs {
            if let PathVerb::ArcTo {
                radii, swing_angle, ..
            } = verb
            {
                // Radii are half the box, not the box — a whole-extent radius
                // draws an ellipse twice the shape's size.
                assert_eq!(*radii, PtSize::new(Pt::new(100.0), Pt::new(50.0)));
                swept += swing_angle.raw();
            }
        }
        assert_eq!(swept, 21_600_000, "the two arcs must close the figure");
    }

    #[test]
    fn the_text_rectangle_is_inscribed_not_the_bounding_box() {
        // Using the bounding box would let text sit in the corners, outside
        // the curve entirely.
        let p = build(PtSize::new(Pt::new(200.0), Pt::new(100.0)));
        let tr = p.text_rect.unwrap();
        assert!(tr.size.width.raw() < 200.0 && tr.size.height.raw() < 100.0);
        // Centred on the shape.
        assert!(
            ((tr.origin.x.raw() + tr.size.width.raw() * 0.5) - 100.0).abs() < 0.01,
            "the inscribed rect shares the ellipse's centre"
        );
        assert!(((tr.origin.y.raw() + tr.size.height.raw() * 0.5) - 50.0).abs() < 0.01);
    }

    #[test]
    fn a_zero_dimension_still_builds() {
        // `build_geometry` only rejects a shape that is zero in *both*
        // dimensions, so a degenerate ellipse must not panic here.
        let p = build(PtSize::new(Pt::new(40.0), Pt::ZERO));
        assert_eq!(p.paths.len(), 1);
    }
}
