//! Shared formatting primitives — borders, shading, tabs, alignment, and enums
//! used across paragraph, table, and run properties.

use crate::model::dimension::{Dimension, EighthPoints, Points, Twips};

use super::color::Color;

// ── Alignment ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    Start,
    Center,
    End,
    Both,
    Distribute,
    Thai,
}

// ── Number Format ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumberFormat {
    Decimal,
    UpperRoman,
    LowerRoman,
    UpperLetter,
    LowerLetter,
    Bullet,
    Ordinal,
    CardinalText,
    OrdinalText,
    None,
}

// ── Height Rule ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeightRule {
    Auto,
    Exact,
    AtLeast,
}

// ── Borders ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParagraphBorders {
    pub top: Option<Border>,
    pub bottom: Option<Border>,
    pub left: Option<Border>,
    pub right: Option<Border>,
    pub between: Option<Border>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Border {
    pub style: BorderStyle,
    /// §17.3.4: border width in eighths of a point (ST_EighthPointMeasure).
    pub width: Dimension<EighthPoints>,
    /// §17.3.4: spacing offset (ST_PointMeasure §17.18.68).
    pub space: Dimension<Points>,
    pub color: Color,
}

/// §17.18.2 `ST_Border`.
///
/// `Nil` and `None` are **not** synonyms and must not be merged. Both draw
/// nothing, but [MS-OI29500] §17.4.66 separates them in table border conflict
/// resolution: a `nil` edge *suppresses* the shared border outright, while a
/// `none` edge behaves exactly like an omitted one — it inherits from the style
/// and table-level borders, and yields to the opposing cell's border. Use
/// [`BorderStyle::draws_nothing`] wherever only "is there a line to paint"
/// matters, so the distinction can't be lost by an `== None` comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BorderStyle {
    /// `val="nil"` — no border, and it wins conflict resolution.
    Nil,
    /// `val="none"` — no border, but inherits and yields like an omitted edge.
    None,
    Single,
    Thick,
    Double,
    Dotted,
    Dashed,
    DotDash,
    DotDotDash,
    Triple,
    ThinThickSmallGap,
    ThickThinSmallGap,
    ThinThickThinSmallGap,
    ThinThickMediumGap,
    ThickThinMediumGap,
    ThinThickThinMediumGap,
    ThinThickLargeGap,
    ThickThinLargeGap,
    ThinThickThinLargeGap,
    Wave,
    DoubleWave,
    DashSmallGap,
    DashDotStroked,
    ThreeDEmboss,
    ThreeDEngrave,
    Outset,
    Inset,
}

impl BorderStyle {
    /// Whether this style paints no line at all — true for both `nil` and
    /// `none` (§17.18.2).
    ///
    /// Every consumer that only asks "is there a border to draw" should call
    /// this rather than comparing against a variant, because the two differ
    /// solely in table conflict resolution ([MS-OI29500] §17.4.66). Comparing
    /// `== BorderStyle::None` is how the distinction was lost before: it
    /// silently answered "no" for `nil` too, at every site outside the table
    /// resolver where the difference is genuinely irrelevant — and at the one
    /// site where it isn't.
    pub fn draws_nothing(self) -> bool {
        matches!(self, Self::Nil | Self::None)
    }
}

// ── Shading ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shading {
    pub fill: Color,
    pub pattern: ShadingPattern,
    pub color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadingPattern {
    /// §17.18.78 ST_Shd: `nil` — no shading whatsoever.
    Nil,
    Clear,
    Solid,
    HorzStripe,
    VertStripe,
    ReverseDiagStripe,
    DiagStripe,
    HorzCross,
    DiagCross,
    ThinHorzStripe,
    ThinVertStripe,
    ThinReverseDiagStripe,
    ThinDiagStripe,
    ThinHorzCross,
    ThinDiagCross,
    Pct5,
    Pct10,
    Pct12,
    Pct15,
    Pct20,
    Pct25,
    Pct30,
    Pct35,
    Pct37,
    Pct40,
    Pct45,
    Pct50,
    Pct55,
    Pct60,
    Pct62,
    Pct65,
    Pct70,
    Pct75,
    Pct80,
    Pct85,
    Pct87,
    Pct90,
    Pct95,
}

// ── Tabs ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabStop {
    pub position: Dimension<Twips>,
    pub alignment: TabAlignment,
    pub leader: TabLeader,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabAlignment {
    Left,
    Center,
    Right,
    Decimal,
    Bar,
    Clear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabLeader {
    None,
    Dot,
    Hyphen,
    Underscore,
    Heavy,
    MiddleDot,
}

// ── Absolute position tabs (§17.3.1.30 w:ptab) ───────────────────────────────

/// §17.3.1.30: an absolute-position tab (`<w:ptab>`). Unlike a regular tab
/// (§17.3.3.29), it carries no `pos`; the position is derived from
/// `relative_to` and `alignment` at layout time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionTab {
    /// §17.18.59: how following content aligns to the derived position.
    pub alignment: PTabAlignment,
    /// §17.18.61: reference the position is measured against.
    pub relative_to: PTabRelativeTo,
    /// §17.18.60: leader character filling the gap.
    pub leader: PTabLeader,
}

/// §17.18.59 ST_PTabAlignment — a strict subset of tab alignments (no
/// decimal/bar/clear).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PTabAlignment {
    Left,
    Center,
    Right,
}

/// §17.18.61 ST_PTabRelativeTo — the base the position is measured from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PTabRelativeTo {
    /// Relative to the page text margins.
    Margin,
    /// Relative to the current paragraph indents.
    Indent,
}

/// §17.18.60 ST_PTabLeader — leader characters for a position tab. A subset of
/// [`TabLeader`] (no `heavy`); [`From`] lets layout reuse the tab-leader painter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PTabLeader {
    None,
    Dot,
    Hyphen,
    Underscore,
    MiddleDot,
}

impl From<PTabLeader> for TabLeader {
    fn from(l: PTabLeader) -> Self {
        match l {
            PTabLeader::None => Self::None,
            PTabLeader::Dot => Self::Dot,
            PTabLeader::Hyphen => Self::Hyphen,
            PTabLeader::Underscore => Self::Underscore,
            PTabLeader::MiddleDot => Self::MiddleDot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cnf_first_row_bit_is_leftmost() {
        assert_eq!(CnfStyle::from_val_str("100000000000"), CnfStyle::FIRST_ROW);
    }

    #[test]
    fn cnf_last_row_last_column_is_rightmost() {
        assert_eq!(
            CnfStyle::from_val_str("000000000001"),
            CnfStyle::LAST_ROW_LAST_COLUMN
        );
    }

    #[test]
    fn cnf_all_twelve_bits() {
        assert_eq!(CnfStyle::from_val_str("111111111111"), CnfStyle::all());
    }

    #[test]
    fn cnf_empty_is_empty() {
        assert_eq!(CnfStyle::from_val_str(""), CnfStyle::empty());
    }

    #[test]
    fn cnf_short_string_sets_only_leading_bits() {
        assert_eq!(
            CnfStyle::from_val_str("11"),
            CnfStyle::FIRST_ROW | CnfStyle::LAST_ROW
        );
    }

    #[test]
    fn cnf_chars_beyond_twelve_are_ignored() {
        assert_eq!(
            CnfStyle::from_val_str("100000000000ZZZ"),
            CnfStyle::FIRST_ROW
        );
    }

    #[test]
    fn cnf_non_one_chars_leave_bits_unset() {
        // Only '1' sets a bit; '0' and anything else leave it clear.
        assert_eq!(CnfStyle::from_val_str("0x0000000000"), CnfStyle::empty());
    }

    #[test]
    fn ptab_leader_maps_to_tab_leader() {
        assert_eq!(TabLeader::from(PTabLeader::None), TabLeader::None);
        assert_eq!(TabLeader::from(PTabLeader::Dot), TabLeader::Dot);
        assert_eq!(TabLeader::from(PTabLeader::Hyphen), TabLeader::Hyphen);
        assert_eq!(
            TabLeader::from(PTabLeader::Underscore),
            TabLeader::Underscore
        );
        assert_eq!(TabLeader::from(PTabLeader::MiddleDot), TabLeader::MiddleDot);
    }
}

// ── Conditional Formatting ───────────────────────────────────────────────────

bitflags::bitflags! {
    /// §17.3.1.8: conditional formatting region flags indicating which table
    /// style regions apply to an element (paragraph, row, or cell).
    ///
    /// The 12 bits correspond to the positional regions defined in ST_CnfType.
    /// The legacy `val` binary string (e.g. `"100000000000"`) maps to these
    /// bits left-to-right: bit 0 = firstRow, …, bit 11 = lastRowLastColumn.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct CnfStyle: u16 {
        const FIRST_ROW              = 1 << 0;
        const LAST_ROW               = 1 << 1;
        const FIRST_COLUMN           = 1 << 2;
        const LAST_COLUMN            = 1 << 3;
        const ODD_V_BAND             = 1 << 4;
        const EVEN_V_BAND            = 1 << 5;
        const ODD_H_BAND             = 1 << 6;
        const EVEN_H_BAND            = 1 << 7;
        const FIRST_ROW_FIRST_COLUMN = 1 << 8;
        const FIRST_ROW_LAST_COLUMN  = 1 << 9;
        const LAST_ROW_FIRST_COLUMN  = 1 << 10;
        const LAST_ROW_LAST_COLUMN   = 1 << 11;
    }
}

impl CnfStyle {
    /// Parse the legacy 12-character `val` binary string (§17.3.1.8).
    ///
    /// Each character position maps to a flag left-to-right: `'1'` sets the
    /// flag, `'0'` or any other character leaves it unset. Characters beyond
    /// position 11 are ignored.
    pub fn from_val_str(s: &str) -> Self {
        let flags = [
            CnfStyle::FIRST_ROW,
            CnfStyle::LAST_ROW,
            CnfStyle::FIRST_COLUMN,
            CnfStyle::LAST_COLUMN,
            CnfStyle::ODD_V_BAND,
            CnfStyle::EVEN_V_BAND,
            CnfStyle::ODD_H_BAND,
            CnfStyle::EVEN_H_BAND,
            CnfStyle::FIRST_ROW_FIRST_COLUMN,
            CnfStyle::FIRST_ROW_LAST_COLUMN,
            CnfStyle::LAST_ROW_FIRST_COLUMN,
            CnfStyle::LAST_ROW_LAST_COLUMN,
        ];
        s.bytes()
            .zip(flags.iter())
            .fold(
                CnfStyle::empty(),
                |acc, (ch, &flag)| {
                    if ch == b'1' { acc | flag } else { acc }
                },
            )
    }
}

// ── Text Alignment ───────────────────────────────────────────────────────────

/// §17.18.91 ST_TextAlignment — vertical alignment of characters on a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextAlignment {
    Auto,
    Top,
    Center,
    Baseline,
    Bottom,
}

// ── Positioning enums (shared by table and frame) ────────────────────────────

/// §17.18.106 ST_VAnchor — vertical/horizontal anchor for table positioning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableAnchor {
    Text,
    Margin,
    Page,
}

/// §17.18.108 ST_XAlign — horizontal alignment for floating table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableXAlign {
    Left,
    Center,
    Right,
    Inside,
    Outside,
}

/// §17.18.109 ST_YAlign — vertical alignment for floating table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableYAlign {
    Top,
    Center,
    Bottom,
    Inside,
    Outside,
    Inline,
}
