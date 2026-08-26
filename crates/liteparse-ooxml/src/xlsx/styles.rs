//! `xl/styles.xml` (§18.8): the indirection from a cell's `s=` to the things a
//! renderer needs — the number-format code, the font, the alignment.
//!
//! The chain is `<c s="7">` → `<cellXfs>` entry 7 → its `numFmtId` → either a
//! builtin code (§18.8.30) or the workbook's own `<numFmt>`.
//!
//! **Two containers in this part hold identically-named children, and reading
//! the wrong one fails silently rather than loudly**, which is why every
//! collector below is gated on its enclosing element:
//!
//! * `<cellStyleXfs>` also holds `<xf>` elements. Only `<cellXfs>` is indexed
//!   by a cell's `s=`. Reading the other array yields plausible-looking
//!   formats attached to the wrong cells.
//! * `<dxfs>` (differential formats, used by conditional formatting) also
//!   holds `<font>`, `<fill>` and `<border>` elements. Appending those to the
//!   corresponding list shifts every id above them.
//!
//! # Colour
//!
//! Fills, borders and font colours are read for the raster (a bbox consumer
//! never asks what colour a cell is). Every one of them names its colour
//! through the same four-way `CT_Color` indirection, and the corpus shares
//! them out unevenly enough that skipping any one of the four is visible —
//! `xlsx_paint_census` over 1,248 workbooks: `indexed=` 49.5% of definitions,
//! `theme=` 29.8%, `rgb=` 16.5%, `auto=` 4.2%.
//!
//! Two things about that table are load-bearing:
//!
//! * **`indexed=` is mostly not the palette.** 93.2% of its uses are 64/65/8
//!   — "the default colour" — so [`INDEXED_PALETTE`] is a vendored constant
//!   for a 6.8% tail rather than a subsystem. It cannot be *skipped*: 4.6% of
//!   workbooks override it with their own `<indexedColors>`.
//! * **`theme=` indices are not `clrScheme` order.** SpreadsheetML swaps
//!   0↔1 and 2↔3 (0 = lt1/background, 1 = dk1/text). Getting it backwards
//!   inverts every themed fill in the file, which looks deliberate rather
//!   than broken. See [`Styles::resolve_color`].

use std::collections::HashMap;

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::docx::error::Result;
use crate::model::ThemeColorScheme;
use crate::xlsx::xml::{attr, attr_bool, attr_parse, local_name};

/// The format code applied when a cell names no style at all.
pub const GENERAL: &str = "General";

/// Custom format ids start here (§18.8.30); anything below with no builtin
/// entry is reserved and Excel does not write it.
pub const FIRST_CUSTOM_FORMAT_ID: u32 = 164;

/// ECMA-376 §18.8.30 builtin number formats.
///
/// Ids 23–36 and 41–44 are absent on purpose: the spec reserves them or makes
/// them locale-dependent, Excel does not write them, and a reader that invents
/// codes for them is guessing at the user's locale.
const BUILTIN_FORMATS: &[(u32, &str)] = &[
    (0, "General"),
    (1, "0"),
    (2, "0.00"),
    (3, "#,##0"),
    (4, "#,##0.00"),
    (9, "0%"),
    (10, "0.00%"),
    (11, "0.00E+00"),
    (12, "# ?/?"),
    (13, "# ??/??"),
    (14, "mm-dd-yy"),
    (15, "d-mmm-yy"),
    (16, "d-mmm"),
    (17, "mmm-yy"),
    (18, "h:mm AM/PM"),
    (19, "h:mm:ss AM/PM"),
    (20, "h:mm"),
    (21, "h:mm:ss"),
    (22, "m/d/yy h:mm"),
    (37, "#,##0 ;(#,##0)"),
    (38, "#,##0 ;[Red](#,##0)"),
    (39, "#,##0.00;(#,##0.00)"),
    (40, "#,##0.00;[Red](#,##0.00)"),
    (45, "mm:ss"),
    (46, "[h]:mm:ss"),
    (47, "mmss.0"),
    (48, "##0.0E+0"),
    (49, "@"),
];

/// Look up a builtin format code by id.
pub fn builtin_format(id: u32) -> Option<&'static str> {
    BUILTIN_FORMATS
        .iter()
        .find_map(|&(k, code)| (k == id).then_some(code))
}

/// §18.8.27 `<indexedColors>`: the legacy palette a colour's `indexed=`
/// points into, in index order. Indices 64 and 65 are past this table on
/// purpose — they are the "system foreground" / "system background"
/// sentinels, which resolve to whatever the consumer's default is rather than
/// to a stored colour.
pub const INDEXED_PALETTE: [[u8; 3]; 64] = [
    [0x00, 0x00, 0x00],
    [0xFF, 0xFF, 0xFF],
    [0xFF, 0x00, 0x00],
    [0x00, 0xFF, 0x00],
    [0x00, 0x00, 0xFF],
    [0xFF, 0xFF, 0x00],
    [0xFF, 0x00, 0xFF],
    [0x00, 0xFF, 0xFF],
    [0x00, 0x00, 0x00],
    [0xFF, 0xFF, 0xFF],
    [0xFF, 0x00, 0x00],
    [0x00, 0xFF, 0x00],
    [0x00, 0x00, 0xFF],
    [0xFF, 0xFF, 0x00],
    [0xFF, 0x00, 0xFF],
    [0x00, 0xFF, 0xFF],
    [0x80, 0x00, 0x00],
    [0x00, 0x80, 0x00],
    [0x00, 0x00, 0x80],
    [0x80, 0x80, 0x00],
    [0x80, 0x00, 0x80],
    [0x00, 0x80, 0x80],
    [0xC0, 0xC0, 0xC0],
    [0x80, 0x80, 0x80],
    [0x99, 0x99, 0xFF],
    [0x99, 0x33, 0x66],
    [0xFF, 0xFF, 0xCC],
    [0xCC, 0xFF, 0xFF],
    [0x66, 0x00, 0x66],
    [0xFF, 0x80, 0x80],
    [0x00, 0x66, 0xCC],
    [0xCC, 0xCC, 0xFF],
    [0x00, 0x00, 0x80],
    [0xFF, 0x00, 0xFF],
    [0xFF, 0xFF, 0x00],
    [0x00, 0xFF, 0xFF],
    [0x80, 0x00, 0x80],
    [0x80, 0x00, 0x00],
    [0x00, 0x80, 0x80],
    [0x00, 0x00, 0xFF],
    [0x00, 0xCC, 0xFF],
    [0xCC, 0xFF, 0xFF],
    [0xCC, 0xFF, 0xCC],
    [0xFF, 0xFF, 0x99],
    [0x99, 0xCC, 0xFF],
    [0xFF, 0x99, 0xCC],
    [0xCC, 0x99, 0xFF],
    [0xFF, 0xCC, 0x99],
    [0x33, 0x66, 0xFF],
    [0x33, 0xCC, 0xCC],
    [0x99, 0xCC, 0x00],
    [0xFF, 0xCC, 0x00],
    [0xFF, 0x99, 0x00],
    [0xFF, 0x66, 0x00],
    [0x66, 0x66, 0x99],
    [0x96, 0x96, 0x96],
    [0x00, 0x33, 0x66],
    [0x33, 0x99, 0x66],
    [0x00, 0x33, 0x00],
    [0x33, 0x33, 0x00],
    [0x99, 0x33, 0x00],
    [0x99, 0x33, 0x66],
    [0x33, 0x33, 0x99],
    [0x33, 0x33, 0x33],
];

/// How a `CT_Color` (§18.3.1.15) names its colour, before resolution.
///
/// Kept as a reference rather than resolved at parse time because two of the
/// four kinds need context this part does not hold: `theme=` needs
/// `xl/theme/theme1.xml`, and `auto=`/`indexed=64` mean "the consumer's
/// default", which differs between a font (black) and a background (white).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColorKind {
    /// `rgb="FFRRGGBB"` — the only self-contained spelling.
    Rgb([u8; 3]),
    /// `theme="n"`, an index into the workbook theme's colour scheme after
    /// the SpreadsheetML swap.
    Theme(u32),
    /// `indexed="n"` into [`INDEXED_PALETTE`] or the file's own override.
    Indexed(u32),
    /// `auto="1"`: whatever the system says.
    Auto,
}

/// A colour reference and its §18.8.19 `tint`, which lightens (positive) or
/// darkens (negative) the resolved base in HSL luminance.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorRef {
    pub kind: ColorKind,
    pub tint: f64,
}

impl ColorRef {
    /// Read a `<color>`/`<fgColor>`/`<bgColor>` element, or `None` when it
    /// names nothing this reader understands.
    ///
    /// Attribute order is the spec's precedence, not the file's: a producer
    /// that writes both `rgb=` and `theme=` means the explicit one.
    fn parse(e: &quick_xml::events::BytesStart<'_>) -> Option<Self> {
        let tint = attr_parse(e, b"tint").unwrap_or(0.0);
        let kind = if let Some(rgb) = attr(e, b"rgb") {
            ColorKind::Rgb(parse_argb(&rgb)?)
        } else if let Some(t) = attr_parse::<u32>(e, b"theme") {
            ColorKind::Theme(t)
        } else if let Some(i) = attr_parse::<u32>(e, b"indexed") {
            ColorKind::Indexed(i)
        } else if attr_bool(e, b"auto", false) {
            ColorKind::Auto
        } else {
            return None;
        };
        Some(ColorRef { kind, tint })
    }
}

/// `"FFRRGGBB"` — and the `"RRGGBB"` some producers write instead. The alpha
/// byte is dropped: SpreadsheetML has no transparency model at the cell level
/// and every corpus value writes `FF`.
fn parse_argb(hex: &str) -> Option<[u8; 3]> {
    let hex = hex.trim();
    let body = match hex.len() {
        8 => &hex[2..],
        6 => hex,
        _ => return None,
    };
    let v = u32::from_str_radix(body, 16).ok()?;
    Some([(v >> 16) as u8, ((v >> 8) & 0xff) as u8, (v & 0xff) as u8])
}

/// §18.18.55's seventeen hatch patterns, carried as the one number a painter
/// without a pattern engine needs: what fraction of the cell the pattern's
/// strokes cover.
///
/// Excel draws each of these as an 8×8 bitmap tiled across the cell, so the
/// coverage is exact for the named greys (`gray125` is one pixel in eight)
/// and for single-direction stripes (one line in four is light, one in two is
/// dark). It is an approximation for the crosshatches, where the two stroke
/// directions overlap: `lightGrid` is taken as two light stripes less their
/// intersection, and the trellises as their dark counterparts. That
/// approximation reaches **no corpus cell** — every one of the 323 hatched
/// cells across 1,248 workbooks is `gray0625` (210), `lightHorizontal` (63),
/// `lightGray` (39), `gray125` (6), `darkGray` (3) or `lightUp` (2), all of
/// which have exact fractions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HatchPattern {
    Gray0625,
    Gray125,
    LightGray,
    MediumGray,
    DarkGray,
    /// One stroke direction, one line in four.
    LightStripe,
    /// One stroke direction, one line in two.
    DarkStripe,
    /// Two stroke directions at one line in four each.
    LightCrosshatch,
    /// Two stroke directions at one line in two each.
    DarkCrosshatch,
    /// A `patternType` this build does not know. Painted at the mid grey a
    /// declared-but-unnamed hatch is closest to, rather than dropped: the file
    /// asked for ink.
    Unknown,
}

impl HatchPattern {
    fn parse(s: &str) -> Self {
        match s {
            "gray0625" => HatchPattern::Gray0625,
            "gray125" => HatchPattern::Gray125,
            "lightGray" => HatchPattern::LightGray,
            "mediumGray" => HatchPattern::MediumGray,
            "darkGray" => HatchPattern::DarkGray,
            "lightHorizontal" | "lightVertical" | "lightDown" | "lightUp" => {
                HatchPattern::LightStripe
            }
            "darkHorizontal" | "darkVertical" | "darkDown" | "darkUp" => HatchPattern::DarkStripe,
            "lightGrid" | "lightTrellis" => HatchPattern::LightCrosshatch,
            "darkGrid" | "darkTrellis" => HatchPattern::DarkCrosshatch,
            _ => HatchPattern::Unknown,
        }
    }

    /// The share of the cell the pattern's strokes cover, in `[0, 1]`.
    ///
    /// A painter with no pattern engine blends `fg` over `bg` by this much and
    /// fills the cell with the result, which is what the tiled bitmap averages
    /// to at any resolution where its 8-pixel period is not resolvable — and
    /// at 150 DPI an 8×8 Excel pattern tile is under 4 px across.
    pub fn coverage(self) -> f32 {
        match self {
            HatchPattern::Gray0625 => 0.0625,
            HatchPattern::Gray125 => 0.125,
            HatchPattern::LightGray | HatchPattern::LightStripe => 0.25,
            HatchPattern::LightCrosshatch => 0.4375,
            HatchPattern::MediumGray | HatchPattern::DarkStripe | HatchPattern::Unknown => 0.5,
            HatchPattern::DarkGray | HatchPattern::DarkCrosshatch => 0.75,
        }
    }
}

/// §18.18.55 `ST_PatternType`, collapsed to what a painter does about it.
///
/// The collapse is measured rather than lazy: of 2,386,621 filled cells in
/// the corpus, **2,386,298 (100.0%) are `solid`** — every other pattern type
/// put together is 323 cells. Those 323 do not get a pattern engine; they get
/// [`HatchPattern::coverage`], which is one blend rather than a tiled bitmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PatternType {
    /// `none`, or no `<patternFill>` at all — the cell shows the sheet
    /// background. Slot 0 of every file.
    #[default]
    None,
    /// `solid`: the whole cell takes `fg`. (Excel's own quirk, kept here so
    /// consumers do not have to know it: a solid fill's colour is `fgColor`,
    /// not `bgColor`.)
    Solid,
    /// A declared hatch (`gray125`, `darkUp`, …), carrying the coverage a
    /// painter blends by. Slot 1 of every file is `gray125` — Excel's "no
    /// fill" placeholder rather than a request for hatching — and blending is
    /// what makes it safe to paint at all: at 12.5% coverage the placeholder's
    /// usual black `fg` lands as the light grey Excel itself shows, where a
    /// solid reading would black out the cell.
    Hatch(HatchPattern),
    /// `<gradientFill>`: a fill with no `patternType` at all. Recorded so a
    /// consumer can tell "gradient we do not paint" from "no fill"; the stops
    /// are not read.
    Gradient,
}

/// One `<fills>` entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fill {
    pub pattern: PatternType,
    /// `<fgColor>` — the colour of a `solid` fill, and of a hatch's strokes.
    pub fg: Option<ColorRef>,
    /// `<bgColor>` — a hatch's background. Ignored by a solid fill.
    pub bg: Option<ColorRef>,
}

impl Fill {
    /// Does this fill put ink down?
    pub fn paints(&self) -> bool {
        matches!(self.pattern, PatternType::Solid | PatternType::Hatch(_))
    }
}

/// §18.18.3 `ST_BorderStyle`.
///
/// The full enum is kept — reading `dashDot` as `none` would erase an edge —
/// but the corpus makes clear what has to be *right*: thin 91.6%, hair 4.3%,
/// medium 3.9% are 99.8% of 42,885,045 declared edges, and everything
/// dashed/dotted together is under 0.05%.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    None,
    Hair,
    Thin,
    Medium,
    Thick,
    Double,
    Dotted,
    Dashed,
    DashDot,
    DashDotDot,
    MediumDashed,
    MediumDashDot,
    MediumDashDotDot,
    SlantDashDot,
}

impl BorderStyle {
    fn parse(s: &str) -> Self {
        match s {
            "hair" => BorderStyle::Hair,
            "thin" => BorderStyle::Thin,
            "medium" => BorderStyle::Medium,
            "thick" => BorderStyle::Thick,
            "double" => BorderStyle::Double,
            "dotted" => BorderStyle::Dotted,
            "dashed" => BorderStyle::Dashed,
            "dashDot" => BorderStyle::DashDot,
            "dashDotDot" => BorderStyle::DashDotDot,
            "mediumDashed" => BorderStyle::MediumDashed,
            "mediumDashDot" => BorderStyle::MediumDashDot,
            "mediumDashDotDot" => BorderStyle::MediumDashDotDot,
            "slantDashDot" => BorderStyle::SlantDashDot,
            _ => BorderStyle::None,
        }
    }

    /// Stroke width in points. Excel draws these in device pixels at 96 DPI —
    /// hair is sub-pixel, thin 1 px, medium 2 px, thick 3 px — so the point
    /// values are those pixel widths at 0.75 pt each.
    pub fn width_pt(self) -> f32 {
        match self {
            BorderStyle::None => 0.0,
            BorderStyle::Hair => 0.5,
            BorderStyle::Thin
            | BorderStyle::Dotted
            | BorderStyle::Dashed
            | BorderStyle::DashDot
            | BorderStyle::DashDotDot
            | BorderStyle::Double => 0.75,
            BorderStyle::Medium
            | BorderStyle::MediumDashed
            | BorderStyle::MediumDashDot
            | BorderStyle::MediumDashDotDot
            | BorderStyle::SlantDashDot => 1.5,
            BorderStyle::Thick => 2.25,
        }
    }

    /// How thick the edge is *including* what it draws around itself.
    ///
    /// The same as [`BorderStyle::width_pt`] for every style but `double`,
    /// which Excel draws as two strokes with a stroke-wide gap between them —
    /// three pen widths of cell, not one. A painter that reserved only
    /// `width_pt` for it would overlap the neighbouring edge's inset, and one
    /// that drew a single stroke would render 27,300 corpus edges (0.1%, and
    /// more than every dashed style put together) as a plain thin line.
    pub fn extent_pt(self) -> f32 {
        match self {
            BorderStyle::Double => self.width_pt() * 3.0,
            _ => self.width_pt(),
        }
    }

    /// The on/off run lengths of a broken edge, in multiples of its own pen
    /// width, or `None` when the edge is continuous.
    ///
    /// Relative to the pen rather than absolute so the `medium*` spellings
    /// come out coarser than the thin ones without a second table, which is
    /// what Excel draws. `slantDashDot`'s slant is not expressible in an
    /// axis-aligned rect and is drawn as its unslanted counterpart — 6 corpus
    /// edges.
    pub fn dash_pattern(self) -> Option<&'static [f32]> {
        match self {
            BorderStyle::Dotted => Some(&[1.0, 1.0]),
            BorderStyle::Dashed | BorderStyle::MediumDashed => Some(&[3.0, 2.0]),
            BorderStyle::DashDot | BorderStyle::MediumDashDot | BorderStyle::SlantDashDot => {
                Some(&[4.0, 2.0, 1.0, 2.0])
            }
            BorderStyle::DashDotDot | BorderStyle::MediumDashDotDot => {
                Some(&[4.0, 2.0, 1.0, 2.0, 1.0, 2.0])
            }
            _ => None,
        }
    }

    /// Is this a broken line? Equivalent to [`BorderStyle::dash_pattern`]
    /// being `Some`, kept as a predicate for callers that only ask the
    /// question.
    pub fn is_dashed(self) -> bool {
        matches!(
            self,
            BorderStyle::Dotted
                | BorderStyle::Dashed
                | BorderStyle::DashDot
                | BorderStyle::DashDotDot
                | BorderStyle::MediumDashed
                | BorderStyle::MediumDashDot
                | BorderStyle::MediumDashDotDot
                | BorderStyle::SlantDashDot
        )
    }

    pub fn paints(self) -> bool {
        !matches!(self, BorderStyle::None)
    }
}

/// One edge of a `<border>`: its style and, when it names one, its colour.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BorderEdge {
    pub style: BorderStyle,
    pub color: Option<ColorRef>,
}

/// One `<borders>` entry.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Border {
    pub left: BorderEdge,
    pub right: BorderEdge,
    pub top: BorderEdge,
    pub bottom: BorderEdge,
    /// The diagonal is drawn only in the directions the flags below name; an
    /// edge with neither flag set is inert.
    pub diagonal: BorderEdge,
    pub diagonal_up: bool,
    pub diagonal_down: bool,
}

impl Border {
    /// Does this border put ink down? The diagonal counts only when a
    /// direction flag says which way it runs.
    pub fn paints(&self) -> bool {
        self.left.style.paints()
            || self.right.style.paints()
            || self.top.style.paints()
            || self.bottom.style.paints()
            || (self.diagonal.style.paints() && (self.diagonal_up || self.diagonal_down))
    }
}

/// Which edge of a [`Border`] an element writes into — the parse-time half of
/// the five named fields, so the reader carries a slot instead of a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeSlot {
    Left,
    Right,
    Top,
    Bottom,
    Diagonal,
}

impl EdgeSlot {
    fn of(self, border: &mut Border) -> &mut BorderEdge {
        match self {
            EdgeSlot::Left => &mut border.left,
            EdgeSlot::Right => &mut border.right,
            EdgeSlot::Top => &mut border.top,
            EdgeSlot::Bottom => &mut border.bottom,
            EdgeSlot::Diagonal => &mut border.diagonal,
        }
    }
}

/// Character formatting from `<fonts>`. Mirrors
/// [`RunProps`](crate::xlsx::text::RunProps), which is the run-level version of
/// the same properties — a cell takes this one, and a rich run overrides it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Font {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub size: Option<f32>,
    pub name: Option<String>,
    /// `<color>`: unresolved, for the reason [`ColorRef`] documents. Absent
    /// means the same thing `auto` does — the consumer's default text colour.
    pub color: Option<ColorRef>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HorizontalAlign {
    /// §18.18.40: numbers right, text left. The default, and the reason a
    /// spreadsheet reads as a table without any explicit alignment at all.
    #[default]
    General,
    Left,
    Center,
    Right,
    Fill,
    Justify,
    CenterContinuous,
    Distributed,
}

/// §18.18.88 `ST_VerticalAlignment`. Excel's default is **bottom**, not top —
/// a one-line label in a tall row sits on the row's floor, and taking top
/// instead lifts every heading in a spreadsheet off its own gridline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerticalAlign {
    Top,
    Center,
    #[default]
    Bottom,
    Justify,
    Distributed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Alignment {
    pub horizontal: HorizontalAlign,
    pub vertical: VerticalAlign,
    pub wrap_text: bool,
    /// Indent steps, each 3 spaces' worth in Excel's rendering. Load-bearing
    /// for structure: indented labels are how spreadsheets express hierarchy.
    pub indent: u32,
    /// Degrees counter-clockwise, or 255 for stacked/vertical text (§18.8.1).
    pub text_rotation: Option<u32>,
}

/// One `<cellXfs>` entry — the record a cell's `s=` points at.
#[derive(Clone, Debug, Default)]
pub struct CellXf {
    pub num_fmt_id: u32,
    pub font_id: u32,
    pub fill_id: u32,
    pub border_id: u32,
    pub alignment: Alignment,
    /// `quotePrefix="1"`: the user typed a leading apostrophe to force text.
    /// The apostrophe is not in the cell value, so it must not be emitted, but
    /// it does mean the value is text no matter what it looks like.
    pub quote_prefix: bool,
}

/// The parsed `xl/styles.xml`.
#[derive(Clone, Debug, Default)]
pub struct Styles {
    /// Workbook-defined codes from `<numFmts>`, keyed by id.
    ///
    /// A custom id may legally redefine a builtin one, and may carry the code
    /// `General`; both are in the corpus, so custom entries are consulted
    /// first and never assumed to be "not General".
    custom_formats: HashMap<u32, String>,
    cell_xfs: Vec<CellXf>,
    fonts: Vec<Font>,
    fills: Vec<Fill>,
    borders: Vec<Border>,
    /// `<indexedColors>` when the file replaces the legacy palette — 4.6% of
    /// corpus workbooks do. `None` means [`INDEXED_PALETTE`] stands.
    indexed_colors: Option<Vec<[u8; 3]>>,
}

impl Styles {
    /// Parse `xl/styles.xml`.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut out = Styles::default();
        let mut reader = Reader::from_reader(data);
        let mut buf = Vec::new();
        // Which array we are inside. `<xf>`, `<font>`, `<fill>` and
        // `<border>` each appear under more than one parent; see the module
        // docs.
        let mut in_cell_xfs = false;
        let mut in_fonts = false;
        let mut in_fills = false;
        let mut in_borders = false;
        let mut in_indexed_colors = false;
        // Inside a `<patternFill>`, where `<fgColor>`/`<bgColor>` live. A
        // `<gradientFill>`'s stops carry `<color>` children too, and they must
        // not be read as the pattern's.
        let mut in_pattern = false;
        let mut current_xf: Option<CellXf> = None;
        let mut current_font: Option<Font> = None;
        let mut current_fill: Option<Fill> = None;
        let mut current_border: Option<Border> = None;
        // The edge element being read, so a nested `<color>` lands on it. The
        // slot is remembered rather than the edge written eagerly, because an
        // edge's colour arrives after its style attribute.
        let mut current_edge: Option<(EdgeSlot, BorderEdge)> = None;

        loop {
            let event = reader
                .read_event_into(&mut buf)
                .map_err(quick_xml::DeError::from)?;
            let (start, empty) = match event {
                Event::Eof => break,
                Event::Start(ref e) => (Some(e), false),
                Event::Empty(ref e) => (Some(e), true),
                Event::End(ref e) => {
                    match local_name(e.name().as_ref()) {
                        b"cellXfs" => in_cell_xfs = false,
                        b"fonts" => in_fonts = false,
                        b"fills" => in_fills = false,
                        b"borders" => in_borders = false,
                        b"indexedColors" => in_indexed_colors = false,
                        b"patternFill" => in_pattern = false,
                        b"xf" => {
                            if let Some(xf) = current_xf.take() {
                                out.cell_xfs.push(xf);
                            }
                        }
                        b"font" => {
                            if let Some(font) = current_font.take() {
                                out.fonts.push(font);
                            }
                        }
                        b"fill" => {
                            if let Some(fill) = current_fill.take() {
                                out.fills.push(fill);
                            }
                        }
                        b"border" => {
                            if let Some(border) = current_border.take() {
                                out.borders.push(border);
                            }
                        }
                        b"left" | b"right" | b"top" | b"bottom" | b"diagonal" => {
                            if let (Some(border), Some((slot, edge))) =
                                (current_border.as_mut(), current_edge.take())
                            {
                                *slot.of(border) = edge;
                            }
                        }
                        _ => {}
                    }
                    buf.clear();
                    continue;
                }
                _ => {
                    buf.clear();
                    continue;
                }
            };
            let Some(e) = start else { unreachable!() };

            match local_name(e.name().as_ref()) {
                b"cellXfs" => in_cell_xfs = !empty,
                b"fonts" => in_fonts = !empty,
                b"fills" => in_fills = !empty,
                b"borders" => in_borders = !empty,
                b"indexedColors" => {
                    in_indexed_colors = !empty;
                    // The override replaces the palette wholesale, so an
                    // empty list is still an override — start it here rather
                    // than on the first `<rgbColor>`.
                    out.indexed_colors = Some(Vec::new());
                }
                b"rgbColor" if in_indexed_colors => {
                    if let (Some(list), Some(rgb)) = (
                        out.indexed_colors.as_mut(),
                        attr(e, b"rgb").as_deref().and_then(parse_argb),
                    ) {
                        list.push(rgb);
                    }
                }
                b"numFmt" => {
                    // `<numFmt>` appears both in the workbook-level `<numFmts>`
                    // and inside `<dxf>`. Collecting both is harmless — they
                    // share one id space and a dxf restates rather than
                    // redefines — so this one is not container-gated.
                    if let (Some(id), Some(code)) =
                        (attr_parse(e, b"numFmtId"), attr(e, b"formatCode"))
                    {
                        out.custom_formats.insert(id, code);
                    }
                }
                b"xf" if in_cell_xfs => {
                    let xf = CellXf {
                        num_fmt_id: attr_parse(e, b"numFmtId").unwrap_or(0),
                        font_id: attr_parse(e, b"fontId").unwrap_or(0),
                        fill_id: attr_parse(e, b"fillId").unwrap_or(0),
                        border_id: attr_parse(e, b"borderId").unwrap_or(0),
                        alignment: Alignment::default(),
                        quote_prefix: attr_bool(e, b"quotePrefix", false),
                    };
                    if empty {
                        out.cell_xfs.push(xf);
                    } else {
                        // A non-empty `<xf>` still has an `<alignment>` child
                        // to read before it can be pushed.
                        current_xf = Some(xf);
                    }
                }
                b"alignment" => {
                    if let Some(xf) = current_xf.as_mut() {
                        xf.alignment = Alignment {
                            horizontal: match attr(e, b"horizontal").as_deref() {
                                Some("left") => HorizontalAlign::Left,
                                Some("center") => HorizontalAlign::Center,
                                Some("right") => HorizontalAlign::Right,
                                Some("fill") => HorizontalAlign::Fill,
                                Some("justify") => HorizontalAlign::Justify,
                                Some("centerContinuous") => HorizontalAlign::CenterContinuous,
                                Some("distributed") => HorizontalAlign::Distributed,
                                _ => HorizontalAlign::General,
                            },
                            vertical: match attr(e, b"vertical").as_deref() {
                                Some("top") => VerticalAlign::Top,
                                Some("center") => VerticalAlign::Center,
                                Some("justify") => VerticalAlign::Justify,
                                Some("distributed") => VerticalAlign::Distributed,
                                _ => VerticalAlign::Bottom,
                            },
                            wrap_text: attr_bool(e, b"wrapText", false),
                            indent: attr_parse(e, b"indent").unwrap_or(0),
                            text_rotation: attr_parse::<u32>(e, b"textRotation")
                                .filter(|&r| r != 0),
                        };
                    }
                }
                b"font" if in_fonts => {
                    let font = Font::default();
                    if empty {
                        out.fonts.push(font);
                    } else {
                        current_font = Some(font);
                    }
                }
                b"fill" if in_fills => {
                    if empty {
                        out.fills.push(Fill::default());
                    } else {
                        current_fill = Some(Fill::default());
                    }
                }
                b"patternFill" if current_fill.is_some() => {
                    if let Some(fill) = current_fill.as_mut() {
                        fill.pattern = match attr(e, b"patternType").as_deref() {
                            Some("solid") => PatternType::Solid,
                            // No `patternType` on a `<patternFill>` means
                            // `none` (§18.8.32), not "unknown".
                            None | Some("none") => PatternType::None,
                            Some(s) => PatternType::Hatch(HatchPattern::parse(s)),
                        };
                    }
                    in_pattern = !empty;
                }
                b"gradientFill" if current_fill.is_some() => {
                    if let Some(fill) = current_fill.as_mut() {
                        fill.pattern = PatternType::Gradient;
                    }
                }
                b"fgColor" | b"bgColor" if in_pattern => {
                    let color = ColorRef::parse(e);
                    if let Some(fill) = current_fill.as_mut() {
                        if local_name(e.name().as_ref()) == b"fgColor" {
                            fill.fg = color;
                        } else {
                            fill.bg = color;
                        }
                    }
                }
                b"border" if in_borders => {
                    let border = Border {
                        diagonal_up: attr_bool(e, b"diagonalUp", false),
                        diagonal_down: attr_bool(e, b"diagonalDown", false),
                        ..Border::default()
                    };
                    if empty {
                        out.borders.push(border);
                    } else {
                        current_border = Some(border);
                    }
                }
                b"left" | b"right" | b"top" | b"bottom" | b"diagonal"
                    if current_border.is_some() =>
                {
                    let slot = match local_name(e.name().as_ref()) {
                        b"left" => EdgeSlot::Left,
                        b"right" => EdgeSlot::Right,
                        b"top" => EdgeSlot::Top,
                        b"bottom" => EdgeSlot::Bottom,
                        _ => EdgeSlot::Diagonal,
                    };
                    let edge = BorderEdge {
                        style: attr(e, b"style")
                            .map_or(BorderStyle::None, |s| BorderStyle::parse(&s)),
                        color: None,
                    };
                    if empty {
                        if let Some(border) = current_border.as_mut() {
                            *slot.of(border) = edge;
                        }
                    } else {
                        current_edge = Some((slot, edge));
                    }
                }
                // A `<color>` inside an edge belongs to that edge; one inside
                // a `<font>` to that font. The two never nest, so the edge
                // wins whenever one is open.
                b"color" => {
                    let color = ColorRef::parse(e);
                    if let Some((_, edge)) = current_edge.as_mut() {
                        edge.color = color;
                    } else if in_fonts && let Some(font) = current_font.as_mut() {
                        font.color = color;
                    }
                }
                name if current_font.is_some() => {
                    let font = current_font.as_mut().expect("checked by the guard");
                    // Same CT_BooleanProperty rule as run properties: an
                    // explicit `val="0"` turns the property off.
                    let on = || !matches!(attr(e, b"val").as_deref(), Some("0" | "false"));
                    match name {
                        b"b" => font.bold = on(),
                        b"i" => font.italic = on(),
                        b"strike" => font.strike = on(),
                        b"u" => {
                            font.underline = !matches!(attr(e, b"val").as_deref(), Some("none"))
                        }
                        b"sz" => font.size = attr_parse(e, b"val"),
                        b"name" => font.name = attr(e, b"val"),
                        _ => {}
                    }
                }
                _ => {}
            }
            buf.clear();
        }
        Ok(out)
    }

    /// The number-format code for a cell's `s=` index.
    ///
    /// Absent `s=`, an out-of-range index, and an unmapped `numFmtId` all
    /// resolve to `General` — the same answer Excel gives, and the reason most
    /// cells in most workbooks need no lookup at all.
    ///
    /// `applyNumberFormat="0"` is deliberately ignored. In the spec it means
    /// "take the format from the linked `<cellStyleXfs>` entry instead", but
    /// every producer in the corpus writes the effective `numFmtId` on the
    /// `<cellXfs>` entry regardless, and honouring the flag would replace a
    /// correct answer with an inherited one.
    pub fn format_code(&self, style_index: Option<u32>) -> &str {
        let Some(xf) = self.cell_xf(style_index) else {
            return GENERAL;
        };
        self.format_code_for_id(xf.num_fmt_id)
    }

    /// The code registered for a `numFmtId`, custom entries winning over
    /// builtins.
    pub fn format_code_for_id(&self, id: u32) -> &str {
        if let Some(code) = self.custom_formats.get(&id) {
            return code;
        }
        builtin_format(id).unwrap_or(GENERAL)
    }

    pub fn cell_xf(&self, style_index: Option<u32>) -> Option<&CellXf> {
        self.cell_xfs.get(style_index.unwrap_or(0) as usize)
    }

    /// The font for a cell's `s=` index, or the default font when anything in
    /// the chain is missing.
    pub fn font(&self, style_index: Option<u32>) -> Font {
        self.cell_xf(style_index)
            .and_then(|xf| self.fonts.get(xf.font_id as usize))
            .cloned()
            .unwrap_or_default()
    }

    /// The fill for a cell's `s=` index. A missing link resolves to "no
    /// fill", which is what slot 0 of every file holds anyway.
    pub fn fill(&self, style_index: Option<u32>) -> Fill {
        self.cell_xf(style_index)
            .and_then(|xf| self.fills.get(xf.fill_id as usize))
            .cloned()
            .unwrap_or_default()
    }

    /// The border for a cell's `s=` index; a missing link is no border.
    pub fn border(&self, style_index: Option<u32>) -> Border {
        self.cell_xf(style_index)
            .and_then(|xf| self.borders.get(xf.border_id as usize))
            .copied()
            .unwrap_or_default()
    }

    /// Resolve a colour reference to RGB, or `None` for "the consumer's
    /// default".
    ///
    /// `None` is returned rather than black for every flavour of *automatic*
    /// — `auto="1"`, `indexed=64`/`65`, an unresolvable theme index — because
    /// the default differs by consumer: automatic text is black, an automatic
    /// background is white, and collapsing them here would paint white text
    /// on white cells. The census makes that concrete: 35.1% of colour-
    /// carrying cells name an automatic colour.
    ///
    /// The theme mapping is the one that is easy to get wrong.
    /// SpreadsheetML's indices are **not** `<a:clrScheme>` document order:
    /// 0↔1 and 2↔3 are swapped, so index 0 is `lt1` (the background) and
    /// index 1 is `dk1` (the text). Painting a file with them transposed
    /// inverts every themed fill.
    pub fn resolve_color(
        &self,
        color: ColorRef,
        theme: Option<&ThemeColorScheme>,
    ) -> Option<[u8; 3]> {
        let base = match color.kind {
            ColorKind::Rgb(rgb) => rgb,
            ColorKind::Auto => return None,
            ColorKind::Indexed(i) => self.indexed_color(i)?,
            ColorKind::Theme(i) => {
                let scheme = theme?;
                let packed = match i {
                    0 => scheme.light1,
                    1 => scheme.dark1,
                    2 => scheme.light2,
                    3 => scheme.dark2,
                    4 => scheme.accent1,
                    5 => scheme.accent2,
                    6 => scheme.accent3,
                    7 => scheme.accent4,
                    8 => scheme.accent5,
                    9 => scheme.accent6,
                    10 => scheme.hyperlink,
                    11 => scheme.followed_hyperlink,
                    _ => return None,
                };
                [
                    (packed >> 16) as u8,
                    ((packed >> 8) & 0xff) as u8,
                    (packed & 0xff) as u8,
                ]
            }
        };
        Some(apply_tint(base, color.tint))
    }

    /// The palette entry an `indexed=` names — the file's own
    /// `<indexedColors>` when it declares one, else [`INDEXED_PALETTE`].
    ///
    /// 64 and 65 (system foreground/background) and anything past the table
    /// are `None`: they are automatic, not stored colours. That pair is
    /// 89.4% of every `indexed=` in the corpus, which is why the palette
    /// itself is a small tail rather than the main path.
    pub fn indexed_color(&self, index: u32) -> Option<[u8; 3]> {
        let i = index as usize;
        match self.indexed_colors.as_deref() {
            // An override shorter than the index falls back to the legacy
            // table rather than to nothing: producers that write a partial
            // `<indexedColors>` mean "these differ", not "the rest are gone".
            Some(list) => list
                .get(i)
                .copied()
                .or_else(|| INDEXED_PALETTE.get(i).copied()),
            None => INDEXED_PALETTE.get(i).copied(),
        }
    }

    /// Does the file replace the legacy palette? 4.6% of corpus workbooks do.
    pub fn overrides_indexed_palette(&self) -> bool {
        self.indexed_colors.is_some()
    }

    pub fn alignment(&self, style_index: Option<u32>) -> Alignment {
        self.cell_xf(style_index)
            .map(|xf| xf.alignment.clone())
            .unwrap_or_default()
    }

    /// Every distinct format code the workbook can reach, for censuses.
    pub fn custom_formats(&self) -> &HashMap<u32, String> {
        &self.custom_formats
    }

    pub fn cell_xf_count(&self) -> usize {
        self.cell_xfs.len()
    }

    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    pub fn fill_count(&self) -> usize {
        self.fills.len()
    }

    pub fn border_count(&self) -> usize {
        self.borders.len()
    }
}

/// §18.8.19 `tint`: shift a colour's HSL luminance toward white (positive) or
/// black (negative), leaving hue and saturation alone.
///
/// The HSL round-trip matters rather than a straight RGB lerp: a themed
/// accent tinted in RGB drifts toward grey as it lightens, and the corpus
/// tints accents constantly — banded table styles are a single accent at four
/// tints, and drift makes the bands read as different colours.
fn apply_tint(base: [u8; 3], tint: f64) -> [u8; 3] {
    if tint == 0.0 || !tint.is_finite() {
        return base;
    }
    let (h, s, l) = rgb_to_hsl(base);
    let tint = tint.clamp(-1.0, 1.0);
    let l = if tint < 0.0 {
        l * (1.0 + tint)
    } else {
        l * (1.0 - tint) + tint
    };
    hsl_to_rgb(h, s, l)
}

fn rgb_to_hsl([r, g, b]: [u8; 3]) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let l = l.clamp(0.0, 1.0);
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return [v, v, v];
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let channel = |t: f64| -> u8 {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: &str = r#"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="2">
    <numFmt numFmtId="164" formatCode="&quot;$&quot;#,##0.00"/>
    <numFmt numFmtId="165" formatCode="General"/>
  </numFmts>
  <fonts count="4">
    <font><sz val="11"/><name val="Calibri"/></font>
    <font><b/><sz val="14"/><name val="Arial"/></font>
    <font><b val="0"/><i/></font>
    <font><color theme="1"/><sz val="11"/></font>
  </fonts>
  <cellStyleXfs count="1">
    <xf numFmtId="9" fontId="1"/>
  </cellStyleXfs>
  <fills count="4">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="gray125"/></fill>
    <fill><patternFill patternType="solid">
      <fgColor rgb="FFFFC000"/><bgColor indexed="64"/>
    </patternFill></fill>
    <fill><patternFill patternType="solid">
      <fgColor theme="4" tint="0.3999755851924192"/><bgColor indexed="65"/>
    </patternFill></fill>
  </fills>
  <borders count="3">
    <border><left/><right/><top/><bottom/><diagonal/></border>
    <border>
      <left style="thin"><color indexed="8"/></left>
      <right style="medium"><color rgb="FF00B050"/></right>
      <top style="hair"/>
      <bottom style="double"><color theme="1"/></bottom>
      <diagonal/>
    </border>
    <border diagonalUp="1"><diagonal style="thin"/></border>
  </borders>
  <cellXfs count="5">
    <xf numFmtId="0" fontId="0" fillId="0" borderId="0"/>
    <xf numFmtId="164" fontId="1" fillId="2" borderId="1" applyNumberFormat="1"/>
    <xf numFmtId="3" fontId="2" quotePrefix="1"/>
    <xf numFmtId="0" fontId="0"><alignment horizontal="center" vertical="top" wrapText="1" indent="2"/></xf>
    <xf numFmtId="0" fontId="3" fillId="3" borderId="2"/>
  </cellXfs>
  <dxfs count="1"><dxf>
    <font><b/><color rgb="FF9C0006"/></font>
    <fill><patternFill patternType="solid"><fgColor rgb="FFFFC7CE"/></patternFill></fill>
    <border><left style="thick"/></border>
  </dxf></dxfs>
  <colors><indexedColors>
    <rgbColor rgb="00FF0000"/>
    <rgbColor rgb="0000FF00"/>
  </indexedColors></colors>
</styleSheet>"#;

    fn styles() -> Styles {
        Styles::parse(STYLES.as_bytes()).unwrap()
    }

    /// The headline trap. `<cellStyleXfs>` holds an `<xf numFmtId="9">` (a
    /// percent format) directly above `<cellXfs>`. If it leaked in, every
    /// `s=` index would be shifted by one and cell styles would silently
    /// resolve to their neighbours'.
    #[test]
    fn cell_style_xfs_do_not_leak_into_cell_xfs() {
        let s = styles();
        assert_eq!(s.cell_xf_count(), 5);
        assert_eq!(s.format_code(Some(0)), "General");
        assert_eq!(s.format_code(Some(1)), "\"$\"#,##0.00");
        assert_eq!(s.format_code(Some(2)), "#,##0");
        // The percent code from cellStyleXfs must appear nowhere.
        assert!((0..4).all(|i| s.format_code(Some(i)) != "0%"));
    }

    /// `<dxfs>` holds `<font>` elements too; counting them shifts every
    /// `fontId` above the insertion point.
    #[test]
    fn dxf_fonts_do_not_shift_the_font_ids() {
        let s = styles();
        assert_eq!(s.font_count(), 4);
        assert_eq!(s.font(Some(1)).name.as_deref(), Some("Arial"));
        assert!(s.font(Some(1)).bold);
    }

    #[test]
    fn absent_and_out_of_range_style_indices_are_general() {
        let s = styles();
        assert_eq!(s.format_code(None), GENERAL);
        assert_eq!(s.format_code(Some(999)), GENERAL);
        assert_eq!(s.font(Some(999)), Font::default());
    }

    /// A custom id may carry the code `General` — a reader that assumed
    /// "id >= 164 means non-General" would format these cells wrongly.
    #[test]
    fn a_custom_format_may_be_general() {
        assert_eq!(styles().format_code_for_id(165), "General");
    }

    #[test]
    fn format_code_attributes_are_unescaped() {
        // `"$"#,##0.00` arrives as `&quot;$&quot;#,##0.00`; leaving the
        // entities in would hand the formatter a code it cannot parse.
        assert!(styles().format_code_for_id(164).starts_with('"'));
    }

    #[test]
    fn font_boolean_off_switch_is_honoured() {
        let f = styles().font(Some(2));
        assert!(!f.bold, "b val=0 must turn bold off");
        assert!(f.italic);
    }

    #[test]
    fn alignment_is_read_from_the_xf_child() {
        let a = styles().alignment(Some(3));
        assert_eq!(a.horizontal, HorizontalAlign::Center);
        assert_eq!(a.vertical, VerticalAlign::Top);
        assert!(a.wrap_text);
        assert_eq!(a.indent, 2);
        // An xf with no <alignment> child gets the default, not the previous
        // xf's — the classic streaming-parser carry-over bug.
        assert_eq!(styles().alignment(Some(0)), Alignment::default());
        // §18.18.88's default is bottom, and a cell that states no
        // `vertical=` must resolve to it rather than to top.
        assert_eq!(styles().alignment(Some(0)).vertical, VerticalAlign::Bottom);
    }

    #[test]
    fn quote_prefix_is_captured() {
        assert!(styles().cell_xf(Some(2)).unwrap().quote_prefix);
        assert!(!styles().cell_xf(Some(0)).unwrap().quote_prefix);
    }

    #[test]
    fn a_workbook_with_no_styles_part_still_resolves() {
        let s = Styles::default();
        assert_eq!(s.format_code(Some(7)), GENERAL);
        assert_eq!(s.font(Some(7)), Font::default());
    }

    /// A theme scheme with distinguishable slots, so a transposed index is a
    /// wrong *colour* rather than a wrong shade.
    fn theme() -> ThemeColorScheme {
        ThemeColorScheme {
            dark1: 0x00_00_00,
            light1: 0xFF_FF_FF,
            dark2: 0x44_54_6A,
            light2: 0xE7_E6_E6,
            accent1: 0x44_72_C4,
            accent2: 0xED_7D_31,
            accent3: 0xA5_A5_A5,
            accent4: 0xFF_C0_00,
            accent5: 0x5B_9B_D5,
            accent6: 0x70_AD_47,
            hyperlink: 0x05_63_C1,
            followed_hyperlink: 0x95_4F_72,
        }
    }

    /// `<dxfs>` holds `<fill>` and `<border>` children as well as `<font>`;
    /// counting them shifts every `fillId`/`borderId` above the insertion
    /// point, the same silent failure the font list has.
    #[test]
    fn dxf_fills_and_borders_do_not_shift_the_ids() {
        let s = styles();
        assert_eq!(s.fill_count(), 4, "the dxf's solid fill must not be here");
        assert_eq!(s.border_count(), 3, "nor its thick left edge");
        // Fill 2 is the yellow one; the dxf's pink would land at 4.
        assert_eq!(
            s.fill(Some(1)).fg.map(|c| c.kind),
            Some(ColorKind::Rgb([0xFF, 0xC0, 0x00]))
        );
    }

    #[test]
    fn a_solid_fill_takes_its_colour_from_fg_not_bg() {
        let fill = styles().fill(Some(1));
        assert_eq!(fill.pattern, PatternType::Solid);
        assert!(fill.paints());
        // `bgColor indexed="64"` is the automatic sentinel that accompanies
        // nearly every solid fill; reading it as the fill colour would paint
        // the sheet in system background.
        assert_eq!(fill.bg.map(|c| c.kind), Some(ColorKind::Indexed(64)));
        assert_eq!(styles().resolve_color(fill.bg.unwrap(), None), None);
    }

    #[test]
    fn the_placeholder_patterns_do_not_paint() {
        let s = styles();
        // Slot 0 (`none`) is genuinely nothing; slot 1 (`gray125`) is Excel's
        // boilerplate, present in every file and used by almost none.
        assert!(!s.fill(Some(0)).paints());
        assert_eq!(s.fill(Some(0)).pattern, PatternType::None);
    }

    #[test]
    fn border_edges_keep_their_style_and_their_own_colour() {
        let b = styles().border(Some(1));
        assert_eq!(b.left.style, BorderStyle::Thin);
        assert_eq!(b.left.color.map(|c| c.kind), Some(ColorKind::Indexed(8)));
        assert_eq!(b.right.style, BorderStyle::Medium);
        assert_eq!(
            b.right.color.map(|c| c.kind),
            Some(ColorKind::Rgb([0x00, 0xB0, 0x50]))
        );
        // An edge with a style and no `<color>` child is still an edge.
        assert_eq!(b.top.style, BorderStyle::Hair);
        assert_eq!(b.top.color, None);
        assert_eq!(b.bottom.style, BorderStyle::Double);
        assert_eq!(b.bottom.color.map(|c| c.kind), Some(ColorKind::Theme(1)));
        // A colour must not leak from one edge onto the next.
        assert_eq!(b.diagonal, BorderEdge::default());
        assert!(b.paints());
    }

    /// A diagonal with no direction flag draws nothing — Excel stores the
    /// style and the direction separately.
    #[test]
    fn a_diagonal_needs_a_direction_to_paint() {
        let b = styles().border(Some(4));
        assert_eq!(b.diagonal.style, BorderStyle::Thin);
        assert!(b.diagonal_up && !b.diagonal_down);
        assert!(b.paints());
        let inert = Border {
            diagonal: BorderEdge {
                style: BorderStyle::Thin,
                color: None,
            },
            ..Border::default()
        };
        assert!(!inert.paints());
    }

    #[test]
    fn a_font_colour_is_read_and_does_not_leak_between_fonts() {
        let s = styles();
        assert_eq!(
            s.font(Some(4)).color.map(|c| c.kind),
            Some(ColorKind::Theme(1))
        );
        // The dxf font's red must not have been appended, and an uncoloured
        // font must not inherit the previous one's colour.
        assert_eq!(s.font(Some(0)).color, None);
        assert_eq!(s.font(Some(1)).color, None);
    }

    /// The headline colour trap: SpreadsheetML theme indices are not
    /// `clrScheme` order. Index 0 is the *background*, index 1 the text.
    #[test]
    fn theme_indices_are_swapped_against_the_colour_scheme() {
        let s = styles();
        let at = |i: u32| {
            s.resolve_color(
                ColorRef {
                    kind: ColorKind::Theme(i),
                    tint: 0.0,
                },
                Some(&theme()),
            )
        };
        assert_eq!(at(0), Some([0xFF, 0xFF, 0xFF]), "0 is lt1, not dk1");
        assert_eq!(at(1), Some([0x00, 0x00, 0x00]), "1 is dk1, not lt1");
        assert_eq!(at(2), Some([0xE7, 0xE6, 0xE6]), "2 is lt2");
        assert_eq!(at(3), Some([0x44, 0x54, 0x6A]), "3 is dk2");
        assert_eq!(at(4), Some([0x44, 0x72, 0xC4]), "4 is accent1");
        assert_eq!(at(9), Some([0x70, 0xAD, 0x47]), "9 is accent6");
        assert_eq!(at(10), Some([0x05, 0x63, 0xC1]), "10 is hlink");
        assert_eq!(at(12), None, "past the scheme is automatic, not black");
    }

    /// Without the theme part, a themed colour is *automatic* rather than
    /// black — resolving it to black would paint white-on-black cells in the
    /// 0.6% of workbooks that ship no theme.
    #[test]
    fn a_themed_colour_with_no_theme_is_automatic() {
        assert_eq!(
            styles().resolve_color(
                ColorRef {
                    kind: ColorKind::Theme(4),
                    tint: 0.0
                },
                None
            ),
            None
        );
    }

    /// "Accent1, Lighter 40%" — the single most common tint in the corpus,
    /// since it is what every banded table style is built from. Excel
    /// renders `4472C4` at `tint=0.4` as `8EA9DB`.
    ///
    /// Matched to ±1 per channel rather than exactly: Excel converts through
    /// an *integer* HLS space (luminance in 0..240 steps), so its answer is
    /// its own rounding of the same curve. The tolerance is on the
    /// quantization, not on the algorithm — an RGB lerp lands 6 off on the
    /// blue channel here and drifts further as saturation rises, which is
    /// what makes a four-tint band read as four different colours.
    #[test]
    fn tint_lightens_in_hsl() {
        let s = styles();
        let tinted = |t: f64| {
            s.resolve_color(
                ColorRef {
                    kind: ColorKind::Theme(4),
                    tint: t,
                },
                Some(&theme()),
            )
            .unwrap()
        };
        let near = |got: [u8; 3], want: [u8; 3]| {
            assert!(
                (0..3).all(|i| got[i].abs_diff(want[i]) <= 1),
                "{got:02X?} is not within a rounding step of {want:02X?}"
            );
        };
        near(tinted(0.3999755851924192), [0x8E, 0xA9, 0xDB]);
        // Excel's "Accent1, Darker 25%", which this path reproduces exactly.
        assert_eq!(tinted(-0.249977111117893), [0x2F, 0x55, 0x97]);
        // Tint 0 must be the untouched base — no round-trip drift.
        assert_eq!(tinted(0.0), [0x44, 0x72, 0xC4]);
    }

    /// 93.2% of `indexed=` is 64/65/8 — the automatic sentinels and black.
    /// Resolving 64 to palette-position-64 (which does not exist) or to
    /// black would repaint a third of the corpus's colour-carrying cells.
    #[test]
    fn the_automatic_indexed_sentinels_resolve_to_nothing() {
        let s = styles();
        assert_eq!(s.indexed_color(64), None);
        assert_eq!(s.indexed_color(65), None);
        assert_eq!(s.indexed_color(999), None);
    }

    /// The file's own `<indexedColors>` wins where it defines an entry, and
    /// the legacy table carries the rest.
    #[test]
    fn an_indexed_colours_override_replaces_the_palette_entries_it_defines() {
        let s = styles();
        assert!(s.overrides_indexed_palette());
        assert_eq!(s.indexed_color(0), Some([0xFF, 0x00, 0x00]));
        assert_eq!(s.indexed_color(1), Some([0x00, 0xFF, 0x00]));
        // Past the override, the vendored palette still answers.
        assert_eq!(s.indexed_color(22), Some([0xC0, 0xC0, 0xC0]));
        // A file with no `<colors>` gets the palette unchanged.
        let plain = Styles::default();
        assert!(!plain.overrides_indexed_palette());
        assert_eq!(plain.indexed_color(0), Some([0x00, 0x00, 0x00]));
    }

    #[test]
    fn rgb_is_read_with_or_without_its_alpha_byte() {
        assert_eq!(parse_argb("FF00B050"), Some([0x00, 0xB0, 0x50]));
        assert_eq!(parse_argb("00B050"), Some([0x00, 0xB0, 0x50]));
        assert_eq!(parse_argb("nope"), None);
    }

    /// The three styles that are 99.8% of the corpus's edges must differ in
    /// width, or every table reads as one weight.
    #[test]
    fn border_widths_separate_the_three_common_styles() {
        assert!(BorderStyle::Hair.width_pt() < BorderStyle::Thin.width_pt());
        assert!(BorderStyle::Thin.width_pt() < BorderStyle::Medium.width_pt());
        assert!(BorderStyle::Medium.width_pt() < BorderStyle::Thick.width_pt());
        assert!(!BorderStyle::None.paints());
        assert!(BorderStyle::DashDot.is_dashed() && !BorderStyle::Thin.is_dashed());
        // An unknown style must be inert rather than a default-width line.
        assert_eq!(BorderStyle::parse("mediumWavy"), BorderStyle::None);
    }

    #[test]
    fn a_gradient_fill_is_recorded_but_not_a_pattern() {
        let s = Styles::parse(
            br#"<styleSheet><fills count="1"><fill><gradientFill degree="90">
              <stop position="0"><color rgb="FFFF0000"/></stop>
              <stop position="1"><color rgb="FF0000FF"/></stop>
            </gradientFill></fill></fills>
            <cellXfs count="1"><xf fillId="0"/></cellXfs></styleSheet>"#,
        )
        .unwrap();
        let fill = s.fill(Some(0));
        assert_eq!(fill.pattern, PatternType::Gradient);
        // The stops' `<color>` children must not be mistaken for the
        // pattern's fg/bg.
        assert_eq!(fill.fg, None);
        assert_eq!(fill.bg, None);
    }

    #[test]
    fn builtin_table_omits_reserved_ids() {
        assert_eq!(builtin_format(14), Some("mm-dd-yy"));
        assert_eq!(builtin_format(49), Some("@"));
        // Reserved / locale-dependent: no code may be invented for these.
        assert_eq!(builtin_format(23), None);
        assert_eq!(builtin_format(42), None);
    }
}
