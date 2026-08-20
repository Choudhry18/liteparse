//! Evaluator for `<a:custGeom>` (§20.1.9.8).
//!
//! Each `<a:path>` declares its own coordinate space via `w`/`h`. The
//! evaluator:
//!
//! 1. Builds a `GuideContext` for the path using the path's `w`/`h` (in EMU,
//!    cast to f64). Shape-level and path-level guides are both evaluated
//!    against this context per §20.1.9.8.
//! 2. Evaluates `av_list` then `gd_list` in document order.
//! 3. Walks the path's `PathCommand` list, resolving each `AdjPoint` /
//!    `AdjCoord` / `AdjAngle` to a concrete path-local f64 via
//!    [`super::guides::resolve_adj_coord`].
//! 4. Scales the result from path-local units into the shape's `extent`
//!    (Pt), producing [`PathVerb`]s.
//!
//! Angles are preserved in 60000ths of a degree (§20.1.10.3) — we do not
//! scale them by the size transform; Skia picks them up in OOXML's native
//! convention via a painter-side conversion.

use crate::model::dimension::Dimension;
use crate::model::{AdjAngle, AdjCoord, AdjPoint, CustomGeometry, PathCommand, PathDef};
use crate::render::dimension::Pt;
use crate::render::geometry::{PtOffset, PtRect, PtSize};

use super::guides::{
    GuideContext, evaluate_guides, evaluate_guides_into, resolve_adj_angle, resolve_adj_coord,
};
use super::{PathVerb, ShapePath, SubPath};

pub fn build_custom(geom: &CustomGeometry, extent: PtSize) -> Option<ShapePath> {
    if geom.paths.is_empty() {
        return None;
    }
    let paths: Vec<SubPath> = geom
        .paths
        .iter()
        .filter_map(|p| build_subpath(geom, p, extent))
        .collect();

    if paths.is_empty() {
        return None;
    }

    // `rect` is resolved in the first path's coordinate space. When multiple
    // paths exist, the spec (§20.1.9.22) does not name a preferred scope —
    // we use the first as a convention.
    let text_rect = geom
        .rect
        .as_ref()
        .and_then(|r| resolve_text_rect(geom, &geom.paths[0], r, extent));

    Some(ShapePath { paths, text_rect })
}

fn build_subpath(geom: &CustomGeometry, path: &PathDef, extent: PtSize) -> Option<SubPath> {
    // §20.1.9.15 @w/@h: "If this attribute is omitted, then a value of 0 is
    // implied" — which names the shape's own coordinate space, not an empty
    // path (see `path_space`). This is what makes the preset table work at
    // all: 270 of the spec's 320 preset paths omit both attributes, and none
    // of those 270 uses a literal coordinate, so every value in them is a
    // guide resolved against this context.
    let path_w = path_space(path.w.raw() as f64, extent.width.raw() as f64);
    let path_h = path_space(path.h.raw() as f64, extent.height.raw() as f64);
    let ctx = GuideContext::new(path_w, path_h);

    // Evaluate shape-level guides first (av_list then gd_list, into one map:
    // a computed guide routinely reads an adjust value), then we're ready to
    // resolve `AdjCoord::Guide` references within the commands.
    let mut values = evaluate_guides(&geom.av_list, ctx);
    evaluate_guides_into(&mut values, &geom.gd_list, ctx);

    // Scale factors from path-local units → shape-local Pt.
    let sx = extent.width.raw() / path_w as f32;
    let sy = extent.height.raw() / path_h as f32;

    let mut verbs = Vec::with_capacity(path.commands.len());
    for cmd in &path.commands {
        match cmd {
            PathCommand::MoveTo(p) => {
                verbs.push(PathVerb::MoveTo(point_to_pt(p, &values, ctx, sx, sy)))
            }
            PathCommand::LineTo(p) => {
                verbs.push(PathVerb::LineTo(point_to_pt(p, &values, ctx, sx, sy)))
            }
            PathCommand::QuadBezTo(c, p) => verbs.push(PathVerb::QuadTo(
                point_to_pt(c, &values, ctx, sx, sy),
                point_to_pt(p, &values, ctx, sx, sy),
            )),
            PathCommand::CubicBezTo(c1, c2, p) => verbs.push(PathVerb::CubicTo(
                point_to_pt(c1, &values, ctx, sx, sy),
                point_to_pt(c2, &values, ctx, sx, sy),
                point_to_pt(p, &values, ctx, sx, sy),
            )),
            PathCommand::ArcTo {
                wr,
                hr,
                start_angle,
                swing_angle,
            } => {
                let wr_pt = Pt::new((coord_val(wr, &values, ctx) as f32) * sx);
                let hr_pt = Pt::new((coord_val(hr, &values, ctx) as f32) * sy);
                verbs.push(PathVerb::ArcTo {
                    radii: PtSize::new(wr_pt, hr_pt),
                    start_angle: angle(start_angle, &values, ctx),
                    swing_angle: angle(swing_angle, &values, ctx),
                });
            }
            PathCommand::Close => verbs.push(PathVerb::Close),
        }
    }

    Some(SubPath {
        verbs,
        fill_mode: path.fill,
        stroked: path.stroke,
    })
}

/// The coordinate space one axis of a path is expressed in: its own `@w`/`@h`
/// when declared, else the shape's extent on that axis (§20.1.9.15).
///
/// A zero *extent* is not degenerate the way a zero path space is — a vertical
/// line is authored as `cx=0, cy=N`, and its geometry is still a line. Falling
/// back to a unit space keeps the guides evaluable; the scale factor is then
/// `0 / 1`, which collapses every coordinate on that axis to 0. That is the
/// line.
fn path_space(declared: f64, extent: f64) -> f64 {
    if declared > 0.0 {
        declared
    } else if extent > 0.0 {
        extent
    } else {
        1.0
    }
}

fn resolve_text_rect(
    geom: &CustomGeometry,
    path: &PathDef,
    rect: &crate::model::TextRect,
    extent: PtSize,
) -> Option<PtRect> {
    // Same §20.1.9.15 rule as `build_subpath`: the text rect is resolved in
    // the first path's space, which for an omitted @w/@h is the shape's own.
    let path_w = path_space(path.w.raw() as f64, extent.width.raw() as f64);
    let path_h = path_space(path.h.raw() as f64, extent.height.raw() as f64);
    let ctx = GuideContext::new(path_w, path_h);
    let mut values = evaluate_guides(&geom.av_list, ctx);
    evaluate_guides_into(&mut values, &geom.gd_list, ctx);

    let sx = extent.width.raw() / path_w as f32;
    let sy = extent.height.raw() / path_h as f32;

    let l = (coord_val(&rect.left, &values, ctx) as f32) * sx;
    let t = (coord_val(&rect.top, &values, ctx) as f32) * sy;
    let r = (coord_val(&rect.right, &values, ctx) as f32) * sx;
    let b = (coord_val(&rect.bottom, &values, ctx) as f32) * sy;
    Some(PtRect::from_xywh(
        Pt::new(l),
        Pt::new(t),
        Pt::new((r - l).max(0.0)),
        Pt::new((b - t).max(0.0)),
    ))
}

fn point_to_pt(
    p: &AdjPoint,
    values: &super::guides::GuideValues,
    ctx: GuideContext,
    sx: f32,
    sy: f32,
) -> PtOffset {
    let x = (resolve_adj_coord(&p.x, values, ctx) as f32) * sx;
    let y = (resolve_adj_coord(&p.y, values, ctx) as f32) * sy;
    PtOffset::new(Pt::new(x), Pt::new(y))
}

fn coord_val(c: &AdjCoord, values: &super::guides::GuideValues, ctx: GuideContext) -> f64 {
    resolve_adj_coord(c, values, ctx)
}

fn angle(
    a: &AdjAngle,
    values: &super::guides::GuideValues,
    ctx: GuideContext,
) -> Dimension<crate::model::dimension::SixtieThousandthDeg> {
    Dimension::new(resolve_adj_angle(a, values, ctx) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GeomGuide, PathDef, PathFillMode};

    fn lit_point(x: i64, y: i64) -> AdjPoint {
        AdjPoint {
            x: AdjCoord::Lit(x),
            y: AdjCoord::Lit(y),
        }
    }

    fn emu(v: i64) -> Dimension<crate::model::dimension::Emu> {
        Dimension::new(v)
    }

    fn rect_path() -> PathDef {
        PathDef {
            w: emu(100),
            h: emu(50),
            fill: PathFillMode::Norm,
            stroke: true,
            extrusion_ok: true,
            commands: vec![
                PathCommand::MoveTo(lit_point(0, 0)),
                PathCommand::LineTo(lit_point(100, 0)),
                PathCommand::LineTo(lit_point(100, 50)),
                PathCommand::LineTo(lit_point(0, 50)),
                PathCommand::Close,
            ],
        }
    }

    #[test]
    fn scales_path_coords_to_shape_extent() {
        let geom = CustomGeometry {
            paths: vec![rect_path()],
            ..Default::default()
        };
        let extent = PtSize::new(Pt::new(200.0), Pt::new(100.0));
        let s = build_custom(&geom, extent).unwrap();
        let sub = &s.paths[0];
        // Second verb is LineTo(100, 0) in path-local, should scale to (200, 0).
        let PathVerb::LineTo(p) = sub.verbs[1] else {
            panic!()
        };
        assert_eq!(p.x, Pt::new(200.0));
        assert_eq!(p.y, Pt::new(0.0));
        // Fourth verb is LineTo(0, 50) in path-local, should scale to (0, 100).
        let PathVerb::LineTo(p) = sub.verbs[3] else {
            panic!()
        };
        assert_eq!(p.x, Pt::new(0.0));
        assert_eq!(p.y, Pt::new(100.0));
    }

    #[test]
    fn empty_paths_return_none() {
        let geom = CustomGeometry::default();
        assert!(build_custom(&geom, PtSize::new(Pt::new(10.0), Pt::new(10.0))).is_none());
    }

    #[test]
    fn zero_width_path_resolves_in_shape_space() {
        // §20.1.9.15: @w defaults to 0, which names the shape's own coordinate
        // space rather than an empty path. The literal 100 in `rect_path` is
        // then 100 Pt in a 10 Pt-wide shape — unscaled on x (the path space is
        // the shape space), halved on y where @h = 50 against a 10 Pt height.
        let mut path = rect_path();
        path.w = emu(0);
        let geom = CustomGeometry {
            paths: vec![path],
            ..Default::default()
        };
        let built = build_custom(&geom, PtSize::new(Pt::new(10.0), Pt::new(10.0)))
            .expect("a zero-width path space is the shape's, not nothing");
        let PathVerb::LineTo(p) = built.paths[0].verbs[1] else {
            panic!("expected a lnTo second")
        };
        assert!((p.x.raw() - 100.0).abs() < 1e-3);
        assert!((p.y.raw() - 0.0).abs() < 1e-3);
    }

    #[test]
    fn guide_reference_resolved_via_gd_list() {
        let geom = CustomGeometry {
            gd_list: vec![GeomGuide {
                name: "mid".into(),
                formula: "*/ w 1 2".into(),
            }],
            paths: vec![PathDef {
                w: emu(100),
                h: emu(50),
                fill: PathFillMode::Norm,
                stroke: true,
                extrusion_ok: true,
                commands: vec![
                    PathCommand::MoveTo(AdjPoint {
                        x: AdjCoord::Guide("mid".into()),
                        y: AdjCoord::Lit(0),
                    }),
                    PathCommand::LineTo(AdjPoint {
                        x: AdjCoord::Guide("mid".into()),
                        y: AdjCoord::Guide("h".into()),
                    }),
                ],
            }],
            ..Default::default()
        };
        let extent = PtSize::new(Pt::new(200.0), Pt::new(100.0));
        let s = build_custom(&geom, extent).unwrap();
        let PathVerb::MoveTo(p) = s.paths[0].verbs[0] else {
            panic!()
        };
        // mid = w/2 = 50 path-local → scale factor 2 → 100 Pt.
        assert_eq!(p.x, Pt::new(100.0));
        let PathVerb::LineTo(p) = s.paths[0].verbs[1] else {
            panic!()
        };
        // h = 50 path-local → scale factor 2 → 100 Pt.
        assert_eq!(p.y, Pt::new(100.0));
    }

    #[test]
    fn arc_to_preserves_angle_in_60k_deg() {
        let geom = CustomGeometry {
            paths: vec![PathDef {
                w: emu(100),
                h: emu(100),
                fill: PathFillMode::Norm,
                stroke: true,
                extrusion_ok: true,
                commands: vec![PathCommand::ArcTo {
                    wr: AdjCoord::Lit(25),
                    hr: AdjCoord::Lit(25),
                    start_angle: AdjCoord::Lit(0),
                    swing_angle: AdjCoord::Lit(5_400_000),
                }],
            }],
            ..Default::default()
        };
        let extent = PtSize::new(Pt::new(100.0), Pt::new(100.0));
        let s = build_custom(&geom, extent).unwrap();
        let PathVerb::ArcTo {
            start_angle,
            swing_angle,
            ..
        } = s.paths[0].verbs[0]
        else {
            panic!()
        };
        assert_eq!(start_angle.raw(), 0);
        assert_eq!(swing_angle.raw(), 5_400_000);
    }

    #[test]
    fn text_rect_resolves_and_scales() {
        let geom = CustomGeometry {
            rect: Some(crate::model::TextRect {
                left: AdjCoord::Lit(0),
                top: AdjCoord::Lit(0),
                right: AdjCoord::Lit(100),
                bottom: AdjCoord::Lit(50),
            }),
            paths: vec![rect_path()],
            ..Default::default()
        };
        let s = build_custom(&geom, PtSize::new(Pt::new(200.0), Pt::new(100.0))).unwrap();
        let tr = s.text_rect.unwrap();
        assert_eq!(tr.origin.x, Pt::ZERO);
        assert_eq!(tr.size.width, Pt::new(200.0));
        assert_eq!(tr.size.height, Pt::new(100.0));
    }
}
