//! Rasterize [`LayoutedPage`] draw commands to RGBA bitmaps — tiny-skia +
//! skrifa, no Skia proper.
//!
//! liteparse addition (no upstream equivalent; upstream rasterizes through
//! `painter.rs` + Skia, which the C′ vendor dropped). This module is the
//! "rasterise from layout draw-commands directly" option from
//! `NATIVE_OFFICE_PLAN.md`: because glyph placement re-derives pen advances
//! from the same [`FontRegistry`]/skrifa faces the layout measured with, the
//! raster is in the *same coordinate space* as the native `TextItem` geometry
//! by construction — which is what makes highlight-on-screenshot safe, the
//! thing the LibreOffice screenshot path could never guarantee.
//!
//! Fidelity tiers, mirroring the vendored painter's own tiering:
//! - **Rendered faithfully**: text (monochrome outlines), underlines, lines,
//!   rects, images (PNG/JPEG/GIF/BMP/TIFF/WebP incl. `src_rect` crops), shape
//!   paths with solid fills and solid/dashed strokes.
//! - **Approximated**: gradient fills collapse to their mean stop color
//!   (logged once); emoji clusters draw monochrome outlines when the resolved
//!   face has them, else nothing.
//! - **Skipped**: blip/pattern fills, effects (shadow/glow), EMF/WMF/SVG
//!   media (no decoder — same set `collect_images` skips), color glyph
//!   tables. Each skip logs once per process, never per command.
//!
//! The output is plain `{rgba, width, height}` so consumers need no tiny-skia
//! types; the buffer is effectively straight (non-premultiplied) RGBA because
//! every page starts from opaque white, so composited alpha is always 255.

use std::sync::Once;

use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PixmapPaint, Shader, Stroke,
    StrokeDash, Transform,
};

use crate::model::{ImageFormat, PathFillMode};
use crate::render::dimension::Pt;
use crate::render::fonts::{FontRegistry, FontStyle, TypefaceEntry};
use crate::render::geometry::{PtOffset, PtRect};
use crate::render::layout::draw_command::{
    DrawCommand, LayoutedPage, ResolvedDashPattern, ResolvedFill, ResolvedLineCap,
    ResolvedLineJoin, ResolvedStroke, TransformMark,
};
use crate::render::resolve::color::RgbColor;
use crate::render::resolve::drawing_color::Rgba;
use crate::render::resolve::images::MediaEntry;
use crate::render::resolve::shape_geometry::{PathVerb, SubPath};

/// One rasterized page. `rgba` is `width * height * 4` bytes, row-major,
/// fully opaque (see module docs).
pub struct RasterPage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Page size in Pt, for consumers that scale pixel coordinates back to
    /// viewport space.
    pub page_width_pt: f32,
    pub page_height_pt: f32,
}

/// Rasterize one page at `scale` device pixels per Pt (`dpi / 72`).
///
/// `registry` must be the registry the layout ran with — resolution rules are
/// per-registry state, and a different registry could pick different faces
/// than the ones the line breaks were measured against.
pub fn rasterize_page(
    page: &LayoutedPage,
    registry: &FontRegistry,
    scale: f32,
) -> Result<RasterPage, String> {
    let page_w = page.page_size.width.raw();
    let page_h = page.page_size.height.raw();
    let px_w = (page_w * scale).round().max(1.0) as u32;
    let px_h = (page_h * scale).round().max(1.0) as u32;
    let mut pixmap =
        Pixmap::new(px_w, px_h).ok_or_else(|| format!("cannot allocate {px_w}x{px_h} pixmap"))?;
    pixmap.fill(Color::WHITE);

    // Page Pt → device px. Every path below is built in page-Pt coordinates
    // and handed to tiny-skia with this transform (possibly pre-concatenated
    // with a shape-local placement), so there is exactly one scaling site.
    let device = Transform::from_scale(scale, scale);

    // Layered passes, each in stream order. `DrawCommand` carries no z-order
    // and §20.4.2.3 `behindDoc` is honored by emission position only *within*
    // a header/footer run — a body-anchored behind-doc shape lands mid-stream
    // after the header's text and would paint over it in one sequential pass
    // (upstream's Skia painter has the same artifact). The layering encodes
    // what the flags mean in practice:
    //
    //   Shape (floats: banners, watermarks — usually behind-doc)
    //   < Shading (highlight/cell-shading rects, tied to the text flow)
    //   < Media (placed images; inline ones must beat cell shading)
    //   < Ink (glyphs, rules, borders)
    //
    // Cost of the approximation: a deliberately in-front float no longer
    // covers text/shading — rare, and its fill still paints. A shape's own
    // text-box text is emitted as separate `Text` commands, so it stays over
    // its fill in the ink pass.
    for pass in [
        RasterPass::Shape,
        RasterPass::Shading,
        RasterPass::Media,
        RasterPass::Ink,
    ] {
        // The shape-local → page transform opened by the innermost
        // `DrawCommand::Transform` bracket, or the identity outside one.
        // Tracked per pass, and updated *before* the pass filter: every pass
        // walks the whole command list, so a bracket skipped as "not my pass"
        // would leave the pass that does paint inside it in the wrong space.
        let mut local = Transform::identity();
        for cmd in &page.commands {
            if let DrawCommand::Transform(mark) = cmd {
                local = match mark {
                    TransformMark::Begin(t) => {
                        place_transform(t.origin, t.rotation, t.flip_h, t.flip_v, t.extent)
                    }
                    TransformMark::End => Transform::identity(),
                };
                continue;
            }
            if raster_pass(cmd) != Some(pass) {
                continue;
            }
            let device = device.pre_concat(local);
            match cmd {
                DrawCommand::Text {
                    position,
                    text,
                    font_family,
                    char_spacing,
                    font_size,
                    bold,
                    italic,
                    color,
                    text_scale,
                } => {
                    let entry =
                        registry.resolve(font_family, FontStyle::from_flags(*bold, *italic));
                    draw_text_run(
                        &mut pixmap,
                        registry,
                        &entry,
                        text,
                        *position,
                        *font_size,
                        *text_scale,
                        *char_spacing,
                        rgb_color(*color),
                        device,
                    );
                }
                DrawCommand::Underline { line, color, width }
                | DrawCommand::Line { line, color, width } => {
                    let mut pb = PathBuilder::new();
                    pb.move_to(line.start.x.raw(), line.start.y.raw());
                    pb.line_to(line.end.x.raw(), line.end.y.raw());
                    if let Some(path) = pb.finish() {
                        let stroke = Stroke {
                            width: width.raw().max(0.2),
                            ..Stroke::default()
                        };
                        pixmap.stroke_path(&path, &solid(rgb_color(*color)), &stroke, device, None);
                    }
                }
                DrawCommand::Rect { rect, color } => {
                    if let Some(r) = skia_rect(rect) {
                        pixmap.fill_rect(r, &solid(rgb_color(*color)), device, None);
                    }
                }
                DrawCommand::Image {
                    rect,
                    image_data,
                    src_rect,
                } => {
                    draw_image(&mut pixmap, image_data, rect, src_rect.as_ref(), device);
                }
                DrawCommand::EmojiCluster {
                    rect,
                    text,
                    typeface,
                    size,
                    ..
                } => {
                    draw_emoji_fallback(&mut pixmap, registry, typeface, text, rect, *size, device);
                }
                DrawCommand::Path {
                    origin,
                    rotation,
                    flip_h,
                    flip_v,
                    extent,
                    paths,
                    fill,
                    stroke,
                    effects: _, // shadow/glow: skipped at this tier
                } => {
                    let transform = device.pre_concat(place_transform(
                        *origin, *rotation, *flip_h, *flip_v, *extent,
                    ));
                    for sub in paths {
                        draw_subpath(&mut pixmap, sub, fill, stroke.as_ref(), transform);
                    }
                }
                // Non-drawing commands: link/destination/outline metadata
                // (`raster_pass` returns `None` for these, so this arm is
                // unreachable and exists for exhaustiveness).
                DrawCommand::LinkAnnotation { .. }
                | DrawCommand::InternalLink { .. }
                | DrawCommand::NamedDestination { .. }
                | DrawCommand::Outline(_)
                | DrawCommand::Transform(_) => {}
            }
        }
    }

    Ok(RasterPage {
        rgba: pixmap.take(),
        width: px_w,
        height: px_h,
        page_width_pt: page_w,
        page_height_pt: page_h,
    })
}

/// Which paint layer a command belongs to (see the layering comment in
/// [`rasterize_page`]). `None` = not painted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RasterPass {
    /// Anchored shape geometry (floats — banners, watermarks).
    Shape,
    /// Highlight and cell/paragraph shading rects.
    Shading,
    /// Placed raster images.
    Media,
    /// Glyphs, emoji, underlines, table/border lines.
    Ink,
}

fn raster_pass(cmd: &DrawCommand) -> Option<RasterPass> {
    match cmd {
        DrawCommand::Path { .. } => Some(RasterPass::Shape),
        DrawCommand::Rect { .. } => Some(RasterPass::Shading),
        DrawCommand::Image { .. } => Some(RasterPass::Media),
        DrawCommand::Text { .. }
        | DrawCommand::Underline { .. }
        | DrawCommand::Line { .. }
        | DrawCommand::EmojiCluster { .. } => Some(RasterPass::Ink),
        DrawCommand::LinkAnnotation { .. }
        | DrawCommand::InternalLink { .. }
        | DrawCommand::NamedDestination { .. }
        | DrawCommand::Outline(_)
        // Handled before this filter runs — a bracket belongs to no single
        // pass, it changes the space every pass paints the run in.
        | DrawCommand::Transform(_) => None,
    }
}

/// Shape-local Pt → page Pt: flips and rotation happen about the shape's
/// centre, then the shape lands at `origin`.
///
/// Shared by [`DrawCommand::Path`], which carries these fields inline, and by
/// the [`TransformMark`] bracket, which carries the same five for a run of
/// commands. One function so a bracketed text body and a shape's own geometry
/// cannot land in different places on the same slide.
fn place_transform(
    origin: PtOffset,
    rotation: crate::model::dimension::Dimension<crate::model::dimension::SixtieThousandthDeg>,
    flip_h: bool,
    flip_v: bool,
    extent: crate::render::geometry::PtSize,
) -> Transform {
    let (cx, cy) = (extent.width.raw() / 2.0, extent.height.raw() / 2.0);
    let mut place = Transform::from_translate(origin.x.raw(), origin.y.raw());
    let deg = rotation.raw() as f32 / 60_000.0;
    if deg != 0.0 {
        place = place.pre_concat(Transform::from_rotate_at(deg, cx, cy));
    }
    if flip_h || flip_v {
        let (sx, sy) = (
            if flip_h { -1.0 } else { 1.0 },
            if flip_v { -1.0 } else { 1.0 },
        );
        place = place
            .pre_concat(Transform::from_translate(cx, cy))
            .pre_concat(Transform::from_scale(sx, sy))
            .pre_concat(Transform::from_translate(-cx, -cy));
    }
    place
}

// ─── Text ───────────────────────────────────────────────────────────────────

/// skrifa outline pen writing into a tiny-skia path builder.
///
/// skrifa emits glyph-local coordinates y-up; the page is y-down, so y is
/// negated about the baseline. `sx` carries §17.3.2.45 horizontal scale so
/// stretched text stretches its glyphs, not just its advances (upstream's
/// painter does the same via `Font::set_scale_x`).
struct GlyphPen<'a> {
    pb: &'a mut PathBuilder,
    x: f32,
    baseline_y: f32,
    sx: f32,
}

impl OutlinePen for GlyphPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.pb.move_to(self.x + x * self.sx, self.baseline_y - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.pb.line_to(self.x + x * self.sx, self.baseline_y - y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.pb.quad_to(
            self.x + cx0 * self.sx,
            self.baseline_y - cy0,
            self.x + x * self.sx,
            self.baseline_y - y,
        );
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.pb.cubic_to(
            self.x + cx0 * self.sx,
            self.baseline_y - cy0,
            self.x + cx1 * self.sx,
            self.baseline_y - cy1,
            self.x + x * self.sx,
            self.baseline_y - y,
        );
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

/// Draw one `Text` command: cmap walk, glyph outlines onto one path, single
/// fill. The pen advance per char is `advance(gid) * text_scale +
/// char_spacing` — the same arithmetic [`TextMeasurer::measure`] sums, so ink
/// lands inside the measured extent by construction.
///
/// [`TextMeasurer::measure`]: crate::render::layout::measurer::TextMeasurer::measure
#[allow(clippy::too_many_arguments)]
fn draw_text_run(
    pixmap: &mut Pixmap,
    registry: &FontRegistry,
    entry: &TypefaceEntry,
    text: &str,
    position: PtOffset,
    font_size: Pt,
    text_scale: f32,
    char_spacing: Pt,
    color: Color,
    device: Transform,
) {
    let path = registry.db().with_face_data(entry.id, |data, index| {
        let font = skrifa::FontRef::from_index(data, index).ok()?;
        let charmap = font.charmap();
        let outlines = font.outline_glyphs();
        let metrics = font.glyph_metrics(Size::new(font_size.raw()), LocationRef::default());

        let mut pb = PathBuilder::new();
        let mut pen_x = position.x.raw();
        for ch in text.chars() {
            let gid = charmap.map(ch).unwrap_or(skrifa::GlyphId::NOTDEF);
            if let Some(glyph) = outlines.get(gid) {
                let mut pen = GlyphPen {
                    pb: &mut pb,
                    x: pen_x,
                    baseline_y: position.y.raw(),
                    sx: text_scale,
                };
                let settings =
                    DrawSettings::unhinted(Size::new(font_size.raw()), LocationRef::default());
                // A malformed glyph program draws nothing; the advance below
                // still moves the pen so the rest of the run stays aligned.
                let _ = glyph.draw(settings, &mut pen);
            }
            pen_x += metrics.advance_width(gid).unwrap_or(0.0) * text_scale + char_spacing.raw();
        }
        pb.finish()
    });
    if let Some(Some(path)) = path {
        // Non-zero winding is the convention for TrueType/CFF outlines.
        pixmap.fill_path(&path, &solid(color), FillRule::Winding, device, None);
    }
}

/// Monochrome fallback for an emoji cluster: draw the resolved face's
/// outlines if it has any. Color glyph tables (COLR/CBDT/sbix) are not
/// rendered at this tier; faces that carry only those draw nothing.
fn draw_emoji_fallback(
    pixmap: &mut Pixmap,
    registry: &FontRegistry,
    typeface: &TypefaceEntry,
    text: &str,
    rect: &PtRect,
    size: Pt,
    device: Transform,
) {
    static EMOJI_ONCE: Once = Once::new();
    let drew = registry.db().with_face_data(typeface.id, |data, index| {
        let font = skrifa::FontRef::from_index(data, index).ok()?;
        let outlines = font.outline_glyphs();
        let charmap = font.charmap();
        let m = font.metrics(Size::new(size.raw()), LocationRef::default());
        let metrics = font.glyph_metrics(Size::new(size.raw()), LocationRef::default());
        let baseline_y = rect.origin.y.raw() + m.ascent;

        let mut pb = PathBuilder::new();
        let mut pen_x = rect.origin.x.raw();
        for ch in text.chars() {
            let Some(gid) = charmap.map(ch) else { continue };
            if let Some(glyph) = outlines.get(gid) {
                let mut pen = GlyphPen {
                    pb: &mut pb,
                    x: pen_x,
                    baseline_y,
                    sx: 1.0,
                };
                let settings =
                    DrawSettings::unhinted(Size::new(size.raw()), LocationRef::default());
                let _ = glyph.draw(settings, &mut pen);
            }
            pen_x += metrics.advance_width(gid).unwrap_or(0.0);
        }
        pb.finish()
    });
    match drew {
        Some(Some(path)) => {
            pixmap.fill_path(&path, &solid(Color::BLACK), FillRule::Winding, device, None);
        }
        _ => EMOJI_ONCE.call_once(|| {
            log::info!("emoji cluster face has no outline glyphs; clusters render blank");
        }),
    }
}

// ─── Images ─────────────────────────────────────────────────────────────────

fn draw_image(
    pixmap: &mut Pixmap,
    media: &MediaEntry,
    rect: &PtRect,
    src_rect: Option<&PtRect>,
    device: Transform,
) {
    static VECTOR_ONCE: Once = Once::new();
    match media.format {
        ImageFormat::Emf | ImageFormat::Wmf | ImageFormat::Svg | ImageFormat::Unknown => {
            VECTOR_ONCE.call_once(|| {
                log::info!("EMF/WMF/SVG media are not rasterized; placements render blank");
            });
            return;
        }
        _ => {}
    }
    let Ok(decoded) = image::load_from_memory(&media.data) else {
        log::warn!(
            "undecodable {:?} media ({} bytes)",
            media.format,
            media.data.len()
        );
        return;
    };
    let mut rgba = decoded.to_rgba8();

    // §20.1.10.48 srcRect: fractional crop of the natural extent, applied
    // before stretching into `rect`.
    if let Some(crop) = src_rect {
        let (w, h) = (rgba.width() as f32, rgba.height() as f32);
        let x = (crop.origin.x.raw() * w).round().clamp(0.0, w) as u32;
        let y = (crop.origin.y.raw() * h).round().clamp(0.0, h) as u32;
        let cw = (crop.size.width.raw() * w).round().max(1.0) as u32;
        let ch = (crop.size.height.raw() * h).round().max(1.0) as u32;
        let cw = cw.min(rgba.width().saturating_sub(x)).max(1);
        let ch = ch.min(rgba.height().saturating_sub(y)).max(1);
        rgba = image::imageops::crop_imm(&rgba, x, y, cw, ch).to_image();
    }

    let (iw, ih) = (rgba.width(), rgba.height());
    // tiny-skia sources are premultiplied RGBA.
    let mut data = rgba.into_raw();
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u16;
        if a != 255 {
            px[0] = ((px[0] as u16 * a) / 255) as u8;
            px[1] = ((px[1] as u16 * a) / 255) as u8;
            px[2] = ((px[2] as u16 * a) / 255) as u8;
        }
    }
    let Some(size) = tiny_skia::IntSize::from_wh(iw, ih) else {
        return;
    };
    let Some(src) = Pixmap::from_vec(data, size) else {
        return;
    };

    let sx = rect.size.width.raw() / iw as f32;
    let sy = rect.size.height.raw() / ih as f32;
    if sx <= 0.0 || sy <= 0.0 {
        return;
    }
    let transform = device.pre_concat(
        Transform::from_translate(rect.origin.x.raw(), rect.origin.y.raw())
            .pre_concat(Transform::from_scale(sx, sy)),
    );
    let paint = PixmapPaint {
        quality: tiny_skia::FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, None);
}

// ─── Shape paths ────────────────────────────────────────────────────────────

fn draw_subpath(
    pixmap: &mut Pixmap,
    sub: &SubPath,
    fill: &ResolvedFill,
    stroke: Option<&ResolvedStroke>,
    transform: Transform,
) {
    let Some(path) = build_path(sub) else { return };

    if sub.fill_mode != PathFillMode::None {
        if let Some(paint) = fill_paint(fill) {
            pixmap.fill_path(&path, &paint, FillRule::EvenOdd, transform, None);
        }
    }
    if sub.stroked
        && let Some(s) = stroke
        && s.color.a > 0.0
        && s.width.raw() > 0.0
    {
        let stroke = Stroke {
            width: s.width.raw(),
            line_cap: match s.cap {
                ResolvedLineCap::Butt => LineCap::Butt,
                ResolvedLineCap::Round => LineCap::Round,
                ResolvedLineCap::Square => LineCap::Square,
            },
            line_join: match s.join {
                ResolvedLineJoin::Round => LineJoin::Round,
                ResolvedLineJoin::Bevel => LineJoin::Bevel,
                ResolvedLineJoin::Miter => LineJoin::Miter,
            },
            dash: match &s.dash {
                ResolvedDashPattern::Solid => None,
                ResolvedDashPattern::Dashes(d) => {
                    let mut v: Vec<f32> = d.iter().map(|p| p.raw()).collect();
                    // tiny-skia requires an even-length dash array.
                    if v.len() % 2 == 1 {
                        v.extend_from_within(..);
                    }
                    StrokeDash::new(v, 0.0)
                }
            },
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &solid(rgba_color(s.color)), &stroke, transform, None);
    }
}

fn build_path(sub: &SubPath) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    // Arc cursor tracking: ArcTo starts at the current point without an
    // implicit move, so remember where the pen is.
    let mut cursor: Option<(f32, f32)> = None;
    for verb in &sub.verbs {
        match verb {
            PathVerb::MoveTo(p) => {
                pb.move_to(p.x.raw(), p.y.raw());
                cursor = Some((p.x.raw(), p.y.raw()));
            }
            PathVerb::LineTo(p) => {
                pb.line_to(p.x.raw(), p.y.raw());
                cursor = Some((p.x.raw(), p.y.raw()));
            }
            PathVerb::QuadTo(c, p) => {
                pb.quad_to(c.x.raw(), c.y.raw(), p.x.raw(), p.y.raw());
                cursor = Some((p.x.raw(), p.y.raw()));
            }
            PathVerb::CubicTo(c0, c1, p) => {
                pb.cubic_to(
                    c0.x.raw(),
                    c0.y.raw(),
                    c1.x.raw(),
                    c1.y.raw(),
                    p.x.raw(),
                    p.y.raw(),
                );
                cursor = Some((p.x.raw(), p.y.raw()));
            }
            PathVerb::ArcTo {
                radii,
                start_angle,
                swing_angle,
            } => {
                let Some((sx, sy)) = cursor else { continue };
                let rx = radii.width.raw();
                let ry = radii.height.raw();
                if rx <= 0.0 || ry <= 0.0 {
                    continue;
                }
                // OOXML angles: 0° = 3 o'clock, positive = clockwise — which
                // in y-down page space is the standard parametric direction,
                // so p(θ) = center + (rx cos θ, ry sin θ) needs no sign flip.
                let th0 = (start_angle.raw() as f32 / 60_000.0).to_radians();
                let swing = (swing_angle.raw() as f32 / 60_000.0).to_radians();
                let (cx, cy) = (sx - rx * th0.cos(), sy - ry * th0.sin());
                // Polyline approximation, ≤5° per segment: sub-pixel error at
                // raster scale for any radius that fits on a page.
                let steps = ((swing.abs().to_degrees() / 5.0).ceil() as usize).max(1);
                let mut end = (sx, sy);
                for i in 1..=steps {
                    let th = th0 + swing * (i as f32 / steps as f32);
                    end = (cx + rx * th.cos(), cy + ry * th.sin());
                    pb.line_to(end.0, end.1);
                }
                cursor = Some(end);
            }
            PathVerb::Close => {
                pb.close();
            }
        }
    }
    pb.finish()
}

/// Tier-0 fill: solid faithfully; gradient as the mean stop color (logged
/// once); blip/pattern skipped (logged once). Inventing pixels quietly is the
/// silent-corruption trap; a one-time log keeps the approximation loud.
fn fill_paint(fill: &ResolvedFill) -> Option<Paint<'static>> {
    static GRADIENT_ONCE: Once = Once::new();
    static TEXTURE_ONCE: Once = Once::new();
    match fill {
        ResolvedFill::None => None,
        ResolvedFill::Solid(c) => (c.a > 0.0).then(|| solid(rgba_color(*c))),
        ResolvedFill::Gradient(g) => {
            GRADIENT_ONCE.call_once(|| {
                log::info!("gradient fills approximate as their mean stop color");
            });
            if g.stops.is_empty() {
                return None;
            }
            let n = g.stops.len() as f32;
            let (mut r, mut gr, mut b, mut a) = (0.0, 0.0, 0.0, 0.0);
            for s in &g.stops {
                r += s.color.r;
                gr += s.color.g;
                b += s.color.b;
                a += s.color.a;
            }
            let c = Rgba {
                r: r / n,
                g: gr / n,
                b: b / n,
                a: a / n,
            };
            (c.a > 0.0).then(|| solid(rgba_color(c)))
        }
        ResolvedFill::Blip(_) | ResolvedFill::Pattern(_) => {
            TEXTURE_ONCE.call_once(|| {
                log::info!("blip/pattern fills are not rasterized; interiors render unfilled");
            });
            None
        }
    }
}

// ─── Small helpers ──────────────────────────────────────────────────────────

fn solid(color: Color) -> Paint<'static> {
    Paint {
        shader: Shader::SolidColor(color),
        anti_alias: true,
        ..Paint::default()
    }
}

fn rgb_color(c: RgbColor) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, 255)
}

fn rgba_color(c: Rgba) -> Color {
    Color::from_rgba(
        c.r.clamp(0.0, 1.0),
        c.g.clamp(0.0, 1.0),
        c.b.clamp(0.0, 1.0),
        c.a.clamp(0.0, 1.0),
    )
    .unwrap_or(Color::BLACK)
}

fn skia_rect(r: &PtRect) -> Option<tiny_skia::Rect> {
    tiny_skia::Rect::from_xywh(
        r.origin.x.raw(),
        r.origin.y.raw(),
        r.size.width.raw(),
        r.size.height.raw(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::dimension::Dimension;
    use crate::render::geometry::PtSize;
    use crate::render::layout::draw_command::ShapeTransform;

    /// A page with one black rect, wrapped in `bracket`.
    fn bracketed_rect(bracket: ShapeTransform, rect: PtRect) -> LayoutedPage {
        LayoutedPage {
            commands: vec![
                DrawCommand::Transform(TransformMark::Begin(bracket)),
                DrawCommand::Rect {
                    rect,
                    color: RgbColor { r: 0, g: 0, b: 0 },
                },
                DrawCommand::Transform(TransformMark::End),
            ],
            page_size: PtSize::new(Pt::new(100.0), Pt::new(100.0)),
            block_starts: Vec::new(),
        }
    }

    fn place(origin: (f32, f32), deg: f32, extent: (f32, f32)) -> ShapeTransform {
        ShapeTransform {
            origin: PtOffset::new(Pt::new(origin.0), Pt::new(origin.1)),
            rotation: Dimension::new((deg * 60_000.0) as i64),
            flip_h: false,
            flip_v: false,
            extent: PtSize::new(Pt::new(extent.0), Pt::new(extent.1)),
        }
    }

    fn rect(x: f32, y: f32, w: f32, h: f32) -> PtRect {
        PtRect::from_xywh(Pt::new(x), Pt::new(y), Pt::new(w), Pt::new(h))
    }

    /// Is the pixel at Pt `(x, y)` inked? Rasterized at 1 px/Pt, so the two
    /// coordinate systems coincide.
    fn inked(raster: &RasterPage, x: usize, y: usize) -> bool {
        let i = (y * raster.width as usize + x) * 4;
        raster.rgba[i] < 128
    }

    /// A bracketed command is in *shape-local* Pt: a rect at the local origin
    /// must land where the bracket puts it, and nothing must land at the page
    /// origin — which is exactly where a painter that ignored the bracket
    /// would draw it.
    #[test]
    fn a_bracket_places_the_commands_it_opens() {
        let page = bracketed_rect(
            place((50.0, 50.0), 0.0, (20.0, 20.0)),
            rect(0.0, 0.0, 10.0, 10.0),
        );
        let r = rasterize_page(&page, &FontRegistry::new(), 1.0).expect("raster");
        assert!(inked(&r, 55, 55), "the rect lands at the bracket's origin");
        assert!(!inked(&r, 5, 5), "and not at the page origin");
    }

    /// Rotation is about the shape's centre, not its origin — the same rule
    /// `DrawCommand::Path` states. A quarter turn takes the body's top strip
    /// to its right-hand one.
    #[test]
    fn a_bracket_rotates_about_the_shape_centre() {
        let page = bracketed_rect(
            place((50.0, 50.0), 90.0, (20.0, 20.0)),
            rect(0.0, 0.0, 20.0, 10.0),
        );
        let r = rasterize_page(&page, &FontRegistry::new(), 1.0).expect("raster");
        assert!(
            inked(&r, 65, 60),
            "top strip turned into the right-hand one"
        );
        assert!(!inked(&r, 55, 60), "the left half is now empty");
    }

    /// Every pass walks the whole command list, so bracket state has to be
    /// tracked in each of them: a rect (shading pass) and a path (shape pass)
    /// inside one bracket must both be placed by it.
    #[test]
    fn every_pass_honours_the_bracket() {
        let mut page = bracketed_rect(
            place((50.0, 50.0), 0.0, (20.0, 20.0)),
            rect(0.0, 0.0, 10.0, 10.0),
        );
        let square = SubPath {
            verbs: vec![
                PathVerb::MoveTo(PtOffset::new(Pt::new(0.0), Pt::new(10.0))),
                PathVerb::LineTo(PtOffset::new(Pt::new(10.0), Pt::new(10.0))),
                PathVerb::LineTo(PtOffset::new(Pt::new(10.0), Pt::new(20.0))),
                PathVerb::LineTo(PtOffset::new(Pt::new(0.0), Pt::new(20.0))),
                PathVerb::Close,
            ],
            fill_mode: PathFillMode::Norm,
            stroked: false,
        };
        page.commands.insert(
            2,
            DrawCommand::Path {
                origin: PtOffset::new(Pt::ZERO, Pt::ZERO),
                rotation: Dimension::new(0),
                flip_h: false,
                flip_v: false,
                extent: PtSize::new(Pt::new(20.0), Pt::new(20.0)),
                paths: vec![square],
                fill: ResolvedFill::Solid(Rgba::BLACK),
                stroke: None,
                effects: Vec::new(),
            },
        );
        let r = rasterize_page(&page, &FontRegistry::new(), 1.0).expect("raster");
        assert!(inked(&r, 55, 55), "shading pass placed by the bracket");
        assert!(inked(&r, 55, 65), "shape pass placed by the bracket too");
        assert!(!inked(&r, 5, 15), "neither drew at the page origin");
    }
}
