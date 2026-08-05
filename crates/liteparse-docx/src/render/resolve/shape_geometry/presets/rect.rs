//! §20.1.9.18 `rect` preset — a closed rectangle matching the shape's
//! bounding box. Fills with the shape's fill; strokes with the outline.

use crate::model::PathFillMode;
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

pub fn build(extent: PtSize) -> ShapePath {
    let (w, h) = (extent.width, extent.height);
    let verbs = vec![
        PathVerb::MoveTo(PtOffset::new(Pt::ZERO, Pt::ZERO)),
        PathVerb::LineTo(PtOffset::new(w, Pt::ZERO)),
        PathVerb::LineTo(PtOffset::new(w, h)),
        PathVerb::LineTo(PtOffset::new(Pt::ZERO, h)),
        PathVerb::Close,
    ];
    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: PathFillMode::Norm,
            stroked: true,
        }],
        // §20.1.9.22: the preset rect's text area is its full bounding box.
        text_rect: Some(crate::render::geometry::PtRect::from_xywh(
            Pt::ZERO,
            Pt::ZERO,
            w,
            h,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_five_verbs() {
        let p = build(PtSize::new(Pt::new(100.0), Pt::new(50.0)));
        assert_eq!(p.paths.len(), 1);
        assert_eq!(p.paths[0].verbs.len(), 5);
        assert!(matches!(p.paths[0].verbs[4], PathVerb::Close));
    }

    #[test]
    fn rect_text_rect_matches_extent() {
        let p = build(PtSize::new(Pt::new(100.0), Pt::new(50.0)));
        let tr = p.text_rect.unwrap();
        assert_eq!(tr.origin, PtOffset::new(Pt::ZERO, Pt::ZERO));
        assert_eq!(tr.size, PtSize::new(Pt::new(100.0), Pt::new(50.0)));
    }

    #[test]
    fn rect_fill_mode_is_normal() {
        let p = build(PtSize::new(Pt::new(10.0), Pt::new(10.0)));
        assert!(matches!(p.paths[0].fill_mode, PathFillMode::Norm));
    }
}
