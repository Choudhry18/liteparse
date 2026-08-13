//! Text measurer — resolves text widths through a `FontRegistry`.
//!
//! C′ rewrite of upstream's Skia measurer over fontdb + skrifa. The
//! measurement arithmetic is copied verbatim from upstream (`measure_str` is a
//! cmap → sum-of-`hmtx`-advances walk with no shaping or kerning, then
//! §17.3.2.45 `text_scale` on the advances and §17.3.2.35 `char_spacing` per
//! character), so it cannot diverge; the things that could — font resolution
//! and metric quantization — were measured in spikes 6–8:
//!
//! - widths agree with Skia to 99.82% exactly where both engines hold the
//!   same face, with no quantization divergence at any size;
//! - line metrics (`ascent`/`descent`/`leading`, so line height and therefore
//!   pagination) are **bit-exact** against both CoreText- and FreeType-backed
//!   Skia — every corpus face resolves its metrics through `hhea`, and skrifa
//!   agrees with both platform scalers on all of them;
//! - an unmapped codepoint contributes glyph 0's (.notdef) advance. The
//!   zero-contribution alternative was tried and *disproved* (spike 6): Skia
//!   has space-fallback handling of its own, and zeroing made parity worse.
//!
//! ## Sign conventions (the trap spike 7 hit)
//!
//! skrifa reports ascent positive-up and descent negative-down; [`TextMetrics`]
//! stores ascent positive-up and descent positive-down, so descent is negated
//! at this boundary. For underlines: upstream's comment claims Skia returns a
//! negative-below value and that it negates to positive-below — **the comment
//! has it backwards**. `SkFontMetrics::fUnderlinePosition` is positive below
//! the baseline; upstream negates it, so `underline_metrics` actually returns
//! a *negative-below* offset. skrifa's `post`-table `underlinePosition` is
//! already negative-below, so matching upstream means passing it through
//! untouched. (Diagnostic worth keeping: a 0%-exact comparison is never a font
//! disagreement — some face would agree by chance. 0% means a sign or formula
//! error on your own side.)
//!
//! ## Emoji
//!
//! Upstream shapes emoji clusters through Skia's HarfBuzz so ZWJ sequences
//! measure to their ligated width. No shaper is wired here yet, so
//! [`TextMeasurer::measure_with_typeface`] takes upstream's own documented
//! fallback path — the cmap-only advance sum — for every cluster. A
//! multi-codepoint sequence therefore over-measures relative to upstream by
//! (n−1) glyph advances; acceptable for now, and the seam for a harfrust
//! shaper is this one function.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use rustc_hash::FxHashMap;
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};

use crate::render::dimension::Pt;
use crate::render::emoji::resolve::{EmojiFamily, EmojiResolver, EmojiTypeface, RegistryLookup};
use crate::render::fonts::{FontRegistry, FontStyle, TypefaceEntry, TypefaceId};

use super::fragment::{FontProps, TextMetrics};

/// Per-(face, size) measurement state: the face's line metrics (constant for
/// the slot), its underline metrics, and a memo of raw advance sums per
/// distinct word seen. Keyed inside [`TextMeasurer`] by a stable slot index so
/// the family string is hashed once per distinct (family, size, style), not
/// once per call; the width memo is looked up by `&str` (via
/// `Box<str>: Borrow<str>`) so a cache hit allocates nothing.
struct FontSlot {
    face: fontdb::ID,
    size: Pt,
    metrics: TextMetrics,
    /// `post`-table underline (offset, thickness) in pt at `size`, already in
    /// upstream's negative-below convention (i.e. skrifa's, untouched).
    underline: Option<(f32, f32)>,
    /// Raw advance sums (before `text_scale` / `char_spacing`), which depend
    /// only on (face, size, text) — safe to reuse across runs that differ
    /// only in scale or spacing.
    widths: FxHashMap<Box<str>, f32>,
}

#[derive(Hash, Eq, PartialEq)]
struct SlotKey {
    family_lc: String,
    /// Font size as raw bits for exact f32 hashing.
    size_bits: u32,
    bold: bool,
    italic: bool,
}

/// Measures text using skrifa metrics for faces resolved through a
/// [`FontRegistry`]. Holds per-instance memoization so repeated words cost a
/// hash probe instead of a table walk.
///
/// Also owns the [`EmojiResolver`] for the same render, plus the warn-once
/// dedup set for `Unavailable` clusters — mirroring upstream.
pub struct TextMeasurer<'r> {
    registry: &'r FontRegistry,
    slots: RefCell<Vec<FontSlot>>,
    slot_index: RefCell<HashMap<SlotKey, usize>>,
    emoji_resolver: EmojiResolver<RegistryLookup<'r>>,
    /// Per-render dedup set so we warn at most once per cluster about a
    /// missing color emoji typeface.
    warned_emoji: RefCell<HashSet<String>>,
    /// Per-render memo of cluster advances from `measure_with_typeface`,
    /// keyed by (face id, size in raw-f32 bits, cluster text).
    emoji_advance_cache: RefCell<HashMap<(TypefaceId, u32, String), Pt>>,
}

impl<'r> TextMeasurer<'r> {
    pub fn new(registry: &'r FontRegistry) -> Self {
        Self {
            registry,
            slots: RefCell::new(Vec::new()),
            slot_index: RefCell::new(HashMap::new()),
            emoji_resolver: EmojiResolver::new(RegistryLookup { registry }),
            warned_emoji: RefCell::new(HashSet::new()),
            emoji_advance_cache: RefCell::new(HashMap::new()),
        }
    }

    pub fn registry(&self) -> &'r FontRegistry {
        self.registry
    }

    /// Slot for (family, size, bold, italic), resolving and reading the
    /// face's metrics on first sight.
    fn slot(&self, family: &str, size: Pt, bold: bool, italic: bool) -> usize {
        let key = SlotKey {
            family_lc: family.to_lowercase(),
            size_bits: f32::from(size).to_bits(),
            bold,
            italic,
        };
        if let Some(&idx) = self.slot_index.borrow().get(&key) {
            return idx;
        }
        let entry = self
            .registry
            .resolve(family, FontStyle::from_flags(bold, italic));
        let slot = self.build_slot(entry.id, size);
        let mut slots = self.slots.borrow_mut();
        slots.push(slot);
        let idx = slots.len() - 1;
        self.slot_index.borrow_mut().insert(key, idx);
        idx
    }

    fn build_slot(&self, face: fontdb::ID, size: Pt) -> FontSlot {
        let px = f32::from(size);
        let read = self.registry.db().with_face_data(face, |data, index| {
            let font = skrifa::FontRef::from_index(data, index).ok()?;
            let m = font.metrics(Size::new(px), LocationRef::default());
            Some((
                TextMetrics {
                    ascent: Pt::new(m.ascent),
                    // skrifa reports descent negative-down; TextMetrics wants
                    // positive-down.
                    descent: Pt::new(-m.descent),
                    leading: Pt::new(m.leading.max(0.0)),
                },
                m.underline.map(|u| (u.offset, u.thickness)),
            ))
        });
        let (metrics, underline) = read.flatten().unwrap_or_else(|| {
            log::warn!("face data unreadable for a resolved font; measuring as zero");
            (
                TextMetrics {
                    ascent: Pt::ZERO,
                    descent: Pt::ZERO,
                    leading: Pt::ZERO,
                },
                None,
            )
        });
        FontSlot {
            face,
            size,
            metrics,
            underline,
            widths: FxHashMap::default(),
        }
    }

    /// Raw cmap advance sum for `text` at the slot's size — upstream's
    /// `measure_str` equivalent (no shaping, no kerning). An unmapped
    /// codepoint takes glyph 0 (.notdef), which is what a cmap lookup
    /// genuinely yields — matching Skia rather than skipping the character.
    fn raw_advance(&self, face: fontdb::ID, size: Pt, text: &str) -> f32 {
        self.registry
            .db()
            .with_face_data(face, |data, index| {
                let Ok(font) = skrifa::FontRef::from_index(data, index) else {
                    return 0.0;
                };
                let charmap = font.charmap();
                let metrics =
                    font.glyph_metrics(Size::new(f32::from(size)), LocationRef::default());
                let mut sum = 0.0f32;
                for ch in text.chars() {
                    let gid = charmap.map(ch).unwrap_or(skrifa::GlyphId::NOTDEF);
                    sum += metrics.advance_width(gid).unwrap_or(0.0);
                }
                sum
            })
            .unwrap_or(0.0)
    }

    /// Measure a text string with the given font properties.
    /// Returns (width, TextMetrics).
    pub fn measure(&self, text: &str, font_props: &FontProps) -> (Pt, TextMetrics) {
        let idx = self.slot(
            &font_props.family,
            font_props.size,
            font_props.bold,
            font_props.italic,
        );
        let (face, size, text_metrics, cached) = {
            let slots = self.slots.borrow();
            let s = &slots[idx];
            (s.face, s.size, s.metrics, s.widths.get(text).copied())
        };
        let width = match cached {
            Some(w) => w,
            None => {
                let w = self.raw_advance(face, size, text);
                self.slots.borrow_mut()[idx]
                    .widths
                    .insert(Box::from(text), w);
                w
            }
        };

        // §17.3.2.45: scale the glyph advances horizontally per <w:w>.
        // Applies to glyph widths only — character spacing (§17.3.2.35) is
        // independent and is not scaled (the spec keeps the two separate so
        // kerning in points is unchanged by character scale).
        let scaled_width = Pt::new(width * font_props.text_scale);

        // §17.3.2.35: include character spacing in the measured width so line
        // fitting accounts for the extra inter-character space. Skip the
        // char-count scan entirely in the common no-spacing case.
        let spacing_extra = if font_props.char_spacing != Pt::ZERO {
            font_props.char_spacing * (text.chars().count() as f32)
        } else {
            Pt::ZERO
        };

        (scaled_width + spacing_extra, text_metrics)
    }

    /// Query font metrics for underline positioning.
    /// Returns (underline_position, underline_thickness) in points.
    ///
    /// The offset is **negative below the baseline** — see the module docs;
    /// upstream's own comment states the opposite of what its code does, and
    /// the parity harness proved the negative-below reading bit-exact.
    pub fn underline_metrics(&self, font_props: &FontProps) -> (Pt, Pt) {
        let idx = self.slot(
            &font_props.family,
            font_props.size,
            font_props.bold,
            font_props.italic,
        );
        let slots = self.slots.borrow();
        let s = &slots[idx];
        if s.underline.is_none() {
            log::warn!(
                "font '{}' ({:?}) missing underline metrics, using descent as fallback",
                font_props.family,
                font_props.size
            );
        }
        // Fallback mirrors upstream: descent's magnitude, below the baseline
        // (negative in this convention); thickness 1pt — the smallest visible
        // line at 72dpi.
        let position = Pt::new(
            s.underline
                .map(|(pos, _)| pos)
                .unwrap_or(-s.metrics.descent.raw()),
        );
        let thickness = Pt::new(s.underline.map(|(_, t)| t).unwrap_or(1.0));
        (position, thickness)
    }

    /// Get line height for the default font (used for empty paragraphs).
    /// §17.3.1.33: includes leading so Auto line spacing scales the full
    /// font-recommended height.
    pub fn default_line_height(&self, family: &str, size: Pt) -> Pt {
        let idx = self.slot(family, size, false, false);
        let slots = self.slots.borrow();
        let m = &slots[idx].metrics;
        m.ascent + m.descent + m.leading
    }

    // ─── Emoji pipeline integration ────────────────────────────────────────

    /// Resolve a color emoji typeface via the per-render [`EmojiResolver`].
    /// Cached: repeat calls with the same `requested` family are O(1).
    pub fn resolve_emoji(&self, requested: Option<EmojiFamily>) -> EmojiTypeface {
        self.emoji_resolver.resolve(requested)
    }

    /// Measure a cluster directly against a resolved [`TypefaceEntry`],
    /// bypassing the family-name lookup path. Used by the emoji pipeline,
    /// which has already resolved the typeface and needs metrics at the
    /// cluster's font size.
    ///
    /// Cmap-only — upstream's GSUB shaping fallback path, taken always here
    /// until a harfrust shaper is wired in (see module docs).
    pub fn measure_with_typeface(
        &self,
        text: &str,
        typeface: &TypefaceEntry,
        size: Pt,
    ) -> (Pt, TextMetrics) {
        let px = f32::from(size);
        let text_metrics = self
            .registry
            .db()
            .with_face_data(typeface.id, |data, index| {
                let font = skrifa::FontRef::from_index(data, index).ok()?;
                let m = font.metrics(Size::new(px), LocationRef::default());
                Some(TextMetrics {
                    ascent: Pt::new(m.ascent),
                    descent: Pt::new(-m.descent),
                    leading: Pt::new(m.leading.max(0.0)),
                })
            })
            .flatten()
            .unwrap_or(TextMetrics {
                ascent: Pt::ZERO,
                descent: Pt::ZERO,
                leading: Pt::ZERO,
            });

        let key = (TypefaceId(typeface.id), px.to_bits(), text.to_owned());
        if let Some(&cached) = self.emoji_advance_cache.borrow().get(&key) {
            return (cached, text_metrics);
        }
        let advance = Pt::new(self.raw_advance(typeface.id, size, text));
        self.emoji_advance_cache.borrow_mut().insert(key, advance);
        (advance, text_metrics)
    }

    /// Log a warning once per cluster when no color emoji typeface is
    /// available on the host. The `attempted` list lets operators know
    /// which packages to install (e.g. `fonts-noto-color-emoji` on Debian).
    pub fn warn_emoji_unavailable_once(&self, cluster: &str, attempted: &[EmojiFamily]) {
        let inserted = self.warned_emoji.borrow_mut().insert(cluster.to_string());
        if inserted {
            log::warn!(
                "no color emoji typeface available for cluster {:?}; \
                 tried {:?}. Install a color emoji font on the host \
                 (e.g. fonts-noto-color-emoji) to render this cluster correctly.",
                cluster,
                attempted
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn fp_at_scale(scale: f32) -> FontProps {
        FontProps {
            family: Rc::from("Helvetica"),
            size: Pt::new(12.0),
            bold: false,
            italic: false,
            underline: false,
            char_spacing: Pt::ZERO,
            text_scale: scale,
            underline_position: Pt::ZERO,
            underline_thickness: Pt::ZERO,
        }
    }

    #[test]
    fn measure_scaled_width_is_proportional_to_text_scale() {
        // §17.3.2.45: glyph advances scale linearly with <w:w>. A run at 80%
        // must measure to 0.8× the width of the same text at 100%.
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);

        let (w_100, _) = measurer.measure("scaling sample", &fp_at_scale(1.0));
        let (w_80, _) = measurer.measure("scaling sample", &fp_at_scale(0.8));
        let (w_150, _) = measurer.measure("scaling sample", &fp_at_scale(1.5));

        // The relationship must hold even though the absolute value depends
        // on which fallback font resolution picks on the test host.
        if w_100.raw() <= 0.0 {
            // A fontless CI host can't measure text — bail rather than
            // assert; the text path is exercised by the corpus runs.
            return;
        }
        assert!(
            (w_80.raw() / w_100.raw() - 0.8).abs() < 0.01,
            "80% scale must produce 0.8× width: 100%={}, 80%={}",
            w_100.raw(),
            w_80.raw(),
        );
        assert!(
            (w_150.raw() / w_100.raw() - 1.5).abs() < 0.01,
            "150% scale must produce 1.5× width: 100%={}, 150%={}",
            w_100.raw(),
            w_150.raw(),
        );
    }

    #[test]
    fn measure_char_spacing_not_scaled_by_text_scale() {
        // §17.3.2.45 + §17.3.2.35: w:spacing (inter-character spacing)
        // is independent of w:w. Doubling text_scale must NOT double
        // the spacing contribution.
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);

        let mut fp_scale_1 = fp_at_scale(1.0);
        fp_scale_1.char_spacing = Pt::new(2.0);
        let mut fp_scale_2 = fp_at_scale(2.0);
        fp_scale_2.char_spacing = Pt::new(2.0);

        let text = "abcde";
        let (w1, _) = measurer.measure(text, &fp_scale_1);
        let (w2, _) = measurer.measure(text, &fp_scale_2);

        if w1.raw() <= 0.0 {
            return;
        }
        // Spacing contribution = 5 chars × 2pt = 10pt at both scales.
        // Glyph contribution doubles. So w2 - w1 should equal the glyph
        // contribution at scale 1.0 (i.e. w1 minus the spacing extra).
        let expected_glyph_w1 = w1.raw() - 10.0;
        let observed_glyph_delta = w2.raw() - w1.raw();
        assert!(
            (observed_glyph_delta - expected_glyph_w1).abs() < 0.05,
            "char_spacing must not be scaled by text_scale: \
             w1={}, w2={}, expected glyph delta {}, observed {}",
            w1.raw(),
            w2.raw(),
            expected_glyph_w1,
            observed_glyph_delta,
        );
    }

    #[test]
    fn underline_position_is_negative_below_baseline() {
        // The sign trap from spike 7: skrifa's post-table value passes
        // through untouched, so for any real font the offset must come back
        // negative (below the baseline).
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let (pos, thick) = measurer.underline_metrics(&fp_at_scale(1.0));
        if thick.raw() == 1.0 && pos.raw() == 0.0 {
            return; // fontless host fallback
        }
        assert!(
            pos.raw() < 0.0,
            "underline offset must be negative-below, got {}",
            pos.raw()
        );
        assert!(thick.raw() > 0.0);
    }

    #[test]
    fn default_line_height_is_metrics_sum() {
        let registry = FontRegistry::new();
        let measurer = TextMeasurer::new(&registry);
        let dlh = measurer.default_line_height("Helvetica", Pt::new(12.0));
        let (_, m) = measurer.measure("x", &fp_at_scale(1.0));
        assert_eq!(dlh.raw(), (m.ascent + m.descent + m.leading).raw());
    }
}
