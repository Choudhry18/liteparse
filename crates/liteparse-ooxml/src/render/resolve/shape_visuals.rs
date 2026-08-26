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
    BlipFillKind, DrawingFill, Effect, GlowEffect, InnerShadowEffect, LineCap, LineDash, LineJoin,
    OuterShadowEffect, Outline, PresetShadowEffect, ReflectionEffect, ShapeProperties,
    SoftEdgeEffect, StyleMatrixRef, Theme,
};
use crate::render::dimension::Pt;
use crate::render::geometry::PtOffset;
use crate::render::layout::draw_command::{
    ResolvedBlip, ResolvedDashPattern, ResolvedEffect, ResolvedFill, ResolvedLineCap,
    ResolvedLineJoin, ResolvedStroke,
};
use crate::render::resolve::drawing_color::{DrawingColorContext, Rgba, resolve_drawing_color};
use crate::render::resolve::images::{PartMedia, relative_rect_to_fraction};

/// Resolved bundle for one shape.
pub struct ResolvedVisuals {
    pub fill: ResolvedFill,
    pub stroke: Option<ResolvedStroke>,
    pub effects: Vec<ResolvedEffect>,
    /// True when the stroke is painted black because *nothing* declared a
    /// colour — neither the direct `<a:ln>` nor the theme line style its
    /// `lnRef` names. Distinct from a stroke that is black because the file
    /// says black, which is not a defect.
    ///
    /// Reported rather than left for a caller to re-derive, because the only
    /// way to re-derive it is to redo the theme lookup this function already
    /// did, and a second copy of that lookup is a second thing to get wrong.
    pub stroke_color_defaulted: bool,
}

/// Resolve the visual aspect of a shape (fill, outline, effects) into the
/// painter-ready types. Missing `ShapeProperties` → empty / `None` visuals.
///
/// `style_effect_ref` is the `wps:style/effectRef` on the enclosing shape.
/// Per Word's rendering behavior — which the OOXML spec is ambiguous about —
/// a present-but-empty `<a:effectLst/>` on spPr falls through to the theme
/// effect style. Only when the direct effectLst has children do we treat it
/// as an explicit override.
///
/// Takes the whole [`DrawingColorContext`] rather than a bare `theme` because
/// the theme is no longer the only thing a scheme colour resolves through:
/// PresentationML states its `bg1`/`tx1` mapping in the master's §19.3.1.6
/// `p:clrMap`, and a shape fill has to see the same map its text does.
///
/// `group_fill` is the **already-resolved** fill of the nearest enclosing
/// group, which is what a `a:grpFill` on this shape inherits (§20.1.8.35);
/// `None` for a shape at the top of a tree, and for every DOCX caller. It
/// arrives resolved rather than as a `DrawingFill` for two reasons: the
/// group's fill resolves in the group's own colour context, which is the one
/// place it is correct to resolve it, and a large group resolves its fill once
/// instead of once per child.
pub fn resolve_shape_visuals(
    props: Option<&ShapeProperties>,
    style_line_ref: Option<&StyleMatrixRef>,
    style_effect_ref: Option<&StyleMatrixRef>,
    style_fill_ref: Option<&StyleMatrixRef>,
    ctx: &DrawingColorContext<'_>,
    media: Option<&PartMedia>,
    group_fill: Option<&ResolvedFill>,
) -> ResolvedVisuals {
    let theme = ctx.theme;
    let props = match props {
        Some(p) => p,
        None => {
            // No spPr, but a bare `<wps:style>` may still carry a fillRef.
            return ResolvedVisuals {
                fill: resolve_theme_fill(style_fill_ref, theme, ctx, media),
                stroke: None,
                effects: Vec::new(),
                stroke_color_defaulted: false,
            };
        }
    };

    // §20.1.4.1.13: a direct spPr fill wins; otherwise fall back to the theme
    // fill style referenced by `<a:fillRef>` (recolored by its phClr).
    // A `grpFill` is a *declared* fill, so it wins over a `fillRef` even when
    // the group hands back nothing: falling through to the theme on an
    // unanswered group would paint a colour the file never asks for.
    let fill = match props.fill.as_ref() {
        Some(f) => resolve_fill(f, ctx, media, group_fill),
        None => resolve_theme_fill(style_fill_ref, theme, ctx, media),
    };

    let theme_ln = theme_line_style(style_line_ref, theme);
    // §20.1.4.1.22: the theme line style is written in terms of `phClr`, which
    // the `lnRef` itself supplies — the same substitution `resolve_theme_fill`
    // does. Resolving the theme outline in the *shape's* context instead would
    // hand `phClr` to a scheme lookup that has never heard of it.
    let ln_ctx = match style_line_ref.and_then(|r| r.color.as_ref()) {
        Some(c) => ctx.with_placeholder(resolve_drawing_color(c, ctx)),
        None => *ctx,
    };
    let stroke = match props.outline.as_ref() {
        Some(o) => resolve_outline(o, theme_ln, ctx, &ln_ctx),
        // No `<a:ln>` at all: the `lnRef` is the whole outline, not just its
        // defaults. PowerPoint strokes these, so declining here would be a
        // missing outline rather than a conservative one.
        None => theme_ln.and_then(|t| resolve_outline(t, None, &ln_ctx, &ln_ctx)),
    };

    let direct_effects = props
        .effect_list
        .as_ref()
        .map(|el| el.effects.as_slice())
        .unwrap_or(&[]);
    let effects = if !direct_effects.is_empty() {
        resolve_effects(direct_effects, ctx)
    } else {
        resolve_theme_effects(style_effect_ref, theme, ctx)
    };

    let stroke_color_defaulted = stroke.as_ref().is_some_and(|&(_, d)| d);
    ResolvedVisuals {
        fill,
        stroke: stroke.map(|(s, _)| s),
        effects,
        stroke_color_defaulted,
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
    media: Option<&PartMedia>,
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
    // A theme fill style entry cannot itself defer to a group: there is no
    // shape, so there is no enclosing group.
    resolve_fill(fill, &fill_ctx, media, None)
}

/// §19.3.1.3: resolve a slide/layout/master `<p:bgRef>` against the theme.
///
/// **Deliberately not [`resolve_theme_fill`].** A `bgRef` indexes a different
/// matrix under a different convention: `idx` 1..=999 selects
/// `fillStyleLst[idx - 1]`, while **1001 and above select
/// `bgFillStyleLst[idx - 1001]`**. Backgrounds routinely use `idx="1001"`;
/// routing a `bgRef` through the shape path would look up index 1000 of a
/// 3-entry list, miss, and return [`ResolvedFill::None`] — a slide with no
/// background rather than an error.
///
/// Kept as a separate entry point rather than a branch inside
/// `resolve_theme_fill` so that a shape's `<a:fillRef>` — which has no
/// 1000-offset form — cannot start silently accepting one.
pub fn resolve_background_fill(
    bg_ref: &StyleMatrixRef,
    theme: Option<&Theme>,
    ctx: &DrawingColorContext<'_>,
    media: Option<&PartMedia>,
) -> ResolvedFill {
    // §20.1.4.2.19: 0 is the no-reference sentinel, here meaning "no
    // background" rather than "inherit".
    if bg_ref.idx == 0 {
        return ResolvedFill::None;
    }
    let Some(theme) = theme else {
        return ResolvedFill::None;
    };
    let fill = if bg_ref.idx > 1000 {
        theme.bg_fill_styles.get((bg_ref.idx as usize) - 1001)
    } else {
        theme.fill_styles.get((bg_ref.idx as usize) - 1)
    };
    let Some(fill) = fill else {
        return ResolvedFill::None;
    };
    let fill_ctx = match bg_ref.color.as_ref() {
        Some(c) => ctx.with_placeholder(resolve_drawing_color(c, ctx)),
        None => *ctx,
    };
    resolve_fill(fill, &fill_ctx, media, None)
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

// ── Fills ───────────────────────────────────────────────────────────────────

/// Resolve one fill descriptor.
///
/// `media` is the [`PartMedia`] of the part that **declares** this fill, and
/// `None` means the caller has no media channel at all (the DOCX shape path
/// today). A blip fill without one resolves to [`ResolvedFill::None`]: the
/// alternative — resolving against whatever table happened to be in scope —
/// is how an inherited picture acquires the slide's `rId1` instead of its own.
///
/// `group_fill` is the resolved fill of the nearest enclosing group — see
/// [`resolve_shape_visuals`]. `None` means there is no group to inherit from,
/// which is a correct reading of the file and not a gap.
pub fn resolve_fill(
    fill: &DrawingFill,
    ctx: &DrawingColorContext<'_>,
    media: Option<&PartMedia>,
    group_fill: Option<&ResolvedFill>,
) -> ResolvedFill {
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
        DrawingFill::Blip(blip_fill) => resolve_blip_fill(blip_fill, media),
        DrawingFill::Pattern(_) => {
            log::warn!("shape_visuals: pattern fill not yet resolved (Tier 3)");
            ResolvedFill::None
        }
        // §20.1.8.35 — take the enclosing group's fill. The chain through
        // nested groups is collapsed by the caller before it gets here: a
        // group whose own fill is itself `grpFill` resolves against *its*
        // parent, so what arrives is always a fill, never another deferral.
        DrawingFill::Group => match group_fill {
            Some(f) => f.clone(),
            None => {
                log::debug!(
                    "shape_visuals: grpFill with no enclosing group fill — nothing to inherit"
                );
                ResolvedFill::None
            }
        },
    }
}

/// §20.1.8.14 `a:blipFill` → the bytes the painter stretches into the path.
///
/// Four of the five ways this returns `None` are *correct* readings of the
/// file rather than gaps, which is why they are separated by a log level the
/// caller can act on:
///
/// * **no `a:blip` child** — an empty picture placeholder ("click to add
///   picture"), typically in a layout or master; painting anything would stamp
///   a phantom frame on every slide that inherits it.
/// * **`r:link`** — the image lives outside the package; there are no bytes to
///   draw and never will be from this file alone.
/// * **an `r:embed` the part does not declare**, or one whose target is
///   missing from the zip: a broken package, reported rather than guessed at.
/// * **`a:tile`** — the image repeats rather than stretching. Returned as
///   `None` on purpose: painting a tile stretched is a *wrong* slide in
///   exactly the way an unbuildable preset drawn as its bounding box would be,
///   so strictness here keeps the first real one from rendering silently wrong.
///
/// The crop goes through [`relative_rect_to_fraction`], shared with the
/// picture path, so a cropped fill and a cropped picture cannot diverge.
pub fn resolve_blip_fill(fill: &crate::model::BlipFill, media: Option<&PartMedia>) -> ResolvedFill {
    let Some(blip) = fill.blip.as_ref() else {
        log::debug!("shape_visuals: blipFill with no a:blip — empty picture placeholder");
        return ResolvedFill::None;
    };
    if let BlipFillKind::Tile(_) = fill.fill_kind {
        log::warn!("shape_visuals: tiled blip fill not painted — a stretch would be wrong");
        return ResolvedFill::None;
    }
    let Some(embed) = blip.embed.as_ref() else {
        // `r:link` is the only other way to name an image, and it is external.
        log::debug!("shape_visuals: blip has no r:embed (external r:link?) — nothing to paint");
        return ResolvedFill::None;
    };
    let Some(media) = media else {
        log::debug!("shape_visuals: blip fill on a path with no media channel");
        return ResolvedFill::None;
    };
    let Some(entry) = media.get(embed) else {
        log::warn!(
            "shape_visuals: blip r:embed {} not declared by the part that uses it",
            embed.as_str()
        );
        return ResolvedFill::None;
    };
    ResolvedFill::Blip(ResolvedBlip {
        data: entry.data.clone(),
        format: entry.format,
        src_rect: fill.src_rect.as_ref().and_then(relative_rect_to_fraction),
    })
}

// ── Outline → Stroke ────────────────────────────────────────────────────────

/// Resolve one `<a:ln>` into a painter stroke, with `theme_ln` supplying every
/// property the direct outline omits.
///
/// Two colour contexts, because the two outlines are written in different
/// vocabularies: `ctx` resolves the shape's own colours, and `ln_ctx` is `ctx`
/// with `phClr` bound to the `lnRef`'s colour, which is the only context the
/// theme line style's `<a:schemeClr val="phClr"/>` means anything in. Passing
/// one context for both would silently paint every inherited stroke black.
fn resolve_outline(
    outline: &Outline,
    theme_ln: Option<&Outline>,
    ctx: &DrawingColorContext<'_>,
    ln_ctx: &DrawingColorContext<'_>,
) -> Option<(ResolvedStroke, bool)> {
    // Width: direct `@w` wins; else theme `lnRef`; else spec default 0.75pt.
    // OOXML `w` is EMU (12700 per pt).
    let width = outline
        .width
        .or_else(|| theme_ln.and_then(|t| t.width))
        .map(emu_to_pt)
        .unwrap_or_else(|| Pt::new(0.75));

    // Pull the outline color from its fill. A direct fill wins; an outline
    // with no fill of its own inherits the theme line style's, which is what
    // `lnRef` exists for. Only when neither declares one is black correct.
    let declared = outline
        .fill
        .as_ref()
        .map(|f| (f, ctx))
        .or_else(|| theme_ln.and_then(|t| t.fill.as_ref()).map(|f| (f, ln_ctx)));
    let mut color_defaulted = false;
    let color = match declared {
        Some((DrawingFill::Solid(c), cx)) => resolve_drawing_color(c, cx),
        Some((DrawingFill::None, _)) => return None,
        Some((DrawingFill::Gradient(_) | DrawingFill::Blip(_) | DrawingFill::Pattern(_), _)) => {
            log::warn!("shape_visuals: non-solid stroke fill not yet supported");
            Rgba::BLACK
        }
        // `a:grpFill` on a stroke, or no colour anywhere in the cascade. Black
        // is a guess in both cases, and the flag says so.
        Some((DrawingFill::Group, _)) | None => {
            color_defaulted = true;
            Rgba::BLACK
        }
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

    Some((
        ResolvedStroke {
            width,
            color,
            dash,
            cap,
            join,
        },
        color_defaulted,
    ))
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
        let v = resolve_shape_visuals(
            None,
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            Some(&ln_ref),
            None,
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        let s = v.stroke.unwrap();
        assert_eq!(s.width, Pt::new(2.0));
        assert_eq!(s.color.to_rgb24(), 0xD99F34);
        assert_eq!(s.cap, ResolvedLineCap::Butt);
        assert!(!v.stroke_color_defaulted, "the shape declared the colour");
    }

    /// A theme line style whose colour is the `phClr` placeholder, which is how
    /// `lnStyleLst` entries are normally written.
    fn theme_with_phclr_line(width_emu: i64) -> Theme {
        use crate::model::DrawingColor;
        Theme {
            line_styles: vec![Outline {
                width: Some(Dimension::new(width_emu)),
                fill: Some(DrawingFill::Solid(DrawingColor::Scheme {
                    name: crate::model::SchemeColorVal::PhClr,
                    transforms: vec![],
                })),
                ..empty_outline()
            }],
            ..Theme::default()
        }
    }

    /// §20.1.4.1.22: an `<a:ln>` that names no colour takes the theme line
    /// style's, recoloured by the `lnRef`'s own `phClr` substitute — otherwise
    /// it would be painted black.
    #[test]
    fn colourless_outline_takes_its_colour_from_the_line_ref() {
        use crate::model::DrawingColor;
        let props = shape_props(
            None,
            Some(Outline {
                width: Some(Dimension::new(12_700)),
                ..empty_outline()
            }),
            None,
        );
        let theme = theme_with_phclr_line(25_400);
        let ln_ref = StyleMatrixRef {
            idx: 1,
            color: Some(DrawingColor::Srgb {
                rgb: 0x4472C4,
                transforms: vec![],
            }),
        };
        let v = resolve_shape_visuals(
            Some(&props),
            Some(&ln_ref),
            None,
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        let s = v.stroke.expect("a colourless outline is still an outline");
        assert_eq!(s.color.to_rgb24(), 0x4472C4, "phClr := the lnRef's colour");
        // The direct `@w` still wins over the theme's 2pt.
        assert_eq!(s.width, Pt::new(1.0));
        assert!(!v.stroke_color_defaulted);
    }

    /// No `<a:ln>` at all: the `lnRef` is the whole outline, which PowerPoint
    /// strokes.
    #[test]
    fn line_ref_supplies_the_whole_outline_when_there_is_no_ln() {
        use crate::model::DrawingColor;
        let props = shape_props(None, None, None);
        let theme = theme_with_phclr_line(25_400);
        let ln_ref = StyleMatrixRef {
            idx: 1,
            color: Some(DrawingColor::Srgb {
                rgb: 0xED7D31,
                transforms: vec![],
            }),
        };
        let v = resolve_shape_visuals(
            Some(&props),
            Some(&ln_ref),
            None,
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        let s = v.stroke.expect("the theme line style is the outline");
        assert_eq!(s.color.to_rgb24(), 0xED7D31);
        assert_eq!(s.width, Pt::new(2.0), "width comes from the theme too");
        assert!(!v.stroke_color_defaulted);
    }

    /// The flag means *nothing declared a colour anywhere*, not "the stroke is
    /// black". Without a `lnRef` there is nowhere else to look.
    #[test]
    fn colourless_outline_with_no_line_ref_reports_a_defaulted_colour() {
        let props = shape_props(
            None,
            Some(Outline {
                width: Some(Dimension::new(12_700)),
                ..empty_outline()
            }),
            None,
        );
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
        assert_eq!(v.stroke.unwrap().color.to_rgb24(), 0x000000);
        assert!(
            v.stroke_color_defaulted,
            "black here is a guess, not a fact"
        );
    }

    /// A shape with no `<a:ln>` and a `lnRef` the theme cannot satisfy stays
    /// unstroked — the miss must not become an invented black outline.
    #[test]
    fn line_ref_out_of_range_leaves_the_shape_unstroked() {
        let props = shape_props(None, None, None);
        let theme = theme_with_phclr_line(25_400);
        let ln_ref = StyleMatrixRef {
            idx: 3,
            color: None,
        };
        let v = resolve_shape_visuals(
            Some(&props),
            Some(&ln_ref),
            None,
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        assert!(v.stroke.is_none());
        assert!(!v.stroke_color_defaulted);
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            Some(&er),
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            Some(&er),
            None,
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            Some(&fill_ref),
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        let ResolvedFill::Solid(c) = v.fill else {
            panic!("expected solid theme fill, got {:?}", v.fill);
        };
        assert_eq!(c.to_rgb24(), 0xFF0000, "phClr substituted by fillRef color");
    }

    /// §20.1.8.35 — the enclosing group supplies the fill.
    #[test]
    fn grp_fill_takes_the_enclosing_group_fill() {
        let props = shape_props(Some(DrawingFill::Group), None, None);
        let group = ResolvedFill::Solid(Rgba::from_rgb24(0x00FF00));
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            None,
            &DrawingColorContext::new(None),
            None,
            Some(&group),
        );
        let ResolvedFill::Solid(c) = v.fill else {
            panic!("expected the group's fill, got {:?}", v.fill);
        };
        assert_eq!(c.to_rgb24(), 0x00FF00);
    }

    /// With no group to inherit from, a `grpFill` is still a *declared* fill:
    /// it resolves to nothing rather than falling through to the `fillRef`,
    /// which would paint a colour the file never asks for.
    #[test]
    fn unanswered_grp_fill_does_not_fall_through_to_fill_ref() {
        let theme = ph_theme();
        let fill_ref = StyleMatrixRef {
            idx: 1,
            color: Some(DrawingColor::Srgb {
                rgb: 0xFF0000,
                transforms: vec![],
            }),
        };
        let props = shape_props(Some(DrawingFill::Group), None, None);
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            Some(&fill_ref),
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        assert!(
            matches!(v.fill, ResolvedFill::None),
            "grpFill with no group must not inherit the theme fill, got {:?}",
            v.fill
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            Some(&fill_ref),
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
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
        let v = resolve_shape_visuals(
            Some(&props),
            None,
            None,
            Some(&fill_ref),
            &DrawingColorContext::new(Some(&theme)),
            None,
            None,
        );
        assert!(matches!(v.fill, ResolvedFill::None));
    }

    // ── blip fills ──────────────────────────────────────────────────────────

    fn blip_fill(embed: Option<&str>, kind: BlipFillKind) -> crate::model::BlipFill {
        crate::model::BlipFill {
            rotate_with_shape: None,
            dpi: None,
            blip: embed.map(|e| crate::model::Blip {
                embed: Some(crate::model::RelId::new(e)),
                link: None,
                compression: None,
            }),
            src_rect: None,
            fill_kind: kind,
        }
    }

    fn media_with(id: &str) -> PartMedia {
        let mut m = PartMedia::new();
        m.insert(
            crate::model::RelId::new(id),
            crate::render::resolve::images::MediaEntry {
                data: std::sync::Arc::from(&b"\x89PNG\r\n\x1a\n"[..]),
                format: crate::model::ImageFormat::Png,
            },
        );
        m
    }

    #[test]
    fn blip_resolves_against_its_own_parts_table() {
        let media = media_with("rId7");
        let fill = blip_fill(Some("rId7"), BlipFillKind::Unspecified);
        let ResolvedFill::Blip(b) = resolve_blip_fill(&fill, Some(&media)) else {
            panic!("expected a blip fill");
        };
        assert_eq!(b.format, crate::model::ImageFormat::Png);
        assert!(b.src_rect.is_none(), "no a:srcRect declared");
    }

    #[test]
    fn blip_embed_absent_from_this_parts_table_is_not_painted() {
        // A layout picture's rId looked up in the *slide's* media table must
        // resolve to nothing rather than to whatever that table happens to
        // hold under the same id.
        let media = media_with("rId1");
        let fill = blip_fill(Some("rId9"), BlipFillKind::Unspecified);
        assert!(matches!(
            resolve_blip_fill(&fill, Some(&media)),
            ResolvedFill::None
        ));
    }

    #[test]
    fn blip_fill_with_no_blip_child_is_an_empty_placeholder() {
        // "Click to add picture" placeholder (layouts and masters): correctly
        // paints nothing.
        let fill = blip_fill(None, BlipFillKind::Unspecified);
        assert!(matches!(
            resolve_blip_fill(&fill, Some(&media_with("rId1"))),
            ResolvedFill::None
        ));
    }

    #[test]
    fn tiled_blip_is_refused_rather_than_stretched() {
        // A tile drawn stretched is a *wrong* slide, not a coarse one — the
        // same rule that forbids drawing an unbuildable preset as its bbox.
        let media = media_with("rId7");
        let tile = BlipFillKind::Tile(crate::model::TileFill {
            tx: None,
            ty: None,
            sx: None,
            sy: None,
            flip: None,
            alignment: None,
        });
        let fill = blip_fill(Some("rId7"), tile);
        assert!(matches!(
            resolve_blip_fill(&fill, Some(&media)),
            ResolvedFill::None
        ));
    }

    #[test]
    fn blip_with_no_media_channel_is_not_painted() {
        let fill = blip_fill(Some("rId7"), BlipFillKind::Unspecified);
        assert!(matches!(resolve_blip_fill(&fill, None), ResolvedFill::None));
    }

    #[test]
    fn blip_src_rect_uses_the_shared_converter() {
        let media = media_with("rId7");
        let mut fill = blip_fill(Some("rId7"), BlipFillKind::Unspecified);
        fill.src_rect = Some(crate::model::RelativeRect {
            left: Some(Dimension::new(25000)),
            top: None,
            right: Some(Dimension::new(10000)),
            bottom: Some(Dimension::new(10000)),
        });
        let ResolvedFill::Blip(b) = resolve_blip_fill(&fill, Some(&media)) else {
            panic!("expected a blip fill");
        };
        let r = b.src_rect.expect("crop present");
        assert!((r.origin.x.raw() - 0.25).abs() < 1e-5);
        assert!((r.size.width.raw() - 0.65).abs() < 1e-5);
    }
}
