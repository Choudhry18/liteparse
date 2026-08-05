//! Draw commands — the output of layout, consumed by the painter.

use std::rc::Rc;

use crate::model::dimension::{Dimension, SixtieThousandthDeg};
use crate::render::dimension::Pt;
use crate::render::emoji::cluster::{EmojiPresentation, EmojiStructure};
use crate::render::fonts::TypefaceEntry;
use crate::render::geometry::{PtLineSegment, PtOffset, PtRect, PtSize};
use crate::render::resolve::color::RgbColor;
use crate::render::resolve::drawing_color::Rgba;
use crate::render::resolve::images::MediaEntry;
use crate::render::resolve::shape_geometry::SubPath;

/// §17.3.1.19: one heading, as it will appear in the PDF outline.
///
/// `node_id` is assigned at **layout** time and carried here rather than
/// recomputed at paint. The painter and the structure-tree builder both walk
/// the same command stream, and two counters that agree only by argument is
/// exactly the shape that produced this codebase's MCE double-render and then
/// the missing render after it. One field, read twice.
#[derive(Debug, Clone)]
pub struct OutlineHeading {
    /// PDF structure node ID. Unique per document and **1-based**:
    /// `skia_safe::pdf::node_id` reserves 0 for "no node" and every negative
    /// value for artifacts.
    pub node_id: i32,
    /// §17.3.1.19 level, 1-based (`w:outlineLvl` 0 → 1).
    pub level: crate::model::OutlineLevel,
    /// The paragraph's own text, **without** its §17.9.22 list label — which
    /// is what Word and LibreOffice both put in the outline, even for a
    /// numbered heading that renders with its number on the page.
    pub title: Rc<str>,
}

/// §17.3.1.19: the boundaries of a heading's marked content.
///
/// A *pair* rather than a single marker because the PDF destination is the
/// union of the marks under the node — leaving the node ID set past the
/// heading would drag its destination down onto the body text below. This is
/// the same shape as the `BDC`/`EMC` operator pair it ultimately becomes.
///
/// The balance invariant is held by a single emitter
/// (`paragraph::line_emit::emit_line_commands`) rather than by the type: the
/// alternative — a variant owning a `Vec<DrawCommand>` — would need an arm in
/// every one of the twenty files that match on `DrawCommand`, for a nesting
/// the flat per-page command list does not otherwise have.
#[derive(Debug, Clone)]
pub enum OutlineMark {
    /// Opens the heading's marked content.
    Begin(OutlineHeading),
    /// Closes it, restoring "no node".
    End,
}

/// A positioned drawing command — absolute page coordinates.
#[derive(Debug, Clone)]
pub enum DrawCommand {
    Text {
        position: PtOffset,
        text: Rc<str>,
        font_family: Rc<str>,
        char_spacing: Pt,
        font_size: Pt,
        bold: bool,
        italic: bool,
        color: RgbColor,
        /// §17.3.2.45: horizontal scale factor (1.0 = normal, 0.8 = 80%,
        /// 1.5 = 150%). Painter applies via `Font::set_scale_x`.
        text_scale: f32,
    },
    Underline {
        line: PtLineSegment,
        color: RgbColor,
        width: Pt,
    },
    Line {
        line: PtLineSegment,
        color: RgbColor,
        width: Pt,
    },
    Image {
        rect: PtRect,
        image_data: MediaEntry,
        /// §20.1.10.48 `a:srcRect` — fractional source crop in `[0, 1]`
        /// relative to the image's natural extent, applied before the image
        /// is stretched into `rect`. `None` draws the whole image.
        src_rect: Option<PtRect>,
    },
    /// One emoji grapheme cluster placed at `rect`. The painter rasterizes
    /// the cluster against `typeface` (Skia raster backend honours the color
    /// glyph tables that the PDF backend strips) and embeds the result as
    /// an inline image. Produced from [`Fragment::Emoji`]; painted by the
    /// `EmojiCluster` arm in `painter.rs`.
    ///
    /// [`Fragment::Emoji`]: crate::render::layout::fragment::Fragment::Emoji
    EmojiCluster {
        /// Page-coordinate rectangle at which to draw the rasterized image.
        rect: PtRect,
        /// Cluster text — one grapheme cluster, possibly multi-codepoint.
        text: String,
        /// Color emoji typeface to rasterize against.
        typeface: TypefaceEntry,
        /// Font size in Pt at which to rasterize.
        size: Pt,
        /// UTS #51 cluster classification (carried for future paint-side
        /// behaviour and diagnostics; not needed by the raster pipeline).
        presentation: EmojiPresentation,
        structure: EmojiStructure,
    },
    Rect {
        rect: PtRect,
        color: RgbColor,
    },
    /// One annotation rect per word of a `w:hyperlink`, so the target is
    /// shared with the fragment it came from (see
    /// [`LinkTarget`](crate::render::layout::fragment::LinkTarget)) rather
    /// than copied per word.
    LinkAnnotation {
        rect: PtRect,
        url: Rc<str>,
    },
    /// Internal link to a named destination (bookmark). Shared for the same
    /// reason as [`DrawCommand::LinkAnnotation`]'s `url`.
    InternalLink {
        rect: PtRect,
        destination: Rc<str>,
    },
    /// Named destination marker (bookmark target).
    NamedDestination {
        position: PtOffset,
        name: String,
    },
    /// §17.3.1.19: brackets one heading paragraph's commands so the PDF
    /// outline can point at it. Draws nothing.
    Outline(OutlineMark),
    /// §20.1.8 shape draw command — a resolved geometry with fill, stroke,
    /// and optional effects. `paths` are in shape-local Pt; the painter
    /// applies the placement transform (origin/rotation/flip).
    Path {
        /// Top-left placement anchor in page coordinates.
        origin: PtOffset,
        /// Rotation in 60000ths of a degree, around the shape's center.
        rotation: Dimension<SixtieThousandthDeg>,
        /// Mirror the shape horizontally before placement.
        flip_h: bool,
        /// Mirror the shape vertically before placement.
        flip_v: bool,
        /// Shape-local size — the bounding box the geometry was built for.
        extent: PtSize,
        /// One or more subpaths; each may be stroked and/or filled.
        paths: Vec<SubPath>,
        /// Fill applied to path interiors.
        fill: ResolvedFill,
        /// Stroke applied to path outlines (if stroked at the path level).
        stroke: Option<ResolvedStroke>,
        /// Post-processing effects (shadow, glow, …). Painter applies in order.
        effects: Vec<ResolvedEffect>,
    },
}

// ── Resolved render-ready types ─────────────────────────────────────────────

/// Painter-ready fill specification. Tier 0 renders `None` and `Solid`
/// faithfully; the other variants fall through with a one-time log.
#[derive(Clone, Debug)]
pub enum ResolvedFill {
    /// No fill — path interior is transparent.
    None,
    /// §20.1.8.54 solidFill — a single RGBA color.
    Solid(Rgba),
    /// §20.1.8.33 gradFill — stops and a direction. Tier 2 renders.
    Gradient(ResolvedGradient),
    /// §20.1.8.14 blipFill — image fill. Tier 2 renders.
    Blip(ResolvedBlip),
    /// §20.1.8.47 pattFill — preset pattern with fg/bg. Tier 3 renders.
    Pattern(ResolvedPattern),
}

/// Painter-ready stroke specification. Color/width are always honoured;
/// dash pattern is applied as a Skia path effect; cap/join map to Skia's
/// stroke primitives.
#[derive(Clone, Debug)]
pub struct ResolvedStroke {
    pub width: Pt,
    pub color: Rgba,
    pub dash: ResolvedDashPattern,
    pub cap: ResolvedLineCap,
    pub join: ResolvedLineJoin,
}

/// §20.1.10.31 ST_LineCap, ready for Skia.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolvedLineCap {
    Butt,
    Round,
    Square,
}

/// §20.1.2.2.22 EG_LineJoinProperties, ready for Skia.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResolvedLineJoin {
    Round,
    Bevel,
    Miter,
}

/// Stroke dash pattern. `Solid` renders without a dash effect; `Dashes`
/// alternates on/off lengths in Pt.
#[derive(Clone, Debug, PartialEq)]
pub enum ResolvedDashPattern {
    Solid,
    Dashes(Vec<Pt>),
}

/// Post-processing effects. Tier 0 carries the data only — the painter
/// renders none of these until later tiers.
#[derive(Clone, Debug)]
pub enum ResolvedEffect {
    /// §20.1.8.45 outerShdw.
    OuterShadow {
        blur_radius: Pt,
        offset: PtOffset,
        color: Rgba,
    },
}

/// Painter-ready gradient fill — included for typing completeness; Tier 0
/// painter logs and draws no fill. Tier 2 adds rendering.
#[derive(Clone, Debug)]
pub struct ResolvedGradient {
    pub stops: Vec<GradientStopRgba>,
    pub kind: ResolvedGradientKind,
}

#[derive(Clone, Copy, Debug)]
pub struct GradientStopRgba {
    /// Position in `[0, 1]`.
    pub position: f32,
    pub color: Rgba,
}

#[derive(Clone, Copy, Debug)]
pub enum ResolvedGradientKind {
    /// Linear gradient at angle (degrees, OOXML convention: 0° = horizontal,
    /// clockwise positive).
    Linear { angle_deg: f32 },
    /// Radial/path-based gradient.
    Radial,
}

/// Painter-ready image fill — pointer to decoded bytes + source crop.
#[derive(Clone, Debug)]
pub struct ResolvedBlip {
    pub data: std::sync::Arc<[u8]>,
    pub format: crate::model::ImageFormat,
    /// Fraction of the source to crop: values in `[0, 1]` relative to the
    /// blip's natural extent. Derive this from the blip's `a:srcRect` via
    /// `resolve::images::relative_rect_to_fraction` — the same converter the
    /// picture path uses — so cropped fills and cropped pictures stay in
    /// lockstep.
    pub src_rect: Option<PtRect>,
}

/// Painter-ready pattern fill — foreground/background + preset id.
#[derive(Clone, Debug)]
pub struct ResolvedPattern {
    pub preset: crate::model::PresetPatternVal,
    pub fg: Rgba,
    pub bg: Rgba,
}

impl DrawCommand {
    /// Shift all coordinates by `(dx, dy)`.
    pub fn shift(&mut self, dx: Pt, dy: Pt) {
        match self {
            DrawCommand::Text { position, .. } => {
                position.x += dx;
                position.y += dy;
            }
            DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
                line.start.x += dx;
                line.start.y += dy;
                line.end.x += dx;
                line.end.y += dy;
            }
            DrawCommand::Image { rect, .. }
            | DrawCommand::EmojiCluster { rect, .. }
            | DrawCommand::Rect { rect, .. }
            | DrawCommand::LinkAnnotation { rect, .. }
            | DrawCommand::InternalLink { rect, .. } => {
                rect.origin.x += dx;
                rect.origin.y += dy;
            }
            DrawCommand::NamedDestination { position, .. } => {
                position.x += dx;
                position.y += dy;
            }
            DrawCommand::Path { origin, .. } => {
                origin.x += dx;
                origin.y += dy;
            }
            // A marked-content boundary has no coordinates of its own — the
            // destination is derived from the commands it brackets, which do
            // get shifted.
            DrawCommand::Outline(_) => {}
        }
    }

    /// The vertical band this command paints into, as `(top, bottom)`.
    ///
    /// `None` for commands that paint nothing — a link annotation covers
    /// content that is already accounted for by the command underneath it, and
    /// a named destination is a point.
    ///
    /// **Text is approximate.** A `Text` command carries a baseline and a font
    /// size, not the ascent/descent it was measured with, so the band is taken
    /// as `baseline ± font_size`. That is the same approximation
    /// `render::estimate_cursor_y` already makes for the descender, kept
    /// deliberately identical so the two cannot disagree about where a line
    /// ends. It is generous on both sides, which is the safe direction for the
    /// one caller that exists (`vertOverflow="clip"`): a line is dropped when
    /// it *might* paint outside its box rather than when it certainly does.
    pub fn vertical_span(&self) -> Option<(Pt, Pt)> {
        match self {
            DrawCommand::Text {
                position,
                font_size,
                ..
            } => Some((position.y - *font_size, position.y + *font_size)),
            DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
                Some((line.start.y.min(line.end.y), line.start.y.max(line.end.y)))
            }
            DrawCommand::Image { rect, .. }
            | DrawCommand::EmojiCluster { rect, .. }
            | DrawCommand::Rect { rect, .. } => {
                Some((rect.origin.y, rect.origin.y + rect.size.height))
            }
            // `extent` is the shape's unrotated bounding box; a rotation can
            // reach slightly past it, which is within this function's remit.
            DrawCommand::Path { origin, extent, .. } => Some((origin.y, origin.y + extent.height)),
            DrawCommand::LinkAnnotation { .. }
            | DrawCommand::InternalLink { .. }
            | DrawCommand::NamedDestination { .. }
            | DrawCommand::Outline(_) => None,
        }
    }

    /// Shift all y-coordinates by `dy`.
    pub fn shift_y(&mut self, dy: Pt) {
        self.shift(Pt::ZERO, dy);
    }

    /// Shift all x-coordinates by `dx`.
    pub fn shift_x(&mut self, dx: Pt) {
        self.shift(dx, Pt::ZERO);
    }
}

/// A fully laid-out page — ready for painting.
#[derive(Debug, Clone)]
pub struct LayoutedPage {
    pub commands: Vec<DrawCommand>,
    pub page_size: PtSize,
}

impl LayoutedPage {
    pub fn new(page_size: PtSize) -> Self {
        Self {
            commands: Vec::new(),
            page_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shift_y_moves_text() {
        let mut cmd = DrawCommand::Text {
            position: PtOffset::new(Pt::new(10.0), Pt::new(20.0)),
            text: "hi".into(),
            font_family: Rc::from("Arial"),
            char_spacing: Pt::ZERO,
            font_size: Pt::new(12.0),
            bold: false,
            italic: false,
            color: RgbColor::BLACK,
            text_scale: 1.0,
        };
        cmd.shift_y(Pt::new(5.0));
        if let DrawCommand::Text { position, .. } = cmd {
            assert_eq!(position.y.raw(), 25.0);
            assert_eq!(position.x.raw(), 10.0); // x unchanged
        }
    }

    #[test]
    fn shift_y_moves_line() {
        let mut cmd = DrawCommand::Line {
            line: PtLineSegment::new(
                PtOffset::new(Pt::new(0.0), Pt::new(10.0)),
                PtOffset::new(Pt::new(100.0), Pt::new(10.0)),
            ),
            color: RgbColor::BLACK,
            width: Pt::new(1.0),
        };
        cmd.shift_y(Pt::new(50.0));
        if let DrawCommand::Line { line, .. } = cmd {
            assert_eq!(line.start.y.raw(), 60.0, "Line y shifted");
        }
    }

    #[test]
    fn shift_y_moves_rect() {
        let mut cmd = DrawCommand::Rect {
            rect: PtRect::from_xywh(Pt::new(0.0), Pt::new(10.0), Pt::new(50.0), Pt::new(20.0)),
            color: RgbColor::BLACK,
        };
        cmd.shift_y(Pt::new(100.0));
        if let DrawCommand::Rect { rect, .. } = cmd {
            assert_eq!(rect.origin.y.raw(), 110.0);
        }
    }

    #[test]
    fn layouted_page_new() {
        let page = LayoutedPage::new(PtSize::new(Pt::new(612.0), Pt::new(792.0)));
        assert!(page.commands.is_empty());
        assert_eq!(page.page_size.width.raw(), 612.0);
    }

    #[test]
    fn shift_y_moves_path_origin() {
        use crate::model::dimension::Dimension;

        let mut cmd = DrawCommand::Path {
            origin: PtOffset::new(Pt::new(10.0), Pt::new(20.0)),
            rotation: Dimension::new(0),
            flip_h: false,
            flip_v: false,
            extent: PtSize::new(Pt::new(100.0), Pt::new(50.0)),
            paths: vec![],
            fill: ResolvedFill::None,
            stroke: None,
            effects: vec![],
        };
        cmd.shift_y(Pt::new(7.5));
        if let DrawCommand::Path { origin, .. } = cmd {
            assert_eq!(origin.y.raw(), 27.5);
            assert_eq!(origin.x.raw(), 10.0);
        } else {
            panic!();
        }
    }

    // ── `shift` over every variant ───────────────────────────────────────
    //
    // The match in `shift` is exhaustive, so a *new* variant is a compile
    // error — that half is free. What exhaustiveness cannot check is whether
    // each arm moves the right field, which is what these cover. Six variants
    // (`Underline`, `Image`, `EmojiCluster`, `LinkAnnotation`, `InternalLink`,
    // `NamedDestination`) previously had no test at all, and `shift_x` had
    // none on any variant.

    const DX: f32 = 3.0;
    const DY: f32 = 7.0;

    fn origin_of(cmd: &DrawCommand) -> (f32, f32) {
        match cmd {
            DrawCommand::Text { position, .. } | DrawCommand::NamedDestination { position, .. } => {
                (position.x.raw(), position.y.raw())
            }
            DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => {
                (line.start.x.raw(), line.start.y.raw())
            }
            DrawCommand::Image { rect, .. }
            | DrawCommand::EmojiCluster { rect, .. }
            | DrawCommand::Rect { rect, .. }
            | DrawCommand::LinkAnnotation { rect, .. }
            | DrawCommand::InternalLink { rect, .. } => (rect.origin.x.raw(), rect.origin.y.raw()),
            DrawCommand::Path { origin, .. } => (origin.x.raw(), origin.y.raw()),
            // §17.3.1.19: a marked-content boundary has no coordinates, so
            // there is nothing for `shift` to move. `shift_leaves_a_marker_be`
            // asserts that directly rather than through this helper.
            DrawCommand::Outline(_) => unreachable!("markers are tested separately"),
        }
    }

    /// The only variant `one_of_each` cannot carry, because it has no origin to
    /// compare. Shifting it must leave it alone — the destination its outline
    /// entry gets is derived from the commands it brackets, which *are* shifted.
    #[test]
    fn shift_leaves_a_marker_be() {
        let heading = OutlineHeading {
            node_id: 7,
            level: crate::model::OutlineLevel::new(1),
            title: Rc::from("Title"),
        };
        for mut cmd in [
            DrawCommand::Outline(OutlineMark::Begin(heading)),
            DrawCommand::Outline(OutlineMark::End),
        ] {
            let before = format!("{cmd:?}");
            cmd.shift(Pt::new(DX), Pt::new(DY));
            assert_eq!(format!("{cmd:?}"), before);
            assert_eq!(cmd.vertical_span(), None, "a marker occupies no band");
        }
    }

    /// A sample of each variant, all anchored at (10, 20).
    fn one_of_each() -> Vec<(&'static str, DrawCommand)> {
        let at = |x: f32, y: f32| PtOffset::new(Pt::new(x), Pt::new(y));
        let rect = PtRect::from_xywh(Pt::new(10.0), Pt::new(20.0), Pt::new(4.0), Pt::new(4.0));
        let seg = PtLineSegment {
            start: at(10.0, 20.0),
            end: at(30.0, 20.0),
        };
        vec![
            (
                "Text",
                DrawCommand::Text {
                    position: at(10.0, 20.0),
                    text: "hi".into(),
                    font_family: Rc::from("Arial"),
                    char_spacing: Pt::ZERO,
                    font_size: Pt::new(12.0),
                    bold: false,
                    italic: false,
                    color: RgbColor::BLACK,
                    text_scale: 1.0,
                },
            ),
            (
                "Underline",
                DrawCommand::Underline {
                    line: seg,
                    color: RgbColor::BLACK,
                    width: Pt::new(1.0),
                },
            ),
            (
                "Line",
                DrawCommand::Line {
                    line: seg,
                    color: RgbColor::BLACK,
                    width: Pt::new(1.0),
                },
            ),
            (
                "Rect",
                DrawCommand::Rect {
                    rect,
                    color: RgbColor::BLACK,
                },
            ),
            (
                "Image",
                DrawCommand::Image {
                    rect,
                    image_data: crate::render::resolve::images::MediaEntry {
                        data: std::sync::Arc::from(&[0u8][..]),
                        format: crate::model::ImageFormat::Png,
                    },
                    src_rect: None,
                },
            ),
            (
                "LinkAnnotation",
                DrawCommand::LinkAnnotation {
                    rect,
                    url: "https://example.invalid".into(),
                },
            ),
            (
                "InternalLink",
                DrawCommand::InternalLink {
                    rect,
                    destination: "bm".into(),
                },
            ),
            (
                "NamedDestination",
                DrawCommand::NamedDestination {
                    position: at(10.0, 20.0),
                    name: "bm".into(),
                },
            ),
            (
                "Path",
                DrawCommand::Path {
                    origin: at(10.0, 20.0),
                    rotation: Dimension::new(0),
                    flip_h: false,
                    flip_v: false,
                    extent: PtSize::new(Pt::new(4.0), Pt::new(4.0)),
                    paths: Vec::new(),
                    fill: ResolvedFill::None,
                    stroke: None,
                    effects: Vec::new(),
                },
            ),
        ]
    }

    // ── `vertical_span` ──────────────────────────────────────────────────

    /// Every painting variant reports a band, and it must track the command:
    /// `one_of_each` places them all at y = 20, so shifting down by `DY` moves
    /// each band by exactly `DY`. Catches an arm that reads the wrong field.
    #[test]
    fn vertical_span_follows_every_painting_variant() {
        for (name, cmd) in one_of_each() {
            let Some((top, bottom)) = cmd.vertical_span() else {
                continue;
            };
            let mut moved = cmd;
            moved.shift_y(Pt::new(DY));
            let (top2, bottom2) = moved.vertical_span().expect("still paints");
            assert_eq!(top2.raw() - top.raw(), DY, "{name} top");
            assert_eq!(bottom2.raw() - bottom.raw(), DY, "{name} bottom");
            assert!(top.raw() <= bottom.raw(), "{name} band is inverted");
        }
    }

    /// The three annotation variants paint nothing of their own, so they have
    /// no band — a clip must not drop a link whose rect happens to hang below
    /// a box while the text it annotates is kept.
    #[test]
    fn annotations_have_no_vertical_span() {
        for (name, cmd) in one_of_each() {
            let expected_none = matches!(
                cmd,
                DrawCommand::LinkAnnotation { .. }
                    | DrawCommand::InternalLink { .. }
                    | DrawCommand::NamedDestination { .. }
            );
            assert_eq!(
                cmd.vertical_span().is_none(),
                expected_none,
                "{name} disagrees about whether it paints",
            );
        }
    }

    /// A `Text` command carries a baseline, not a box: the band straddles it.
    #[test]
    fn a_text_span_straddles_its_baseline() {
        let cmd = DrawCommand::Text {
            position: PtOffset::new(Pt::new(10.0), Pt::new(20.0)),
            text: Rc::from("x"),
            font_family: Rc::from("Arial"),
            char_spacing: Pt::ZERO,
            font_size: Pt::new(12.0),
            bold: false,
            italic: false,
            color: RgbColor::BLACK,
            text_scale: 1.0,
        };
        let (top, bottom) = cmd.vertical_span().unwrap();
        assert_eq!((top.raw(), bottom.raw()), (8.0, 32.0));
    }

    #[test]
    fn shift_moves_every_variant_on_both_axes() {
        for (name, mut cmd) in one_of_each() {
            cmd.shift(Pt::new(DX), Pt::new(DY));
            assert_eq!(origin_of(&cmd), (10.0 + DX, 20.0 + DY), "{name}");
        }
    }

    #[test]
    fn shift_x_and_shift_y_move_one_axis_each() {
        for (name, mut cmd) in one_of_each() {
            cmd.shift_x(Pt::new(DX));
            assert_eq!(origin_of(&cmd), (10.0 + DX, 20.0), "{name} shift_x");
        }
        for (name, mut cmd) in one_of_each() {
            cmd.shift_y(Pt::new(DY));
            assert_eq!(origin_of(&cmd), (10.0, 20.0 + DY), "{name} shift_y");
        }
    }

    /// A line has two endpoints; shifting must move both, or the segment
    /// changes length instead of position.
    #[test]
    fn shift_moves_both_ends_of_a_segment() {
        let mut cmd = DrawCommand::Line {
            line: PtLineSegment {
                start: PtOffset::new(Pt::new(10.0), Pt::new(20.0)),
                end: PtOffset::new(Pt::new(30.0), Pt::new(20.0)),
            },
            color: RgbColor::BLACK,
            width: Pt::new(1.0),
        };
        cmd.shift(Pt::new(DX), Pt::new(DY));
        let DrawCommand::Line { line, .. } = cmd else {
            unreachable!()
        };
        assert_eq!((line.start.x.raw(), line.start.y.raw()), (13.0, 27.0));
        assert_eq!((line.end.x.raw(), line.end.y.raw()), (33.0, 27.0));
    }

    /// `Path` carries its geometry in shape-local coordinates, so `shift` must
    /// move only the placement origin — moving the subpaths as well would
    /// double-apply the offset.
    #[test]
    fn shift_leaves_path_extent_and_geometry_alone() {
        let mut cmd = DrawCommand::Path {
            origin: PtOffset::new(Pt::new(10.0), Pt::new(20.0)),
            rotation: Dimension::new(0),
            flip_h: false,
            flip_v: false,
            extent: PtSize::new(Pt::new(4.0), Pt::new(6.0)),
            paths: Vec::new(),
            fill: ResolvedFill::None,
            stroke: None,
            effects: Vec::new(),
        };
        cmd.shift(Pt::new(DX), Pt::new(DY));
        let DrawCommand::Path { extent, .. } = cmd else {
            unreachable!()
        };
        assert_eq!((extent.width.raw(), extent.height.raw()), (4.0, 6.0));
    }
}
