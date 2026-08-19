//! Preset shape generators (§20.1.9.18 ST_ShapeType).
//!
//! Each generator is a pure function `PtSize → ShapePath`. Dispatch by
//! variant lives in [`build_preset`]. Unimplemented presets return `None`
//! and log once; callers should fall back to the shape's bounding box or
//! skip the shape.
//!
//! Tier 0 supports only `line` and `rect` — the minimum to validate the
//! pipeline end-to-end. Tier 1 adds the common ~20 shapes; Tier 2 adds the
//! remaining ~60; Tier 3 completes the spec's ~200.
//!
//! `roundRect`, `ellipse` and `straightConnector1` are here ahead of the rest
//! of tier 1 because the PPTX paint census counted what their absence costs:
//! of 5,848 corpus shapes that put ink on a slide, `rect`/`line`/`custGeom`
//! build 62.9% and these three take that to 87.1%. The shortfall is not spread
//! evenly — it is every rounded panel and every bullet dot — so shipping fills
//! without them drops a fifth of the ink *selectively*, which reads as a layout
//! bug rather than as missing coverage.

mod ellipse;
mod line;
mod rect;
mod round_rect;

use crate::model::{PresetGeometryDef, PresetShapeType};
use crate::render::geometry::PtSize;

use super::ShapePath;

/// Dispatch a preset to its generator. Returns `None` for presets not yet
/// implemented; the call site is expected to log.
pub fn build_preset(def: &PresetGeometryDef, extent: PtSize) -> Option<ShapePath> {
    match def.preset {
        // §20.1.9.18: `straightConnector1` is a `line` — the connector's
        // endpoints are its own box's corners, and the flips in `a:xfrm` are
        // what point it the other way.
        PresetShapeType::Line | PresetShapeType::StraightConnector1 => Some(line::build(extent)),
        PresetShapeType::Rect => Some(rect::build(extent)),
        PresetShapeType::RoundRect => Some(round_rect::build(&def.adjust_values, extent)),
        PresetShapeType::Ellipse => Some(ellipse::build(extent)),
        _ => {
            log::warn!(
                "shape_geometry: preset {:?} not yet implemented",
                def.preset
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::dimension::Pt;

    fn def(preset: PresetShapeType) -> PresetGeometryDef {
        PresetGeometryDef {
            preset,
            adjust_values: vec![],
        }
    }

    #[test]
    fn line_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Line),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn rect_dispatches() {
        let p = build_preset(
            &def(PresetShapeType::Rect),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn unknown_preset_returns_none() {
        let p = build_preset(
            &def(PresetShapeType::Star12),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_none());
    }
}
