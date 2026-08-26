//! Pure data types for section layout.

use super::super::draw_command::{LayoutedPage, ResolvedEffect, ResolvedFill, ResolvedStroke};
use super::super::fragment::Fragment;
use super::super::paragraph::ParagraphStyle;
use super::super::table::TableRowInput;
use crate::model::WrapText;
use crate::model::dimension::{Dimension, SixtieThousandthDeg};
use crate::render::dimension::Pt;
use crate::render::geometry::PtSize;
use crate::render::resolve::images::MediaEntry;
use crate::render::resolve::shape_geometry::SubPath;

/// Layout-resolved text wrap mode for a floating drawing.
///
/// Derived from the model's `TextWrap` (§20.4.2.14-18) — carries the
/// per-line side constraint (`wrap_text`) but strips distances, which are
/// baked into the float's resolved page-coordinate rectangle before
/// registration.
///
/// `Tight` and `Through` currently take `Square`'s path exactly — registered
/// as an active float ([`WrapMode::registers_as_wrap_float`]) and narrowing
/// each line by the float's bounding rect and the side constraint, not by the
/// polygon those two modes name. Polygon-aware line fitting is not implemented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMode {
    /// §20.4.2.15 wrapNone — no reflow; drawing paints over or under text.
    None,
    /// §20.4.2.17 wrapSquare — text wraps around drawing's bounding box.
    Square(WrapText),
    /// §20.4.2.16 wrapTight — text follows drawing's polygon outline.
    Tight(WrapText),
    /// §20.4.2.14 wrapThrough — text flows through polygon interior.
    Through(WrapText),
    /// §20.4.2.18 wrapTopAndBottom — text stops at drawing top and
    /// resumes below drawing bottom.
    TopAndBottom,
}

impl WrapMode {
    /// Construct a layout wrap mode from a parsed `TextWrap`.
    pub fn from_model(wrap: &crate::model::TextWrap) -> Self {
        use crate::model::TextWrap as T;
        match wrap {
            T::None => Self::None,
            T::Square { wrap_text, .. } => Self::Square(*wrap_text),
            T::Tight { wrap_text, .. } => Self::Tight(*wrap_text),
            T::Through { wrap_text, .. } => Self::Through(*wrap_text),
            T::TopAndBottom { .. } => Self::TopAndBottom,
        }
    }

    /// Whether the drawing participates in wrap-around text flow (i.e.,
    /// should be registered as an active float). `None` and
    /// `TopAndBottom` don't — None is purely overlay; TopAndBottom is
    /// handled by advancing `cursor_y` past the drawing on emit.
    pub fn registers_as_wrap_float(self) -> bool {
        matches!(self, Self::Square(_) | Self::Tight(_) | Self::Through(_))
    }

    /// Side constraint for line narrowing. Returns `WrapText::BothSides`
    /// for modes that don't have one (None / TopAndBottom).
    pub fn wrap_text(self) -> WrapText {
        match self {
            Self::Square(w) | Self::Tight(w) | Self::Through(w) => w,
            Self::None | Self::TopAndBottom => WrapText::BothSides,
        }
    }
}

/// §17.4.58: positioning data for a floating table.
#[derive(Debug, Clone)]
pub struct TableFloatInfo {
    /// Gap between the table's right edge and surrounding text.
    pub right_gap: Pt,
    /// Gap between the table's bottom edge and surrounding text.
    pub bottom_gap: Pt,
    /// §17.4.58: horizontal alignment override (tblpXSpec).
    pub x_align: Option<crate::model::TableXAlign>,
    /// §17.4.58: absolute Y offset from the vertical anchor.
    pub y_offset: Pt,
    /// §17.4.58: vertical anchor reference (text / margin / page).
    pub vert_anchor: crate::model::TableAnchor,
    /// §17.4.57 `<w:tblOverlap>` — when present and set to `Never`,
    /// the layout shifts this table down past prior floating tables
    /// on the same page rather than letting them overlap. `None` /
    /// `Some(Overlap)` mean overlap is permitted (the §17.4.57
    /// default).
    pub overlap: Option<crate::model::TableOverlap>,
}

/// A floating (anchor) image to be positioned absolutely on the page.
#[derive(Clone)]
pub struct FloatingImage {
    pub image_data: MediaEntry,
    pub size: PtSize,
    /// §20.1.10.48 `a:srcRect` — fractional source crop in `[0, 1]`.
    pub src_rect: Option<crate::render::geometry::PtRect>,
    /// Horizontal position, possibly deferred to the page's parity.
    pub x: FloatingImageX,
    /// Resolved absolute y position on the page (may be relative to paragraph).
    pub y: FloatingImageY,
    /// §20.4.2.14-18: text wrap mode (drives float registration + cursor advance).
    pub wrap_mode: WrapMode,
    /// §20.4.2.3 distL/distR: horizontal distance from surrounding text.
    pub dist_left: Pt,
    pub dist_right: Pt,
    /// §20.4.2.3 @behindDoc: image is painted behind document text.
    pub behind_doc: bool,
}

impl FloatingImage {
    /// §20.4.2.18 convenience — true when `wrap_mode` is `TopAndBottom`.
    pub fn is_wrap_top_and_bottom(&self) -> bool {
        matches!(self.wrap_mode, WrapMode::TopAndBottom)
    }
}

/// Vertical position for a floating image.
#[derive(Clone, Copy)]
pub enum FloatingImageY {
    /// Absolute page position.
    Absolute(Pt),
    /// Relative to the paragraph's y position (offset added to cursor_y).
    RelativeToParagraph(Pt),
}

/// §20.4.3.1: which side of a two-sided document a page falls on.
///
/// Keyed on the **logical** page number — the one `w:pgNumType/@start` shifts,
/// and the one §17.10.6 `evenAndOddHeaders` already selects headers by. Sharing
/// the key keeps a document self-consistent: the page that gets the "even"
/// header mirrors the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageParity {
    Odd,
    Even,
}

impl PageParity {
    /// From a 1-based logical page number.
    pub fn of_page(logical_page_number: usize) -> Self {
        if logical_page_number.is_multiple_of(2) {
            Self::Even
        } else {
            Self::Odd
        }
    }
}

/// Horizontal position for a floating object.
///
/// `Absolute` is settled during build. `PageParity` cannot be: §20.4.3.1
/// `inside`/`outside` — both as an alignment and as the `insideMargin` /
/// `outsideMargin` references — mirror according to the page the object lands
/// on, and floats are extracted *before* pagination. So the position is carried
/// as both readings and resolved when a page is assigned.
///
/// This mirrors [`FloatingImageY::RelativeToParagraph`], which exists for the
/// same reason on the other axis: a coordinate the build phase cannot know is
/// deferred, not guessed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FloatingImageX {
    /// The same x on every page.
    Absolute(Pt),
    /// Mirrored — which reading applies depends on the page's [`PageParity`].
    PageParity { odd: Pt, even: Pt },
}

impl FloatingImageX {
    /// Build from the two per-parity readings, collapsing to `Absolute` when
    /// they agree.
    ///
    /// They agree for every anchor that is not `inside`/`outside`, which is
    /// nearly all of them — so a document with no mirrored anchor carries no
    /// deferral at all, and the resolver below is a no-op on it.
    pub fn from_pages(odd: Pt, even: Pt) -> Self {
        if odd == even {
            Self::Absolute(odd)
        } else {
            Self::PageParity { odd, even }
        }
    }

    /// The x this object takes on a page of the given parity.
    pub fn resolve(self, parity: PageParity) -> Pt {
        match self {
            Self::Absolute(x) => x,
            Self::PageParity { odd, even } => match parity {
                PageParity::Odd => odd,
                PageParity::Even => even,
            },
        }
    }
}

/// A floating (anchor) DrawingML shape to be positioned absolutely on the
/// page. The geometry is already evaluated into path-local Pt subpaths; the
/// painter applies origin + rotation + flip.
///
/// Parallels `FloatingImage` — the stacker treats them the same way for
/// placement, differing only in the emitted `DrawCommand` variant.
#[derive(Clone)]
pub struct FloatingShape {
    /// Horizontal position of the shape's top-left, possibly deferred to the
    /// page's parity.
    pub x: FloatingImageX,
    /// Resolved absolute y position on the page.
    pub y: FloatingImageY,
    /// Shape bounding-box size in Pt.
    pub size: PtSize,
    /// §20.1.7.6 @rot — rotation around the shape's center, in 60000ths
    /// of a degree.
    pub rotation: Dimension<SixtieThousandthDeg>,
    /// §20.1.7.6 @flipH — mirror horizontally.
    pub flip_h: bool,
    /// §20.1.7.6 @flipV — mirror vertically.
    pub flip_v: bool,
    /// §20.4.2.14-18: text wrap mode.
    pub wrap_mode: WrapMode,
    /// §20.4.2.3 distL/distR — horizontal distance from surrounding text.
    pub dist_left: Pt,
    pub dist_right: Pt,
    /// Path subpaths in shape-local Pt (already scaled into `size`).
    pub paths: Vec<SubPath>,
    /// Resolved fill.
    pub fill: ResolvedFill,
    /// Optional resolved stroke.
    pub stroke: Option<ResolvedStroke>,
    /// Resolved post-processing effects (the painter may defer them).
    pub effects: Vec<ResolvedEffect>,
    /// §17.17.1 / §20.1.2.1.1: pre-laid-out commands for the shape's
    /// text-box content (`wps:wsp/wps:txbx/w:txbxContent`). Each
    /// command is in shape-local coordinates; the stacker shifts
    /// them by the shape's resolved origin (plus body insets, pre-
    /// applied during sub-layout) and emits them *after* the shape's
    /// path so the text overlays the fill. Empty for shapes without
    /// text-box content or whose anchor model isn't supported by the
    /// sub-layout code yet (page-anchored vertical positions).
    pub text_commands: Vec<crate::render::layout::draw_command::DrawCommand>,
}

impl FloatingShape {
    /// §20.4.2.18 convenience — true when wrap_mode is `TopAndBottom`.
    pub fn is_wrap_top_and_bottom(&self) -> bool {
        matches!(self.wrap_mode, WrapMode::TopAndBottom)
    }
}

/// A block ready for layout — either a paragraph or a table.
///
/// The `Paragraph` variant is intentionally larger than `Table` (it carries
/// fragments, floats, and resolved style inline). Boxing would add an
/// allocation per block without a measurable benefit for this codebase,
/// where the Vec holding these is the dominant allocation.
#[allow(clippy::large_enum_variant)]
pub enum LayoutBlock {
    Paragraph {
        fragments: Vec<Fragment>,
        style: ParagraphStyle,
        /// §17.3.1.23: force a page break before this paragraph.
        page_break_before: bool,
        /// Footnotes referenced in this paragraph — rendered at page bottom.
        footnotes: Vec<(Vec<Fragment>, ParagraphStyle)>,
        /// §20.4.2.3: floating images anchored to this paragraph.
        floating_images: Vec<FloatingImage>,
        /// §14.5 / §20.1.2.2.35: floating DrawingML shapes anchored to this paragraph.
        floating_shapes: Vec<FloatingShape>,
    },
    Table {
        rows: Vec<TableRowInput>,
        /// Grid slot widths, already shrunk by `cell_spacing` so the slots plus
        /// one spacing sum to the table's own width.
        col_widths: Vec<Pt>,
        /// §17.4.44 `tblCellSpacing` resolved to points; zero when unset.
        cell_spacing: Pt,
        /// §17.4.38: resolved table border configuration.
        border_config: Option<super::super::table::TableBorderConfig>,
        /// §17.4.51: table indentation from left margin.
        indent: Pt,
        /// §17.4.28: table horizontal alignment.
        alignment: Option<crate::model::Alignment>,
        /// §17.4.58: floating table positioning — if present, text wraps around it.
        float_info: Option<TableFloatInfo>,
        /// §17.4.38: table style reference for adjacent table border collapse.
        style_id: Option<crate::model::StyleId>,
    },
}

/// §17.6.22: continuation state for `Continuous` section breaks.
/// Allows a new section to continue on the current page.
pub struct ContinuationState {
    pub page: LayoutedPage,
    pub cursor_y: Pt,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TextWrap;
    use crate::model::dimension::Emu;
    use crate::model::geometry::EdgeInsets;

    fn insets() -> EdgeInsets<Emu> {
        EdgeInsets::new(
            Dimension::new(0),
            Dimension::new(0),
            Dimension::new(0),
            Dimension::new(0),
        )
    }

    /// The model → layout seam must be **total**: every §20.4.2.14-18 wrap
    /// type maps to a distinct `WrapMode`, and the per-line side constraint
    /// survives the conversion (it is the only part of `TextWrap` layout
    /// still needs — distances are baked into the resolved rect).
    #[test]
    fn from_model_maps_every_wrap_type() {
        assert_eq!(WrapMode::from_model(&TextWrap::None), WrapMode::None);
        assert_eq!(
            WrapMode::from_model(&TextWrap::Square {
                distance: insets(),
                wrap_text: WrapText::Left,
            }),
            WrapMode::Square(WrapText::Left),
        );
        assert_eq!(
            WrapMode::from_model(&TextWrap::Tight {
                distance: insets(),
                wrap_text: WrapText::Right,
                polygon: None,
            }),
            WrapMode::Tight(WrapText::Right),
        );
        assert_eq!(
            WrapMode::from_model(&TextWrap::Through {
                distance: insets(),
                wrap_text: WrapText::Largest,
                polygon: None,
            }),
            WrapMode::Through(WrapText::Largest),
        );
        assert_eq!(
            WrapMode::from_model(&TextWrap::TopAndBottom {
                distance_top: Dimension::new(0),
                distance_bottom: Dimension::new(0),
            }),
            WrapMode::TopAndBottom,
        );
    }

    /// The predicate that decides whether a drawing narrows surrounding text.
    /// §20.4.2.15 `wrapNone` is a pure overlay and §20.4.2.18 `wrapTopAndBottom`
    /// is a block spacer — neither participates. Callers that bypassed this and
    /// used a bare `else` instead let `wrapNone` images reflow text.
    #[test]
    fn only_wrap_enabled_modes_register_as_floats() {
        for mode in [
            WrapMode::Square(WrapText::BothSides),
            WrapMode::Tight(WrapText::BothSides),
            WrapMode::Through(WrapText::BothSides),
        ] {
            assert!(mode.registers_as_wrap_float(), "{mode:?} narrows text");
        }
        for mode in [WrapMode::None, WrapMode::TopAndBottom] {
            assert!(
                !mode.registers_as_wrap_float(),
                "{mode:?} must not narrow text"
            );
        }
    }

    /// The side constraint round-trips for wrap-enabled modes; the modes that
    /// have none report `BothSides`. That fallback is only meaningful because
    /// non-registering modes never reach a line-narrowing call site — see
    /// `only_wrap_enabled_modes_register_as_floats`.
    #[test]
    fn wrap_text_round_trips_and_defaults_to_both_sides() {
        for side in [
            WrapText::BothSides,
            WrapText::Left,
            WrapText::Right,
            WrapText::Largest,
        ] {
            assert_eq!(WrapMode::Square(side).wrap_text(), side);
            assert_eq!(WrapMode::Tight(side).wrap_text(), side);
            assert_eq!(WrapMode::Through(side).wrap_text(), side);
        }
        assert_eq!(WrapMode::None.wrap_text(), WrapText::BothSides);
        assert_eq!(WrapMode::TopAndBottom.wrap_text(), WrapText::BothSides);
    }
}
