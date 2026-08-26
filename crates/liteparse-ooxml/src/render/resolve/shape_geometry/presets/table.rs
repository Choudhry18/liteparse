//! The ECMA-376 preset shape table (§20.1.9.18 ST_ShapeType), as data.
//!
//! A preset shape *is* a custom geometry with a name. Every one of the spec's
//! 187 presets is an `avLst` + `gdLst` + `rect` + `pathLst` — the identical
//! content model to `<a:custGeom>`, evaluated by the identical guide
//! machinery. Once [`guides`](super::super::guides) implements the 17
//! operators and [`custom`](super::super::custom) walks every path verb, a
//! per-preset generator function adds nothing but a chance to disagree with
//! the spec.
//!
//! So the table is vendored rather than transcribed: `assets/preset_shapes.xml`
//! is the upstream definition file (see `assets/minify_presets.py` for the
//! provenance and the two transformations applied), parsed once on first use
//! through the same `CustomGeometryXml` schema `<a:custGeom>` goes through,
//! and looked up by [`PresetShapeType`].
//!
//! Names are mapped in exactly one direction. `map_preset_shape` — the
//! parser's `&str` → `PresetShapeType` function — is applied to the table's
//! own keys at load time, so there is no reverse map to drift, and a preset
//! outside the enum lands on `PresetShapeType::Other(name)` on *both* sides
//! and still resolves.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::docx::parse::drawing::schema::geometry::{
    CustomGeometryXml, GdListXml, PathListXml, TextRectXml,
};
use crate::docx::parse::drawing::schema::shape::map_preset_shape;
use crate::model::{CustomGeometry, GeomGuide, PresetShapeType};

const PRESET_SHAPES_XML: &str = include_str!("../../../../../assets/preset_shapes.xml");

/// `<presetShapes>` — the vendored table's root.
#[derive(Debug, Deserialize)]
struct PresetTableXml {
    #[serde(rename = "sp", default)]
    shapes: Vec<PresetEntryXml>,
}

/// One `<sp n="…">`. The child elements are `CT_CustomGeometry2D`'s, minus the
/// two subtrees the minifier drops (`ahLst`, `cxnLst`), so the entry is
/// re-assembled into a [`CustomGeometryXml`] and converted by the shared
/// `From` impl — this file never builds a `CustomGeometry` by hand.
#[derive(Debug, Deserialize)]
struct PresetEntryXml {
    #[serde(rename = "@n")]
    name: String,
    #[serde(rename = "avLst", default)]
    av_lst: Option<GdListXml>,
    #[serde(rename = "gdLst", default)]
    gd_lst: Option<GdListXml>,
    #[serde(rename = "rect", default)]
    rect: Option<TextRectXml>,
    #[serde(rename = "pathLst", default)]
    path_lst: Option<PathListXml>,
}

impl From<PresetEntryXml> for CustomGeometry {
    fn from(e: PresetEntryXml) -> Self {
        CustomGeometryXml {
            av_lst: e.av_lst,
            gd_lst: e.gd_lst,
            ah_lst: None,
            cxn_lst: None,
            rect: e.rect,
            path_lst: e.path_lst,
        }
        .into()
    }
}

fn table() -> &'static HashMap<PresetShapeType, CustomGeometry> {
    static TABLE: OnceLock<HashMap<PresetShapeType, CustomGeometry>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let parsed: PresetTableXml = match quick_xml::de::from_str(PRESET_SHAPES_XML) {
            Ok(t) => t,
            Err(e) => {
                // The asset ships with the crate, so this is a build-integrity
                // failure rather than a document problem: every preset falls
                // back to "unbuildable" and the log says why once.
                log::error!("shape_geometry: preset table failed to parse: {e}");
                return HashMap::new();
            }
        };
        parsed
            .shapes
            .into_iter()
            .map(|e| (map_preset_shape(&e.name), e.into()))
            .collect()
    })
}

/// The spec's definition for `preset`, with `adjust_values` applied.
///
/// §20.1.9.5: a shape's `<a:avLst>` *overrides* the preset's defaults by guide
/// name; names the preset does not define are still in scope for its `gdLst`,
/// so they are appended rather than dropped. Returns `None` only for a preset
/// absent from the table (`PresetShapeType::Other` for a name the spec has no
/// definition for).
/// Borrowed whenever the shape declares no `<a:avLst>` of its own, which is
/// the common case — the whole definition is then shared rather than cloned
/// per shape.
pub fn resolve<'a>(
    preset: &PresetShapeType,
    adjust_values: &[GeomGuide],
) -> Option<Cow<'a, CustomGeometry>> {
    let base = table().get(preset)?;
    if adjust_values.is_empty() {
        return Some(Cow::Borrowed(base));
    }
    let mut geom = base.clone();
    geom.av_list = merge_adjust_values(&geom.av_list, adjust_values);
    Some(Cow::Owned(geom))
}

fn merge_adjust_values(defaults: &[GeomGuide], overrides: &[GeomGuide]) -> Vec<GeomGuide> {
    let mut merged: Vec<GeomGuide> = defaults
        .iter()
        .map(|d| {
            overrides
                .iter()
                .find(|o| o.name == d.name)
                .unwrap_or(d)
                .clone()
        })
        .collect();
    merged.extend(
        overrides
            .iter()
            .filter(|o| !defaults.iter().any(|d| d.name == o.name))
            .cloned(),
    );
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guide(name: &str, formula: &str) -> GeomGuide {
        GeomGuide {
            name: name.to_string(),
            formula: formula.to_string(),
        }
    }

    #[test]
    fn table_holds_every_spec_preset() {
        // §20.1.9.18 enumerates 187 shape definitions. A short table means the
        // asset or the minifier regressed, which is otherwise silent — the
        // shapes would simply stop painting.
        assert_eq!(table().len(), 187);
    }

    #[test]
    fn every_key_maps_to_a_known_variant() {
        // `Other` in the table means the parser's name map has a hole: the
        // same document would resolve `Other("x")` and find it, but the enum
        // no longer describes the spec.
        let unknown: Vec<_> = table()
            .keys()
            .filter_map(|k| match k {
                PresetShapeType::Other(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert!(unknown.is_empty(), "unmapped preset names: {unknown:?}");
    }

    #[test]
    fn every_preset_has_at_least_one_path() {
        let empty: Vec<_> = table()
            .iter()
            .filter(|(_, g)| g.paths.is_empty())
            .map(|(k, _)| format!("{k:?}"))
            .collect();
        assert!(empty.is_empty(), "presets with no path: {empty:?}");
    }

    #[test]
    fn resolve_applies_adjust_value_overrides() {
        let geom = resolve(&PresetShapeType::RoundRect, &[guide("adj", "val 25000")])
            .expect("roundRect is in the table");
        assert_eq!(geom.av_list.len(), 1);
        assert_eq!(geom.av_list[0].name, "adj");
        assert_eq!(geom.av_list[0].formula, "val 25000");
    }

    #[test]
    fn resolve_keeps_defaults_for_unspecified_adjustments() {
        // round2DiagRect declares adj1 = 16667 and adj2 = 0; overriding only
        // adj2 must not drop adj1, or the shape squares off one corner.
        let geom = resolve(
            &PresetShapeType::Round2DiagRect,
            &[guide("adj2", "val 10000")],
        )
        .expect("round2DiagRect is in the table");
        let names: Vec<_> = geom.av_list.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["adj1", "adj2"]);
        assert_eq!(geom.av_list[0].formula, "val 16667");
        assert_eq!(geom.av_list[1].formula, "val 10000");
    }

    #[test]
    fn unknown_preset_is_not_in_the_table() {
        assert!(resolve(&PresetShapeType::Other("nonesuch".into()), &[]).is_none());
    }
}
