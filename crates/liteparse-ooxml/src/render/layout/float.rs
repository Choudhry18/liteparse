//! Floating element layout — positioned outside the normal flow.

use crate::render::dimension::Pt;
use crate::render::geometry::PtRect;

/// §17.4.56 / §20.4.2: source of a floating element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSource {
    /// §20.4.2: floating image — wraps text for all overlapping lines.
    Image,
    /// §20.4.2: floating DrawingML shape — wraps text for all overlapping
    /// lines. Semantically identical to `Image` for line narrowing; kept
    /// as a distinct variant so the stacker can detect shape-owned rects
    /// when debugging.
    Shape,
    /// §17.4.56: floating table — only wraps text for the paragraph that
    /// was active when the table was encountered. Subsequent paragraphs
    /// clear below the table.
    Table {
        /// Block index of the paragraph that owns this floating table.
        /// Only this paragraph (and earlier ones) should wrap around it.
        owner_block_idx: usize,
    },
}

impl FloatSource {
    /// §17.4.57 — whether `tblOverlap` governs collisions with this float.
    ///
    /// It does not, for images and shapes. `tblOverlap` is defined purely
    /// between *floating tables*: "whether the current table shall allow
    /// other floating tables to overlap its extents". A table may still
    /// visually overlap a floating image, and Word lets it.
    ///
    /// Named rather than spelled `matches!(.., Table { .. })` at the use
    /// site because the interesting content is the spec rule, not the
    /// pattern — the anchor resolver reads as "skip what tblOverlap
    /// doesn't govern".
    pub fn participates_in_table_overlap(&self) -> bool {
        matches!(self, Self::Table { .. })
    }
}

/// A floating element that affects text layout on the current page.
#[derive(Debug, Clone)]
pub struct ActiveFloat {
    /// Absolute x position on page.
    pub page_x: Pt,
    /// Top of the float on the page.
    pub page_y_start: Pt,
    /// Bottom of the float on the page.
    pub page_y_end: Pt,
    /// Width of the float.
    pub width: Pt,
    /// Source of this float (image vs shape vs table).
    pub source: FloatSource,
    /// §20.4.2.20 ST_WrapText — which sides of the float text may flow on.
    /// `Table` sources default to `BothSides`; images/shapes carry the
    /// value from their `wrapSquare/Tight/Through` element.
    pub wrap_text: WrapTextSide,
}

/// §20.4.2.20 ST_WrapText — side(s) of a float on which text may flow.
///
/// Duplicated in the layout layer to decouple it from the model enum so
/// `float.rs` doesn't depend on `crate::model`; a `From` impl bridges the
/// two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapTextSide {
    /// Text flows on both sides (default).
    BothSides,
    /// Text flows on the left only — float pushed to the right edge.
    Left,
    /// Text flows on the right only — float pushed to the left edge.
    Right,
    /// Text flows on the larger of the two sides.
    Largest,
}

impl From<crate::model::WrapText> for WrapTextSide {
    fn from(w: crate::model::WrapText) -> Self {
        match w {
            crate::model::WrapText::BothSides => Self::BothSides,
            crate::model::WrapText::Left => Self::Left,
            crate::model::WrapText::Right => Self::Right,
            crate::model::WrapText::Largest => Self::Largest,
        }
    }
}

impl ActiveFloat {
    /// Whether a given y-position overlaps this float's vertical range.
    pub fn overlaps_y(&self, y: Pt) -> bool {
        y >= self.page_y_start && y < self.page_y_end
    }

    /// The rectangle occupied by this float.
    pub fn rect(&self) -> PtRect {
        PtRect::from_xywh(
            self.page_x,
            self.page_y_start,
            self.width,
            self.page_y_end - self.page_y_start,
        )
    }
}

/// Compute how much the available width should be reduced on a given line
/// due to active floating images.
///
/// Returns (indent_left, indent_right) — additional indentation to avoid floats.
/// `line_y` is the top of the line, `line_height` is the line's height.
/// A line overlaps a float if any part of the line's vertical range intersects
/// the float's vertical range.
pub fn float_adjustments(
    floats: &[ActiveFloat],
    line_y: Pt,
    page_x: Pt,
    content_width: Pt,
) -> (Pt, Pt) {
    float_adjustments_with_height(floats, line_y, Pt::ZERO, page_x, content_width)
}

/// Like `float_adjustments` but with explicit line height for overlap checking.
///
/// Honours §20.4.2.20 `wrap_text` per float:
///  * `BothSides` — text may flow on either side; narrow the closer side.
///  * `Left` — text only on the left; always push text to the left of the float.
///  * `Right` — text only on the right; always push text to the right of the float.
///  * `Largest` — text only on the side with more remaining width.
pub fn float_adjustments_with_height(
    floats: &[ActiveFloat],
    line_y: Pt,
    line_height: Pt,
    page_x: Pt,
    content_width: Pt,
) -> (Pt, Pt) {
    let mut indent_left = Pt::ZERO;
    let mut indent_right = Pt::ZERO;
    let line_bottom = line_y + line_height;

    for float in floats {
        // Check if any part of the line overlaps the float vertically.
        if line_bottom <= float.page_y_start || line_y >= float.page_y_end {
            continue;
        }

        let float_right_edge = float.page_x + float.width;
        let content_right = page_x + content_width;
        let left_shift = (float_right_edge - page_x).max(Pt::ZERO);
        let right_shift = (content_right - float.page_x).max(Pt::ZERO);

        // Choose the side to narrow per `wrap_text`.
        //  * `Left`     → text on the left of the float → narrow from the right.
        //  * `Right`    → text on the right of the float → narrow from the left.
        //  * `Largest`  → keep the larger remaining side; narrow the smaller.
        //  * `BothSides`→ narrow the side the float is closer to.
        let narrow_left = match float.wrap_text {
            WrapTextSide::Right => true,
            WrapTextSide::Left => false,
            WrapTextSide::Largest => {
                let left_remaining = float.page_x - page_x;
                let right_remaining = content_right - float_right_edge;
                left_remaining < right_remaining
            }
            WrapTextSide::BothSides => {
                let content_center = page_x + content_width * 0.5;
                let float_center = float.page_x + float.width * 0.5;
                float_center < content_center
            }
        };

        if narrow_left {
            if left_shift > indent_left {
                indent_left = left_shift;
            }
        } else if right_shift > indent_right {
            indent_right = right_shift;
        }
    }

    (indent_left, indent_right)
}

/// Remove floats that the cursor has passed below.
pub fn prune_floats(floats: &mut Vec<ActiveFloat>, cursor_y: Pt) {
    floats.retain(|f| cursor_y < f.page_y_end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_floats_no_adjustment() {
        let (l, r) = float_adjustments(&[], Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert_eq!(l.raw(), 0.0);
        assert_eq!(r.raw(), 0.0);
    }

    #[test]
    fn float_on_left_pushes_text_right() {
        let floats = vec![ActiveFloat {
            page_x: Pt::new(72.0), // at left margin
            page_y_start: Pt::new(80.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(100.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::BothSides,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert_eq!(l.raw(), 100.0, "push right by float width");
        assert_eq!(r.raw(), 0.0);
    }

    #[test]
    fn float_on_right_pushes_text_left() {
        let floats = vec![ActiveFloat {
            page_x: Pt::new(440.0), // near right margin
            page_y_start: Pt::new(80.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(100.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::BothSides,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert_eq!(l.raw(), 0.0);
        assert!(r.raw() > 0.0, "should indent from right");
    }

    #[test]
    fn float_not_overlapping_line_no_adjustment() {
        let floats = vec![ActiveFloat {
            page_x: Pt::new(72.0),
            page_y_start: Pt::new(200.0),
            page_y_end: Pt::new(300.0),
            width: Pt::new(100.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::BothSides,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert_eq!(l.raw(), 0.0, "line is above float");
        assert_eq!(r.raw(), 0.0);
    }

    #[test]
    fn prune_removes_passed_floats() {
        let mut floats = vec![
            ActiveFloat {
                page_x: Pt::ZERO,
                page_y_start: Pt::new(0.0),
                page_y_end: Pt::new(100.0),
                width: Pt::new(50.0),
                source: FloatSource::Image,
                wrap_text: WrapTextSide::BothSides,
            },
            ActiveFloat {
                page_x: Pt::ZERO,
                page_y_start: Pt::new(0.0),
                page_y_end: Pt::new(300.0),
                width: Pt::new(50.0),
                source: FloatSource::Image,
                wrap_text: WrapTextSide::BothSides,
            },
        ];
        prune_floats(&mut floats, Pt::new(150.0));
        assert_eq!(floats.len(), 1, "first float pruned, second still active");
    }

    #[test]
    fn wrap_text_right_forces_narrow_from_left_even_if_float_is_right() {
        // Float at right, wrap_text=Right → text flows on right of float,
        // so line is narrowed from the LEFT (pushed right past the float).
        let floats = vec![ActiveFloat {
            page_x: Pt::new(440.0),
            page_y_start: Pt::new(80.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(100.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::Right,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert!(
            l.raw() > 0.0,
            "text on right of float means narrow from left"
        );
        assert_eq!(r.raw(), 0.0);
    }

    #[test]
    fn wrap_text_left_forces_narrow_from_right_even_if_float_is_left() {
        let floats = vec![ActiveFloat {
            page_x: Pt::new(72.0),
            page_y_start: Pt::new(80.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(100.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::Left,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        assert_eq!(l.raw(), 0.0);
        assert!(
            r.raw() > 0.0,
            "text on left of float means narrow from right"
        );
    }

    #[test]
    fn wrap_text_largest_picks_wider_remaining_side() {
        // Float centered slightly left of center → right remaining is larger.
        // `Largest` should keep text on the right (narrow from the left).
        let floats = vec![ActiveFloat {
            page_x: Pt::new(200.0),
            page_y_start: Pt::new(80.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(50.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::Largest,
        }];
        let (l, r) = float_adjustments(&floats, Pt::new(100.0), Pt::new(72.0), Pt::new(468.0));
        // Left remaining: 200-72 = 128; right remaining: (72+468) - (200+50) = 290.
        // Right is larger → narrow from the left.
        assert!(l.raw() > 0.0);
        assert_eq!(r.raw(), 0.0);
    }

    #[test]
    fn overlaps_y_boundary() {
        let f = ActiveFloat {
            page_x: Pt::ZERO,
            page_y_start: Pt::new(100.0),
            page_y_end: Pt::new(200.0),
            width: Pt::new(50.0),
            source: FloatSource::Image,
            wrap_text: WrapTextSide::BothSides,
        };
        assert!(!f.overlaps_y(Pt::new(99.0)));
        assert!(f.overlaps_y(Pt::new(100.0)));
        assert!(f.overlaps_y(Pt::new(150.0)));
        assert!(!f.overlaps_y(Pt::new(200.0)));
    }
}
