//! §20.1.9.18 `line` preset — a single straight line from the top-left
//! corner (0, 0) to the bottom-right corner (w, h) of the shape's bounding
//! box, unstroked-by-default-fill (the outline supplies the visible stroke).

use crate::model::PathFillMode;
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtSize};
use crate::render::resolve::shape_geometry::{PathVerb, ShapePath, SubPath};

pub fn build(extent: PtSize) -> ShapePath {
    let verbs = vec![
        PathVerb::MoveTo(PtOffset::new(Pt::ZERO, Pt::ZERO)),
        PathVerb::LineTo(PtOffset::new(extent.width, extent.height)),
    ];
    ShapePath {
        paths: vec![SubPath {
            verbs,
            fill_mode: PathFillMode::None,
            stroked: true,
        }],
        text_rect: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_from_origin_to_extent() {
        let p = build(PtSize::new(Pt::new(100.0), Pt::new(50.0)));
        assert_eq!(p.paths.len(), 1);
        let sub = &p.paths[0];
        assert!(!matches!(sub.fill_mode, PathFillMode::Norm));
        assert!(sub.stroked);
        assert_eq!(sub.verbs.len(), 2);
        matches!(sub.verbs[0], PathVerb::MoveTo(_));
        matches!(sub.verbs[1], PathVerb::LineTo(_));
    }

    #[test]
    fn line_handles_vertical_shape() {
        let p = build(PtSize::new(Pt::ZERO, Pt::new(50.0)));
        let sub = &p.paths[0];
        let PathVerb::LineTo(pt) = sub.verbs[1] else {
            panic!()
        };
        assert_eq!(pt.x, Pt::ZERO);
        assert_eq!(pt.y, Pt::new(50.0));
    }

    #[test]
    fn line_text_rect_is_none() {
        let p = build(PtSize::new(Pt::new(10.0), Pt::new(10.0)));
        assert!(p.text_rect.is_none());
    }
}
