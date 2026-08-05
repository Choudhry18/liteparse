//! Resolve DrawingML shape visuals (fill / stroke / effects) from the
//! parsed model ADTs into painter-ready `Resolved*` types.
//!
//! The resolver is pure: given a shape's `ShapeProperties` and the active
//! theme, it produces concrete RGBA fills, point-sized strokes, and a flat
//! list of effects ready for the painter. Unsupported variants map to
//! sensible defaults (`ResolvedFill::None`, no stroke, empty effects) with
//! a log.

use crate::model::dimension::{Dimension, Emu};
use crate::model::{
    DrawingFill, Effect, GlowEffect, InnerShadowEffect, LineCap, LineDash, LineJoin,
    OuterShadowEffect, Outline, PathFillMode, PresetShadowEffect, ReflectionEffect,
    ShapeProperties, SoftEdgeEffect, StyleMatrixRef, Theme,
};
use crate::render::dimension::Pt;
use crate::render::geometry::PtOffset;
use crate::render::layout::draw_command::{
    ResolvedDashPattern, ResolvedEffect, ResolvedFill, ResolvedLineCap, ResolvedLineJoin,
    ResolvedStroke,
};
use crate::render::resolve::drawing_color::{DrawingColorContext, Rgba, resolve_drawing_color};

/// Resolved bundle for one shape.
pub struct ResolvedVisuals {
    pub fill: ResolvedFill,
    pub stroke: Option<ResolvedStroke>,
    pub effects: Vec<ResolvedEffect>,
}

/// Resolve the visual aspect of a shape (fill, outline, effects) into the
/// painter-ready types. Missing `ShapeProperties` → empty / `None` visuals.
///
/// `style_effect_ref` is the `wps:style/effectRef` on the enclosing shape.
/// Per Word's rendering behavior — which the OOXML spec is ambiguous about —
/// a present-but-empty `<a:effectLst/>` on spPr falls through to the theme
/// effect style. Only when the direct effectLst has children do we treat it
/// as an explicit override.
pub fn resolve_shape_visuals(
    props: Option<&ShapeProperties>,
    style_line_ref: Option<&StyleMatrixRef>,
    style_effect_ref: Option<&StyleMatrixRef>,
    style_fill_ref: Option<&StyleMatrixRef>,
    theme: Option<&Theme>,
) -> ResolvedVisuals {
    let ctx = DrawingColorContext::new(theme);
    let props = match props {
        Some(p) => p,
        None => {
            // No spPr, but a bare `<wps:style>` may still carry a fillRef.
            return ResolvedVisuals {
                fill: resolve_theme_fill(style_fill_ref, theme, &ctx),
                stroke: None,
                effects: Vec::new(),
            };
        }
    };

    // §20.1.4.1.13: a direct spPr fill wins; otherwise fall back to the theme
    // fill style referenced by `<a:fillRef>` (recolored by its phClr).
    let fill = match props.fill.as_ref() {
        Some(f) => resolve_fill(f, &ctx),
        None => resolve_theme_fill(style_fill_ref, theme, &ctx),
    };

    let theme_ln = theme_line_style(style_line_ref, theme);
    let stroke = props
        .outline
        .as_ref()
        .and_then(|o| resolve_outline(o, theme_ln, &ctx));

    let direct_effects = props
        .effect_list
        .as_ref()
        .map(|el| el.effects.as_slice())
        .unwrap_or(&[]);
    let effects = if !direct_effects.is_empty() {
        resolve_effects(direct_effects, &ctx)
    } else {
        resolve_theme_effects(style_effect_ref, theme, &ctx)
    };

    ResolvedVisuals {
        fill,
        stroke,
        effects,
    }
}

/// §20.1.4.1.13: resolve the theme fill style referenced by `<a:fillRef>`,
/// substituting `phClr` with the reference's own color. Returns
/// `ResolvedFill::None` when the ref is absent, `idx` is 0, or the theme
/// doesn't define the requested style.
fn resolve_theme_fill(
    style_fill_ref: Option<&StyleMatrixRef>,
    theme: Option<&Theme>,
    ctx: &DrawingColorContext<'_>,
) -> ResolvedFill {
    let Some(r) = style_fill_ref else {
        return ResolvedFill::None;
    };
    // §20.1.4.2.19: idx is 1-based; 0 is the no-reference sentinel.
    if r.idx == 0 {
        return ResolvedFill::None;
    }
    let Some(theme) = theme else {
        return ResolvedFill::None;
    };
    let Some(fill) = theme.fill_styles.get((r.idx as usize) - 1) else {
        return ResolvedFill::None;
    };
    // Substitute phClr with the fillRef's own color before resolving the fill.
    let fill_ctx = match r.color.as_ref() {
        Some(c) => ctx.with_placeholder(resolve_drawing_color(c, ctx)),
        None => *ctx,
    };
    resolve_fill(fill, &fill_ctx)
}

/// Look up a theme line style by its 1-based `lnRef` index.
fn theme_line_style<'a>(
    style_line_ref: Option<&StyleMatrixRef>,
    theme: Option<&'a Theme>,
) -> Option<&'a Outline> {
    let idx = style_line_ref?.idx;
    if idx == 0 {
        return None;
    }
    theme?.line_styles.get((idx as usize) - 1)
}

/// Consult the theme's `effectStyleLst` via a shape's `effectRef`. Returns an
/// empty list when the ref is absent, the index is out of range, or the theme
/// doesn't define the requested style.
fn resolve_theme_effects(
    style_effect_ref: Option<&StyleMatrixRef>,
    theme: Option<&Theme>,
    ctx: &DrawingColorContext<'_>,
) -> Vec<ResolvedEffect> {
    let Some(r) = style_effect_ref else {
        return Vec::new();
    };
    let Some(theme) = theme else {
        return Vec::new();
    };
    // §20.1.4.2.19: idx is 1-based. idx=0 is the no-reference sentinel.
    if r.idx == 0 {
        return Vec::new();
    }
    let slot = (r.idx as usize).saturating_sub(1);
    let Some(list) = theme.effect_styles.get(slot) else {
        return Vec::new();
    };
    resolve_effects(&list.effects, ctx)
}

/// Most preset shapes default the fill mode from `<a:path>`; for Tier 0
/// presets without a path-level fill directive, callers can read this.
pub fn default_path_fill_for_stroked_shape() -> PathFillMode {
    PathFillMode::None
}

// ── Fills ───────────────────────────────────────────────────────────────────

pub fn resolve_fill(fill: &DrawingFill, ctx: &DrawingColorContext<'_>) -> ResolvedFill {
    match fill {
        DrawingFill::None => ResolvedFill::None,
        DrawingFill::Solid(color) => ResolvedFill::Solid(resolve_drawing_color(color, ctx)),
        DrawingFill::Gradient(g) => {
            log::warn!("shape_visuals: gradient fill not yet resolved (Tier 2)");
            use crate::render::layout::draw_command::{
                GradientStopRgba, ResolvedGradient, ResolvedGradientKind,
            };
            let stops = g
                .stops
                .iter()
                .map(|s| GradientStopRgba {
                    position: s.position.to_fraction(),
                    color: resolve_drawing_color(&s.color, ctx),
                })
                .collect();
            let kind = match &g.shade_properties {
                crate::model::GradientShadeProperties::Linear { angle, .. } => {
                    ResolvedGradientKind::Linear {
                        angle_deg: angle.raw() as f32 / 60_000.0,
                    }
                }
                crate::model::GradientShadeProperties::Path { .. } => ResolvedGradientKind::Radial,
            };
            ResolvedFill::Gradient(ResolvedGradient { stops, kind })
        }
        DrawingFill::Blip(_) => {
            log::warn!("shape_visuals: blip fill not yet resolved (Tier 2)");
            ResolvedFill::None
        }
        DrawingFill::Pattern(_) => {
            log::warn!("shape_visuals: pattern fill not yet resolved (Tier 3)");
            ResolvedFill::None
        }
        DrawingFill::Group => {
            log::warn!(
                "shape_visuals: group fill (grpFill) not resolved — no enclosing group context"
            );
            ResolvedFill::None
        }
    }
}

// ── Outline → Stroke ────────────────────────────────────────────────────────

fn resolve_outline(
    outline: &Outline,
    theme_ln: Option<&Outline>,
    ctx: &DrawingColorContext<'_>,
) -> Option<ResolvedStroke> {
    // Width: direct `@w` wins; else theme `lnRef`; else spec default 0.75pt.
    // OOXML `w` is EMU (12700 per pt).
    let width = outline
        .width
        .or_else(|| theme_ln.and_then(|t| t.width))
        .map(emu_to_pt)
        .unwrap_or_else(|| Pt::new(0.75));

    // Pull the outline color from its fill. If the fill is non-solid or
    // absent, default to black; Tier 0 cannot paint gradient strokes.
    let color = match outline.fill.as_ref() {
        Some(DrawingFill::Solid(c)) => resolve_drawing_color(c, ctx),
        Some(DrawingFill::None) => return None,
        Some(DrawingFill::Gradient(_) | DrawingFill::Blip(_) | DrawingFill::Pattern(_)) => {
            log::warn!("shape_visuals: non-solid stroke fill not yet supported");
            Rgba::BLACK
        }
        Some(DrawingFill::Group) | None => Rgba::BLACK,
    };

    let cap = outline
        .cap
        .or_else(|| theme_ln.and_then(|t| t.cap))
        .map(map_line_cap)
        .unwrap_or(ResolvedLineCap::Butt);
    let join = outline
        .join
        .as_ref()
        .or_else(|| theme_ln.and_then(|t| t.join.as_ref()))
        .map(map_line_join)
        .unwrap_or(ResolvedLineJoin::Round);
    let dash = outline
        .dash
        .as_ref()
        .or_else(|| theme_ln.and_then(|t| t.dash.as_ref()))
        .map(|d| map_line_dash(d, width))
        .unwrap_or(ResolvedDashPattern::Solid);

    Some(ResolvedStroke {
        width,
        color,
        dash,
        cap,
        join,
    })
}

fn map_line_cap(cap: LineCap) -> ResolvedLineCap {
    match cap {
        LineCap::Flat => ResolvedLineCap::Butt,
        LineCap::Round => ResolvedLineCap::Round,
        LineCap::Square => ResolvedLineCap::Square,
    }
}

fn map_line_join(join: &LineJoin) -> ResolvedLineJoin {
    match join {
        LineJoin::Round => ResolvedLineJoin::Round,
        LineJoin::Bevel => ResolvedLineJoin::Bevel,
        LineJoin::Miter { .. } => ResolvedLineJoin::Miter,
    }
}

/// Map a `LineDash` into painter units. Preset dash patterns use the
/// canonical Microsoft ratios expressed as multiples of the stroke width.
/// Custom dashes carry their own dash/space pairs as thousandth-percent of
/// line width.
fn map_line_dash(dash: &LineDash, width: Pt) -> ResolvedDashPattern {
    use crate::model::PresetLineDashVal as P;
    match dash {
        LineDash::Preset(preset) => {
            let ratios: &[f32] = match preset {
                P::Solid => return ResolvedDashPattern::Solid,
                P::Dot | P::SysDot => &[1.0, 3.0],
                P::Dash => &[4.0, 3.0],
                P::LgDash => &[8.0, 3.0],
                P::DashDot => &[4.0, 3.0, 1.0, 3.0],
                P::LgDashDot => &[8.0, 3.0, 1.0, 3.0],
                P::LgDashDotDot => &[8.0, 3.0, 1.0, 3.0, 1.0, 3.0],
                P::SysDash => &[3.0, 1.0],
                P::SysDashDot => &[3.0, 1.0, 1.0, 1.0],
                P::SysDashDotDot => &[3.0, 1.0, 1.0, 1.0, 1.0, 1.0],
            };
            let w = width.raw();
            let dashes: Vec<Pt> = ratios.iter().map(|r| Pt::new(r * w)).collect();
            ResolvedDashPattern::Dashes(dashes)
        }
        LineDash::Custom(stops) => {
            if stops.is_empty() {
                return ResolvedDashPattern::Solid;
            }
            let w = width.raw();
            let mut out = Vec::with_capacity(stops.len() * 2);
            for s in stops {
                // §20.1.8.27: dash/space in 1000ths of a percent of line width.
                out.push(Pt::new(s.dash.to_fraction() * w));
                out.push(Pt::new(s.space.to_fraction() * w));
            }
            ResolvedDashPattern::Dashes(out)
        }
    }
}

// ── Effects ─────────────────────────────────────────────────────────────────

fn resolve_effects(effects: &[Effect], ctx: &DrawingColorContext<'_>) -> Vec<ResolvedEffect> {
    effects
        .iter()
        .filter_map(|e| resolve_effect(e, ctx))
        .collect()
}

fn resolve_effect(effect: &Effect, ctx: &DrawingColorContext<'_>) -> Option<ResolvedEffect> {
    match effect {
        Effect::OuterShdw(sh) => Some(resolve_outer_shadow(sh, ctx)),
        Effect::Blur(_b) => {
            log::warn!("shape_visuals: blur effect not yet rendered (Tier 2)");
            None
        }
        Effect::Glow(g) => {
            log::warn!("shape_visuals: glow effect not yet rendered (Tier 2)");
            let _: &GlowEffect = g;
            None
        }
        Effect::InnerShdw(s) => {
            log::warn!("shape_visuals: innerShdw not yet rendered (Tier 2)");
            let _: &InnerShadowEffect = s;
            None
        }
        Effect::PrstShdw(s) => {
            log::warn!("shape_visuals: prstShdw not yet rendered (Tier 2)");
            let _: &PresetShadowEffect = s;
            None
        }
        Effect::Reflection(r) => {
            log::warn!("shape_visuals: reflection not yet rendered (Tier 2)");
            let _: &ReflectionEffect = r;
            None
        }
        Effect::SoftEdge(s) => {
            log::warn!("shape_visuals: softEdge not yet rendered (Tier 2)");
            let _: &SoftEdgeEffect = s;
            None
        }
        Effect::FillOverlay(_) => {
            log::warn!("shape_visuals: fillOverlay not yet rendered (Tier 2)");
            None
        }
    }
}

fn resolve_outer_shadow(sh: &OuterShadowEffect, ctx: &DrawingColorContext<'_>) -> ResolvedEffect {
    // §20.1.8.45: `dist` = distance from shape, `dir` = angle from the
    // shape's top-left toward which the shadow is cast (60000ths of a
    // degree, clockwise positive, 0° = east).
    let dist = emu_to_pt(sh.distance);
    let dir_rad = (sh.direction.raw() as f32 / 60_000.0).to_radians();
    let dx = dist.raw() * dir_rad.cos();
    let dy = dist.raw() * dir_rad.sin();
    ResolvedEffect::OuterShadow {
        blur_radius: emu_to_pt(sh.blur_radius),
        offset: PtOffset::new(Pt::new(dx), Pt::new(dy)),
        color: resolve_drawing_color(&sh.color, ctx),
    }
}

// ── Unit helpers ────────────────────────────────────────────────────────────

/// Convert EMU (English Metric Units — 914400 per inch) to Pt (72 per inch).
pub fn emu_to_pt(emu: Dimension<Emu>) -> Pt {
    Pt::new(emu.raw() as f32 / 12_700.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        DrawingColor, DrawingFill, EffectList, Outline, PresetLineDashVal, SchemeColorVal,
        ShapeProperties,
    };

    fn empty_outline() -> Outline {
        Outline {
            width: None,
            cap: None,
            compound: None,
            alignment: None,
            fill: None,
            dash: None,
            join: None,
            head_end: None,
            tail_end: None,
        }
    }

    fn shape_props(
        fill: Option<DrawingFill>,
        outline: Option<Outline>,
        effects: Option<EffectList>,
    ) -> ShapeProperties {
        ShapeProperties {
            bw_mode: None,
            transform: None,
            geometry: None,
            fill,
            outline,
            effect_list: effects,
        }
    }

    #[test]
    fn empty_props_resolves_to_none_visuals() {
        let v = resolve_shape_visuals(None, None, None, None, None);
        assert!(matches!(v.fill, ResolvedFill::None));
        assert!(v.stroke.is_none());
        assert!(v.effects.is_empty());
    }

    #[test]
    fn solid_fill_srgb_resolves_to_rgba() {
        let props = shape_props(
            Some(DrawingFill::Solid(DrawingColor::Srgb {
                rgb: 0xD99F34,
                transforms: vec![],
            })),
            None,
            None,
        );
        let v = resolve_shape_visuals(Some(&props), None, None, None, None);
        let ResolvedFill::Solid(c) = v.fill else {
            panic!()
        };
        assert_eq!(c.to_rgb24(), 0xD99F34);
    }

    #[test]
    fn outline_with_solid_fill_resolves_to_stroke() {
        let outline = Outline {
            width: Some(Dimension::new(9525)), // 0.75pt
            cap: Some(LineCap::Round),
            compound: None,
            alignment: None,
            fill: Some(DrawingFill::Solid(DrawingColor::Srgb {
                rgb: 0x0000FF,
                transforms: vec![],
            })),
            dash: Some(LineDash::Preset(PresetLineDashVal::Dash)),
            join: Some(LineJoin::Miter { limit: None }),
            head_end: None,
            tail_end: None,
        };
        let props = shape_props(None, Some(outline), None);
        let v = resolve_shape_visuals(Some(&props), None, None, None, None);
        let s = v.stroke.unwrap();
        assert_eq!(s.width, Pt::new(0.75));
        assert_eq!(s.color.to_rgb24(), 0x0000FF);
        assert_eq!(s.cap, ResolvedLineCap::Round);
        assert_eq!(s.join, ResolvedLineJoin::Miter);
        match s.dash {
            ResolvedDashPattern::Dashes(_) => {}
            _ => panic!("expected dashed pattern"),
        }
    }

    #[test]
    fn outline_defaults_when_no_width_or_fill() {
        let outline = Outline {
            width: None,
            cap: None,
            compound: None,
            alignment: None,
            fill: None,
            dash: None,
            join: None,
            head_end: None,
            tail_end: None,
        };
        let props = shape_props(None, Some(outline), None);
        let v = resolve_shape_visuals(Some(&props), None, None, None, None);
        let s = v.stroke.unwrap();
        assert_eq!(s.width, Pt::new(0.75));
        assert_eq!(s.color, Rgba::BLACK);
        assert_eq!(s.cap, ResolvedLineCap::Butt);
        assert_eq!(s.join, ResolvedLineJoin::Round);
        assert!(matches!(s.dash, ResolvedDashPattern::Solid));
    }

    #[test]
    fn outline_nofill_suppresses_stroke() {
        let outline = Outline {
            width: Some(Dimension::new(9525)),
            cap: None,
            compound: None,
            alignment: None,
            fill: Some(DrawingFill::None),
            dash: None,
            join: None,
            head_end: None,
            tail_end: None,
        };
        let props = shape_props(None, Some(outline), None);
        let v = resolve_shape_visuals(Some(&props), None, None, None, None);
        assert!(v.stroke.is_none());
    }

    #[test]
    fn emu_to_pt_conversion() {
        assert_eq!(emu_to_pt(Dimension::new(12_700)), Pt::new(1.0));
        assert_eq!(emu_to_pt(Dimension::new(9525)), Pt::new(0.75));
        assert_eq!(emu_to_pt(Dimension::new(914_400)), Pt::new(72.0));
    }

    #[test]
    fn outline_width_falls_back_to_theme_ln_ref() {
        use crate::model::DrawingColor;
        // Shape's `<a:ln>` has only a solid color — width/cap/dash absent.
        let outline = Outline {
            width: None,
            cap: None,
            compound: None,
            alignment: None,
            fill: Some(DrawingFill::Solid(DrawingColor::Srgb {
                rgb: 0xD99F34,
                transforms: vec![],
            })),
            dash: None,
            join: None,
            head_end: None,
            tail_end: None,
        };
        // Theme lnStyleLst[1] = 2pt wide (25400 EMU).
        let theme_ln = Outline {
            width: Some(Dimension::new(25_400)),
            cap: Some(LineCap::Flat),
            compound: None,
            alignment: None,
            fill: None,
            dash: None,
            join: None,
            head_end: None,
            tail_end: None,
        };
        let theme = Theme {
            line_styles: vec![empty_outline(), theme_ln, empty_outline()],
            ..Theme::default()
        };
        let props = shape_props(None, Some(outline), None);
        let ln_ref = StyleMatrixRef {
            idx: 2,
            color: None,
        };
        let v = resolve_shape_visuals(Some(&props), Some(&ln_ref), None, None, Some(&theme));
        let s = v.stroke.unwrap();
        assert_eq!(s.width, Pt::new(2.0));
        assert_eq!(s.color.to_rgb24(), 0xD99F34);
        assert_eq!(s.cap, ResolvedLineCap::Butt);
    }

    #[test]
    fn empty_effect_list_falls_back_to_theme_via_effect_ref() {
        use crate::model::DrawingColor;
        // Empty direct effect list + effectRef idx=1 → theme effect style [0].
        let props = shape_props(None, None, Some(EffectList { effects: vec![] }));
        let theme = Theme {
            effect_styles: vec![EffectList {
                effects: vec![Effect::OuterShdw(OuterShadowEffect {
                    blur_radius: Dimension::new(25_400), // 2pt
                    distance: Dimension::new(12_700),    // 1pt
                    direction: Dimension::new(0),        // east
                    sx: Dimension::new(100_000),
                    sy: Dimension::new(100_000),
                    kx: Dimension::new(0),
                    ky: Dimension::new(0),
                    alignment: crate::model::RectAlignment::B,
                    rot_with_shape: None,
                    color: DrawingColor::Srgb {
                        rgb: 0x000000,
                        transforms: vec![],
                    },
                })],
            }],
            ..Theme::default()
        };
        let er = StyleMatrixRef {
            idx: 1,
            color: None,
        };
        let v = resolve_shape_visuals(Some(&props), None, Some(&er), None, Some(&theme));
        assert_eq!(v.effects.len(), 1);
    }

    #[test]
    fn direct_effect_list_overrides_theme() {
        use crate::model::DrawingColor;
        let own = Effect::OuterShdw(OuterShadowEffect {
            blur_radius: Dimension::new(50_800), // 4pt
            distance: Dimension::new(0),
            direction: Dimension::new(0),
            sx: Dimension::new(100_000),
            sy: Dimension::new(100_000),
            kx: Dimension::new(0),
            ky: Dimension::new(0),
            alignment: crate::model::RectAlignment::B,
            rot_with_shape: None,
            color: DrawingColor::Srgb {
                rgb: 0xFF0000,
                transforms: vec![],
            },
        });
        let props = shape_props(None, None, Some(EffectList { effects: vec![own] }));
        let theme = Theme {
            effect_styles: vec![EffectList {
                effects: vec![Effect::OuterShdw(OuterShadowEffect {
                    blur_radius: Dimension::new(12_700),
                    distance: Dimension::new(12_700),
                    direction: Dimension::new(0),
                    sx: Dimension::new(100_000),
                    sy: Dimension::new(100_000),
                    kx: Dimension::new(0),
                    ky: Dimension::new(0),
                    alignment: crate::model::RectAlignment::B,
                    rot_with_shape: None,
                    color: DrawingColor::Srgb {
                        rgb: 0x000000,
                        transforms: vec![],
                    },
                })],
            }],
            ..Theme::default()
        };
        let er = StyleMatrixRef {
            idx: 1,
            color: None,
        };
        let v = resolve_shape_visuals(Some(&props), None, Some(&er), None, Some(&theme));
        let ResolvedEffect::OuterShadow {
            blur_radius, color, ..
        } = &v.effects[0];
        assert_eq!(*blur_radius, Pt::new(4.0));
        assert_eq!(color.to_rgb24(), 0xFF0000);
    }

    #[test]
    fn outer_shadow_resolves_with_offset_from_angle() {
        let sh = OuterShadowEffect {
            blur_radius: Dimension::new(25_400), // 2pt
            distance: Dimension::new(38_100),    // 3pt
            direction: Dimension::new(0),        // 0° = east
            sx: Dimension::new(100_000),
            sy: Dimension::new(100_000),
            kx: Dimension::new(0),
            ky: Dimension::new(0),
            alignment: crate::model::RectAlignment::B,
            rot_with_shape: None,
            color: DrawingColor::Srgb {
                rgb: 0x000000,
                transforms: vec![],
            },
        };
        let props = shape_props(
            None,
            None,
            Some(EffectList {
                effects: vec![Effect::OuterShdw(sh)],
            }),
        );
        let v = resolve_shape_visuals(Some(&props), None, None, None, None);
        assert_eq!(v.effects.len(), 1);
        let ResolvedEffect::OuterShadow {
            blur_radius,
            offset,
            color,
        } = &v.effects[0];
        assert_eq!(*blur_radius, Pt::new(2.0));
        assert!((offset.x.raw() - 3.0).abs() < 1e-5);
        assert!(offset.y.raw().abs() < 1e-5);
        assert_eq!(color.to_rgb24(), 0x000000);
    }

    fn ph_theme() -> Theme {
        // A theme whose fill style 1 is solidFill(phClr) — the common shape
        // fill style, colored by whatever the fillRef supplies.
        Theme {
            fill_styles: vec![DrawingFill::Solid(DrawingColor::Scheme {
                name: SchemeColorVal::PhClr,
                transforms: vec![],
            })],
            ..Default::default()
        }
    }

    #[test]
    fn fill_ref_resolves_theme_fill_with_phclr_substitution() {
        let theme = ph_theme();
        let fill_ref = StyleMatrixRef {
            idx: 1,
            color: Some(DrawingColor::Srgb {
                rgb: 0xFF0000,
                transforms: vec![],
            }),
        };
        // No direct spPr fill → the theme fill referenced by fillRef supplies it.
        let props = shape_props(None, None, None);
        let v = resolve_shape_visuals(Some(&props), None, None, Some(&fill_ref), Some(&theme));
        let ResolvedFill::Solid(c) = v.fill else {
            panic!("expected solid theme fill, got {:?}", v.fill);
        };
        assert_eq!(c.to_rgb24(), 0xFF0000, "phClr substituted by fillRef color");
    }

    #[test]
    fn direct_fill_wins_over_fill_ref() {
        let theme = ph_theme();
        let fill_ref = StyleMatrixRef {
            idx: 1,
            color: Some(DrawingColor::Srgb {
                rgb: 0xFF0000,
                transforms: vec![],
            }),
        };
        let props = shape_props(
            Some(DrawingFill::Solid(DrawingColor::Srgb {
                rgb: 0x00FF00,
                transforms: vec![],
            })),
            None,
            None,
        );
        let v = resolve_shape_visuals(Some(&props), None, None, Some(&fill_ref), Some(&theme));
        let ResolvedFill::Solid(c) = v.fill else {
            panic!("expected solid fill");
        };
        assert_eq!(c.to_rgb24(), 0x00FF00, "direct spPr fill overrides fillRef");
    }

    #[test]
    fn fill_ref_idx_zero_is_no_fill() {
        let theme = ph_theme();
        let fill_ref = StyleMatrixRef {
            idx: 0,
            color: None,
        };
        let props = shape_props(None, None, None);
        let v = resolve_shape_visuals(Some(&props), None, None, Some(&fill_ref), Some(&theme));
        assert!(matches!(v.fill, ResolvedFill::None));
    }
}
