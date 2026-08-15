//! Fragment conversion — transform Inline content into measured Fragments
//! for the line-fitting algorithm.

use std::rc::Rc;

use crate::model::{PTabAlignment, PTabRelativeTo, RunProperties, TabLeader, UnderlineStyle};

use crate::render::dimension::Pt;
use crate::render::emoji::cluster::{EmojiPresentation, EmojiStructure};
use crate::render::fonts::TypefaceEntry;
use crate::render::geometry::{PtRect, PtSize};
use crate::render::resolve::color::RgbColor;
use crate::render::resolve::fonts::effective_font;
use crate::render::resolve::images::MediaEntry;

mod collect;
mod segment;
mod split;
mod text;

pub use collect::{
    FieldContext, FootnoteTracker, FragmentCtx, RecordedFootnote, collect_fragments,
};
pub use split::split_oversized_fragments;

// ── Superscript / subscript rendering constants ───────────────────────────────
// §17.3.2.42: these ratios are "application-defined" per the spec; the values
// below match Word's rendering as documented in the OpenXML SDK reference.

/// Font size of super/subscript text as a fraction of the base font size.
/// Also the size of a note reference mark and its body number (§17.11.12).
pub(crate) const SUPERSCRIPT_FONT_SIZE_RATIO: f32 = 0.58;

/// Superscript baseline shift: fraction of base ascent to raise the text by.
pub(super) const SUPERSCRIPT_ASCENT_OFFSET_RATIO: f32 = 0.33;

/// Subscript baseline shift: fraction of base character height to lower the text by.
pub(super) const SUBSCRIPT_HEIGHT_OFFSET_RATIO: f32 = 0.08;

/// §17.11.12: baseline shift for a footnote/endnote reference mark, and for
/// the matching number prefixed to the note body — as a fraction of the base
/// **font size**.
///
/// Deliberately *not* [`SUPERSCRIPT_ASCENT_OFFSET_RATIO`]: that one is a
/// fraction of the measured *ascent* and carries a different value (0.33).
/// Note marks are raised relative to the font size so the mark and its body
/// number line up without a measurement round-trip.
pub(crate) const NOTE_REF_BASELINE_OFFSET_RATIO: f32 = 0.4;

/// Font properties needed for rendering a text fragment.
#[derive(Clone, Debug)]
pub struct FontProps {
    pub family: Rc<str>,
    pub size: Pt,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub char_spacing: Pt,
    /// §17.3.2.45: horizontal character scale as a multiplier (1.0 = normal,
    /// 0.8 = 80%, 1.5 = 150%). Applied to glyph advances during measure and
    /// to the Skia font's `scale_x` during paint. Inter-character spacing
    /// (`char_spacing`) is **not** scaled by this — the spec keeps the two
    /// independent.
    pub text_scale: f32,
    /// Underline position from font metrics (positive = below baseline).
    pub underline_position: Pt,
    /// Underline thickness from font metrics.
    pub underline_thickness: Pt,
}

/// Font metrics for a specific font at a specific size.
/// Evaluated once by the measurer and carried through the pipeline.
#[derive(Clone, Copy, Debug)]
pub struct TextMetrics {
    /// Distance from baseline to top of glyphs (positive upward).
    pub ascent: Pt,
    /// Distance from baseline to bottom of glyphs (positive downward).
    pub descent: Pt,
    /// §17.3.1.33: inter-line leading from the font's metrics.
    /// Included in Auto line spacing base but not in glyph height.
    pub leading: Pt,
}

impl TextMetrics {
    /// Glyph height (ascent + descent) — used for baseline positioning.
    pub fn height(&self) -> Pt {
        self.ascent + self.descent
    }

    /// §17.3.1.33: full line height including leading — the base unit
    /// that Auto line spacing multipliers scale.
    pub fn line_height(&self) -> Pt {
        self.ascent + self.descent + self.leading
    }
}

/// §17.3.2.4: run-level border for rendering.
#[derive(Clone, Copy, Debug)]
pub struct FragmentBorder {
    pub width: Pt,
    pub color: RgbColor,
    pub space: Pt,
}

/// The target of a hyperlink carried on a text fragment. Keeps the
/// §17.16.22 external-vs-internal distinction (from `HyperlinkTarget`) as a
/// closed ADT so the emitter routes each to the right PDF annotation
/// (external → URI action, internal → GoTo a named destination) instead of
/// re-deriving it from a URL-scheme string check.
///
/// The string is shared, for the same reason [`Fragment::Text`]'s `text` and
/// `font` are: a `w:hyperlink` fragments into one `Fragment::Text` per *word*,
/// each of which then emits its own annotation command. Owning the target
/// would copy the URL once per word and again per command; sharing it copies
/// once per link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkTarget {
    /// A resolved external URI (`http:`, `mailto:`, `file:`, …).
    External(Rc<str>),
    /// An internal bookmark name (`w:hyperlink/@w:anchor`).
    Internal(Rc<str>),
}

/// A measured fragment — the atomic unit for line fitting.
#[derive(Clone, Debug)]
pub enum Fragment {
    Text {
        text: Rc<str>,
        /// Shared per run: all words of a run carry the same font properties,
        /// so an `Rc` keeps the `Fragment::Text` variant small (a pointer, not
        /// an embedded ~48-byte `FontProps`) and makes per-word clones a
        /// refcount bump.
        font: Rc<FontProps>,
        color: RgbColor,
        /// §17.3.2.32: run-level shading (background color behind text).
        shading: Option<RgbColor>,
        /// §17.3.2.4: run-level border (box around text).
        border: Option<FragmentBorder>,
        /// Full width including trailing whitespace (used for positioning).
        width: Pt,
        /// Width excluding trailing whitespace (used for line-break overflow checking).
        /// Trailing whitespace is allowed to hang past the margin per Word behavior.
        trimmed_width: Pt,
        /// Font metrics (ascent + descent = text height).
        metrics: TextMetrics,
        /// Hyperlink target (external URI or internal bookmark), if this
        /// fragment is inside a `w:hyperlink`. Named `hyperlink_url` for
        /// historical reasons; carries the external/internal kind, not a bare
        /// URL, so the emitter never has to guess from the string.
        hyperlink_url: Option<LinkTarget>,
        baseline_offset: Pt,
        /// Horizontal offset for drawing text within the fragment width.
        /// Used for right/center-justified list labels where the text is
        /// positioned within a wider fragment. Default: Pt::ZERO.
        text_offset: Pt,
        /// §17.11.12: true if this is a footnote reference mark (the superscript
        /// number). Rendered as ordinary text, but tagged so across-page
        /// splitting can reserve each footnote on the page its mark lands on.
        is_footnote_ref: bool,
    },
    Image {
        size: PtSize,
        rel_id: String,
        image_data: Option<MediaEntry>,
        /// §20.1.10.48 `a:srcRect` — fractional source crop in `[0, 1]`.
        src_rect: Option<PtRect>,
    },
    /// One emoji grapheme cluster (UAX #29) classified as an emoji sequence
    /// (UTS #51), to be rasterized at paint time via Skia's raster backend
    /// and embedded as an inline PDF image, because Skia's PDF backend strips
    /// the color glyph tables its raster backend honours. Classified by
    /// `render::emoji::cluster`; becomes a `DrawCommand::EmojiCluster`.
    Emoji {
        /// Cluster text exactly as classified — one grapheme cluster, possibly
        /// multi-codepoint (ZWJ, modifier, RIS, tag, keycap sequences).
        text: String,
        /// Color emoji typeface resolved upstream by the emoji resolver.
        /// Frozen at fragment build so paint never re-resolves.
        typeface: TypefaceEntry,
        /// Font size at which to rasterize, in Pt.
        size: Pt,
        /// UTS #51 §2 presentation. `EmojiPresentation::Text` is preserved
        /// (the rasterizer can still render it via the same color path) but
        /// allows future paint-side decisions (e.g. monochrome over color).
        presentation: EmojiPresentation,
        /// UTS #51 §2 cluster structure. Carried for diagnostics + future
        /// painter behaviour (skin-tone modifier substitution, etc.).
        structure: EmojiStructure,
        /// Measured advance from Skia raster metrics at `size`.
        advance: Pt,
        /// Font metrics from the resolved emoji typeface. Drives the
        /// rasterized image's natural aspect ratio and the rect's vertical
        /// extent in `line_emit::emit_line_commands` — NOT the line-height
        /// contribution. Color emoji typefaces (Apple Color Emoji, Segoe UI
        /// Emoji) carry tall ascents (≈1.25× font size) so their glyph art
        /// fits, but bumping running-text line height by that amount makes
        /// emoji-mixed lines visibly taller than text-only lines.
        metrics: TextMetrics,
        /// Metrics for line-height contribution, derived from the run's
        /// font.size against the run-level typeface (not the emoji
        /// typeface). Keeps the inline emoji "1em-tall" semantics so a
        /// paragraph that mixes emoji and plain text lays out evenly.
        /// The rasterized image still draws at its natural extent and may
        /// overhang the line slightly.
        line_metrics: TextMetrics,
        /// Inherited from the run (super/subscript / `w:position`).
        baseline_offset: Pt,
    },
    Tab {
        line_height: Pt,
        /// §17.3.1.38: formatting of the run holding the `<w:tab/>`. A tab
        /// leader carries no formatting of its own — it is drawn in the
        /// formatting in effect at the tab — so the leader emitter reads its
        /// family and size from here rather than substituting a default.
        font: Rc<FontProps>,
        /// §17.3.1.38: text colour of the tab's run, for the same reason.
        color: RgbColor,
        /// Override minimum width for line fitting (default: MIN_TAB_WIDTH).
        fitting_width: Option<Pt>,
    },
    /// §17.3.1.30: absolute-position tab. Its resolved position depends on the
    /// line's geometry (margins / indents) and following content, so — like
    /// [`Fragment::Tab`] — it occupies only a nominal width during line
    /// fitting and is placed during line emission.
    PTab {
        align: PTabAlignment,
        relative_to: PTabRelativeTo,
        leader: TabLeader,
        line_height: Pt,
        /// §17.3.1.38: formatting of the run holding the `<w:ptab/>` — the
        /// leader is drawn in it. See [`Fragment::Tab`].
        font: Rc<FontProps>,
        /// §17.3.1.38: text colour of the ptab's run.
        color: RgbColor,
    },
    LineBreak {
        line_height: Pt,
    },
    /// §17.3.3.1: column break — forces content to the next column.
    ColumnBreak,
    /// §17.3.3.1: page break — forces content to the next page.
    PageBreak {
        line_height: Pt,
    },
    /// Named destination (bookmark target) — zero-width marker.
    Bookmark {
        name: String,
    },
}

impl Fragment {
    pub fn width(&self) -> Pt {
        match self {
            Fragment::Text { width, .. } => *width,
            Fragment::Image { size, .. } => size.width,
            Fragment::Emoji { advance, .. } => *advance,
            Fragment::Tab { fitting_width, .. } => fitting_width.unwrap_or(MIN_TAB_WIDTH),
            Fragment::PTab { .. } => MIN_TAB_WIDTH,
            Fragment::LineBreak { .. }
            | Fragment::ColumnBreak
            | Fragment::PageBreak { .. }
            | Fragment::Bookmark { .. } => Pt::ZERO,
        }
    }

    /// Width for overflow checking — excludes trailing whitespace on text fragments.
    pub fn trimmed_width(&self) -> Pt {
        match self {
            Fragment::Text { trimmed_width, .. } => *trimmed_width,
            other => other.width(),
        }
    }

    pub fn height(&self) -> Pt {
        match self {
            Fragment::Text { metrics, .. } => metrics.height(),
            Fragment::Image { size, .. } => size.height,
            Fragment::Emoji { line_metrics, .. } => line_metrics.height(),
            Fragment::Tab { line_height, .. }
            | Fragment::PTab { line_height, .. }
            | Fragment::LineBreak { line_height }
            | Fragment::PageBreak { line_height } => *line_height,
            Fragment::ColumnBreak | Fragment::Bookmark { .. } => Pt::ZERO,
        }
    }

    pub fn is_line_break(&self) -> bool {
        matches!(
            self,
            Fragment::LineBreak { .. } | Fragment::ColumnBreak | Fragment::PageBreak { .. }
        )
    }

    /// §17.3.3.1: true if this fragment is a page break that forces
    /// subsequent content to the next page.
    pub fn is_page_break(&self) -> bool {
        matches!(self, Fragment::PageBreak { .. })
    }

    /// Get font properties if this is a text fragment.
    pub fn font_props(&self) -> Option<&FontProps> {
        match self {
            Fragment::Text { font, .. } => Some(font),
            _ => None,
        }
    }
}

/// §17.3.1.37: minimum tab fragment width for line fitting.
/// Tabs resolve to tab stops defined on the paragraph; this constant is only
/// used as the fragment width during line breaking (actual tab position is
/// computed during paragraph layout).
pub const MIN_TAB_WIDTH: Pt = Pt::new(1.0);

/// Extract font properties from RunProperties with a default font family fallback.
///
/// `auto_fit` is the §20.1.2.1.18 `a:normAutofit` shrink of the enclosing shape
/// text body, applied to whichever size wins — the run's own or the inherited
/// default — because the scale is a property of the *body*, not of any run in
/// it. Every caller outside a shape text box passes
/// [`ShapeAutoFit::NONE`](crate::render::layout::ShapeAutoFit::NONE); it is a
/// parameter rather than a default so that a new call site has to say which it
/// is.
pub fn font_props_from_run(
    rp: &RunProperties,
    default_family: &str,
    default_size: Pt,
    auto_fit: crate::render::layout::ShapeAutoFit,
) -> FontProps {
    let family = effective_font(&rp.fonts).unwrap_or(default_family);

    let size = auto_fit.scale_font(rp.font_size.map(Pt::from).unwrap_or(default_size));

    let char_spacing = rp.spacing.map(Pt::from).unwrap_or(Pt::ZERO);

    let text_scale = rp.text_scale.map_or(1.0, |s| s.as_factor());

    FontProps {
        family: Rc::from(family),
        size,
        bold: rp.bold.unwrap_or(false),
        italic: rp.italic.unwrap_or(false),
        // §17.3.2.40: an actual underline style sets the bool. The model's
        // tri-state — `None` (inherit), `Some(UnderlineStyle::None)`
        // (explicit "no underline" override), `Some(_actual_style_)` —
        // collapses here into "draw / don't draw"; only the third case
        // draws.
        underline: matches!(rp.underline, Some(s) if s != UnderlineStyle::None),
        char_spacing,
        text_scale,
        // Populated by the measurer from Skia font metrics.
        underline_position: Pt::ZERO,
        underline_thickness: Pt::ZERO,
    }
}

/// Convert a number to lowercase Roman numerals.
pub fn to_roman_lower(mut n: u32) -> String {
    const VALS: [(u32, &str); 13] = [
        (1000, "m"),
        (900, "cm"),
        (500, "d"),
        (400, "cd"),
        (100, "c"),
        (90, "xc"),
        (50, "l"),
        (40, "xl"),
        (10, "x"),
        (9, "ix"),
        (5, "v"),
        (4, "iv"),
        (1, "i"),
    ];
    let mut s = String::new();
    for &(val, sym) in &VALS {
        while n >= val {
            s.push_str(sym);
            n -= val;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UnderlineStyle;

    #[test]
    fn font_props_default_fallback() {
        let rp = RunProperties::default();
        let fp = font_props_from_run(
            &rp,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert_eq!(&*fp.family, "Helvetica");
        assert_eq!(fp.size.raw(), 12.0);
        assert!(!fp.bold);
        assert!(!fp.italic);
    }

    // ── §17.3.2.40 underline tri-state ─────────────────────────────────────
    //
    // `RunProperties::underline: Option<UnderlineStyle>` carries three states:
    //   * `None`                            — element absent; inherit (§17.7.2)
    //   * `Some(UnderlineStyle::None)`      — `<w:u w:val="none"/>` explicit override
    //   * `Some(UnderlineStyle::Single)` …  — actual underline style
    // `font_props.underline` is the rendering-decision boolean: it must be
    // `true` only when an actual underline style is in effect.

    fn rp_with_underline(style: Option<UnderlineStyle>) -> RunProperties {
        RunProperties {
            underline: style,
            ..RunProperties::default()
        }
    }

    #[test]
    fn font_props_underline_absent_is_false() {
        let fp = font_props_from_run(
            &rp_with_underline(None),
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(!fp.underline, "no <w:u> element → no underline");
    }

    #[test]
    fn font_props_underline_explicit_none_is_false() {
        let fp = font_props_from_run(
            &rp_with_underline(Some(UnderlineStyle::None)),
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(
            !fp.underline,
            "<w:u w:val=\"none\"/> is the spec's explicit \"no underline\" \
             override; font_props.underline must remain false"
        );
    }

    #[test]
    fn font_props_underline_single_is_true() {
        let fp = font_props_from_run(
            &rp_with_underline(Some(UnderlineStyle::Single)),
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(fp.underline, "<w:u w:val=\"single\"/> → underline drawn");
    }

    #[test]
    fn font_props_text_scale_default_is_one() {
        // §17.3.2.45: when <w:w> is absent the run renders at 100% width.
        let fp = font_props_from_run(
            &RunProperties::default(),
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert_eq!(fp.text_scale, 1.0);
    }

    #[test]
    fn font_props_text_scale_compressed() {
        // <w:w w:val="80"/> → 0.8× horizontal scale.
        let rp = RunProperties {
            text_scale: Some(crate::model::TextScale::new(80)),
            ..RunProperties::default()
        };
        let fp = font_props_from_run(
            &rp,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!((fp.text_scale - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn font_props_text_scale_expanded() {
        // <w:w w:val="150"/> → 1.5× horizontal scale.
        let rp = RunProperties {
            text_scale: Some(crate::model::TextScale::new(150)),
            ..RunProperties::default()
        };
        let fp = font_props_from_run(
            &rp,
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!((fp.text_scale - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn font_props_underline_double_is_true() {
        // Sanity: any non-`None` style sets the bool. A future renderer
        // change to support distinct styles will replace this bool with
        // an enum; for now, "any style other than None" → draw.
        let fp = font_props_from_run(
            &rp_with_underline(Some(UnderlineStyle::Double)),
            "Helvetica",
            Pt::new(12.0),
            crate::render::layout::ShapeAutoFit::NONE,
        );
        assert!(fp.underline);
    }
}
