//! Preset shapes (§20.1.9.18 ST_ShapeType).
//!
//! A preset *is* a custom geometry with a name: the same `avLst` + `gdLst` +
//! `rect` + `pathLst` content model, evaluated by the same guide machinery.
//! So there are no generators here — [`table`] holds the spec's own definition
//! for all 187 of them and [`build_preset`] runs it through the
//! `<a:custGeom>` evaluator.
//!
//! Note the §20.1.9.22 `roundRect` text rect is inset by 29.289% of the corner
//! radius (`il = x1 * 29289 / 100000`, the sagitta of a 45° arc), not by the
//! full radius — the table gets this right where a naive generator would not.
//!
//! A preset returns `None` only when §20.1.9.18 does not define the name at
//! all, i.e. `PresetShapeType::Other`. Callers log once and skip the shape;
//! nothing here approximates a shape by its bounding box.

pub mod table;

use crate::model::PresetGeometryDef;
use crate::render::geometry::PtSize;

use super::ShapePath;

/// Build a preset from the spec table. `None` when §20.1.9.18 defines no such
/// shape; the call site is expected to log.
pub fn build_preset(def: &PresetGeometryDef, extent: PtSize) -> Option<ShapePath> {
    match table::resolve(&def.preset, &def.adjust_values) {
        Some(geom) => super::custom::build_custom(&geom, extent),
        None => {
            log::warn!("shape_geometry: preset {:?} has no definition", def.preset);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PresetShapeType;
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
    fn table_preset_dispatches() {
        // Star12 comes from the vendored spec table like every other shape.
        let p = build_preset(
            &def(PresetShapeType::Star12),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_some());
    }

    #[test]
    fn preset_without_a_definition_returns_none() {
        let p = build_preset(
            &def(PresetShapeType::Other("nonesuch".into())),
            PtSize::new(Pt::new(10.0), Pt::new(20.0)),
        );
        assert!(p.is_none());
    }
}
