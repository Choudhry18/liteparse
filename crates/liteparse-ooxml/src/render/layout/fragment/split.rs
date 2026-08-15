//! Character-level splitting of over-wide text fragments.
//!
//! A word wider than the space available can't be broken at a normal break
//! opportunity, so it is split into one fragment per character and the
//! line-fitter breaks between them. Used for narrow table cells and deep
//! indents.

use std::rc::Rc;

use super::{FontProps, Fragment, TextMetrics};
use crate::render::dimension::Pt;

/// Text measurement callback. Structurally identical to
/// `paragraph::MeasureTextFn`, restated here so this module doesn't depend on
/// the paragraph layer — callers in either layer pass the same closures.
pub type MeasureFn<'a> = Option<&'a dyn Fn(&str, &FontProps) -> (Pt, TextMetrics)>;

/// True for a text fragment that is both too wide and actually splittable.
///
/// The character *count* is what matters: a single character cannot be split
/// however many bytes it occupies. An earlier byte-length (`text.len() > 1`)
/// spelling of this test disagreed with the split itself for any non-ASCII
/// single character — it reported "needs split", then split nothing, and the
/// caller paid for a full clone of the fragment vector.
fn needs_split(fragment: &Fragment, max_width: Pt) -> bool {
    matches!(
        fragment,
        Fragment::Text { width, text, .. } if *width > max_width && text.chars().count() > 1
    )
}

/// Split text fragments wider than `max_width` into per-character fragments.
///
/// Returns `None` when nothing needs splitting — the common case. That lets a
/// caller holding an owned `Vec` keep it untouched, and a caller holding a
/// slice hand back `Cow::Borrowed`; neither pays for a copy on the fast path.
///
/// Per-character widths come from `measure` when one is supplied. Without a
/// measurer the fragment's width is divided evenly across its characters,
/// which is only a positioning approximation — the total is preserved.
pub fn split_oversized_fragments(
    fragments: &[Fragment],
    max_width: Pt,
    measure: MeasureFn<'_>,
) -> Option<Vec<Fragment>> {
    // A non-positive budget can't be met by any split, and dividing by it
    // below would be meaningless.
    if max_width <= Pt::ZERO {
        return None;
    }
    if !fragments.iter().any(|f| needs_split(f, max_width)) {
        return None;
    }

    let mut result = Vec::with_capacity(fragments.len());
    // Reusable buffer for single-character measurement (avoids a per-character
    // heap allocation).
    let mut ch_buf = [0u8; 4];
    for frag in fragments {
        let Fragment::Text {
            text,
            width,
            font,
            color,
            shading,
            border,
            metrics,
            hyperlink_url,
            baseline_offset,
            ..
        } = frag
        else {
            result.push(frag.clone());
            continue;
        };
        if !needs_split(frag, max_width) {
            result.push(frag.clone());
            continue;
        }

        let char_count = text.chars().count();
        let per_char_fallback = *width / char_count as f32;
        for ch in text.chars() {
            let ch_str = ch.encode_utf8(&mut ch_buf);
            let (w, char_metrics) = match measure {
                Some(m) => m(ch_str, font),
                None => (per_char_fallback, *metrics),
            };
            result.push(Fragment::Text {
                text: Rc::from(&*ch_str),
                font: font.clone(),
                color: *color,
                shading: *shading,
                border: *border,
                width: w,
                trimmed_width: w,
                metrics: char_metrics,
                hyperlink_url: hyperlink_url.clone(),
                baseline_offset: *baseline_offset,
                text_offset: Pt::ZERO,
                // Per-character split of an over-wide word — not a mark.
                is_footnote_ref: false,
            });
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::resolve::color::RgbColor;

    fn text_frag(text: &str, width: f32) -> Fragment {
        Fragment::Text {
            text: Rc::from(text),
            font: Rc::new(FontProps {
                family: Rc::from("Test"),
                size: Pt::new(12.0),
                bold: false,
                italic: false,
                underline: false,
                char_spacing: Pt::ZERO,
                text_scale: 1.0,
                underline_position: Pt::ZERO,
                underline_thickness: Pt::ZERO,
            }),
            color: RgbColor::BLACK,
            width: Pt::new(width),
            trimmed_width: Pt::new(width),
            metrics: TextMetrics {
                ascent: Pt::new(10.0),
                descent: Pt::new(4.0),
                leading: Pt::ZERO,
            },
            hyperlink_url: None,
            shading: None,
            border: None,
            baseline_offset: Pt::ZERO,
            text_offset: Pt::ZERO,
            is_footnote_ref: false,
        }
    }

    fn texts(frags: &[Fragment]) -> Vec<String> {
        frags
            .iter()
            .map(|f| match f {
                Fragment::Text { text, .. } => text.to_string(),
                _ => panic!("expected Text fragment"),
            })
            .collect()
    }

    #[test]
    fn splits_into_one_fragment_per_character() {
        // "ab" at 60pt is wider than max_width=20pt → two 30pt characters
        // (uniform fallback — no measurer provided).
        let frags = vec![text_frag("ab", 60.0)];
        let result = split_oversized_fragments(&frags, Pt::new(20.0), None).expect("splits");
        assert_eq!(
            texts(&result),
            ["a", "b"],
            "one fragment per character, in order"
        );
        for frag in &result {
            let Fragment::Text { width, .. } = frag else {
                unreachable!()
            };
            assert!((width.raw() - 30.0).abs() < 1e-4, "uniform fallback 60/2");
        }
    }

    #[test]
    fn measurer_supplies_per_character_widths() {
        // A measurer that gives 'w' a wider advance than 'i' — the uniform
        // fallback would give both the same width.
        let measure = |t: &str, _: &FontProps| {
            let w = if t == "w" { 40.0 } else { 10.0 };
            (
                Pt::new(w),
                TextMetrics {
                    ascent: Pt::new(9.0),
                    descent: Pt::new(3.0),
                    leading: Pt::ZERO,
                },
            )
        };
        let frags = vec![text_frag("wi", 50.0)];
        let result =
            split_oversized_fragments(&frags, Pt::new(20.0), Some(&measure)).expect("splits");
        let widths: Vec<f32> = result
            .iter()
            .map(|f| match f {
                Fragment::Text { width, .. } => width.raw(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(widths, [40.0, 10.0], "measured, not evenly divided");
        // Metrics come from the measurer too, not the parent fragment.
        let Fragment::Text { metrics, .. } = &result[0] else {
            unreachable!()
        };
        assert_eq!(metrics.ascent.raw(), 9.0);
    }

    #[test]
    fn nothing_to_split_returns_none() {
        let frags = vec![text_frag("hi", 10.0)];
        assert!(split_oversized_fragments(&frags, Pt::new(100.0), None).is_none());
    }

    #[test]
    fn single_character_is_never_split() {
        // Wider than max_width, but one character — nothing to break.
        let frags = vec![text_frag("M", 200.0)];
        assert!(split_oversized_fragments(&frags, Pt::new(10.0), None).is_none());
    }

    /// The character count, not the byte length, decides splittability. A
    /// byte-length test reports "needs split" for these and then splits
    /// nothing, costing the caller a pointless clone of the whole vector.
    #[test]
    fn multi_byte_single_character_is_never_split() {
        for ch in ["é", "😀", "字"] {
            let frags = vec![text_frag(ch, 200.0)];
            assert!(
                split_oversized_fragments(&frags, Pt::new(10.0), None).is_none(),
                "{ch:?} is one character regardless of its byte length"
            );
        }
    }

    #[test]
    fn non_positive_max_width_returns_none() {
        let frags = vec![text_frag("ab", 60.0)];
        assert!(split_oversized_fragments(&frags, Pt::ZERO, None).is_none());
        assert!(split_oversized_fragments(&frags, Pt::new(-5.0), None).is_none());
    }

    #[test]
    fn fragments_that_fit_pass_through_unchanged() {
        let frags = vec![
            text_frag("ab", 60.0), // splits
            text_frag("ok", 5.0),  // fits — must survive whole
        ];
        let result = split_oversized_fragments(&frags, Pt::new(20.0), None).expect("splits");
        assert_eq!(texts(&result), ["a", "b", "ok"]);
    }
}
