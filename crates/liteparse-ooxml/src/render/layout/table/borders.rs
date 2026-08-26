use crate::render::dimension::Pt;
use crate::render::geometry::PtRect;

use super::types::{
    CellBorderOverride, TableBorderConfig, TableBorderLine, TableBorderStyle, TableCellInput,
};
use crate::render::layout::draw_command::DrawCommand;

/// One cell edge during and after §17.4.38 resolution.
///
/// Three states rather than `Option<TableBorderLine>`, because [MS-OI29500]
/// §17.4.66 distinguishes "nothing said about this edge" from "declared
/// `val="nil"`". The difference is about **inheritance**: an omitted or `none`
/// edge falls back to the table style, then `tblPrEx`, then `tblBorders`; `nil`
/// declines that fallback and stays empty. It does *not* outrank the facing
/// cell — `nil` removes this cell's border and nothing else (see
/// [`resolve_border_conflict`]).
///
/// The distinction survives resolution for one reader: the page-split
/// top-border restore in `emit.rs` may revive an `Absent` top but must not
/// revive a `Suppressed` one. For painting they are identical, which is what
/// [`CellEdge::line`] expresses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum CellEdge {
    /// Nothing said about this edge — or it was declared `val="none"`, which
    /// §17.4.66 treats identically. Inherits, then yields.
    Absent,
    /// Declared `val="nil"`: no border here, and no inheritance either.
    Suppressed,
    /// A border to resolve against the opposing edge, and paint if it wins.
    Line(TableBorderLine),
}

impl CellEdge {
    /// The line to paint, if any. Both `Absent` and `Suppressed` paint nothing.
    pub(super) fn line(self) -> Option<TableBorderLine> {
        match self {
            Self::Line(l) => Some(l),
            Self::Absent | Self::Suppressed => None,
        }
    }

    /// Whether two *resolved* edges paint the same thing.
    ///
    /// Not `==`: by this point the `Absent`/`Suppressed` distinction is not
    /// observable to the painter, and letting it in would split a run of columns
    /// that paints one continuous line. Callers asking "can one cell draw this
    /// whole span in a single stroke?" mean *this* question.
    pub(super) fn paints_same(self, other: Self) -> bool {
        self.line() == other.line()
    }
}

impl From<Option<TableBorderLine>> for CellEdge {
    /// Table-level borders have no way to express `nil` — an edge is either
    /// configured or not — so an absent one is `Absent`, never `Suppressed`.
    fn from(b: Option<TableBorderLine>) -> Self {
        match b {
            Some(l) => Self::Line(l),
            None => Self::Absent,
        }
    }
}

/// Resolved borders for one cell.
#[derive(Clone)]
pub(super) struct CellBorders {
    pub(super) top: CellEdge,
    pub(super) bottom: CellEdge,
    pub(super) left: CellEdge,
    pub(super) right: CellEdge,
}

/// §17.4.38 / §17.7.6: resolve effective borders for a cell.
/// Per-cell borders (from conditional formatting) override table-level borders.
/// Table-level insideH/insideV are mapped to cell edges based on position.
///
/// `cell_grid_col` is the cell's absolute starting grid column (accounting
/// for the row's `gridBefore`); `cell_grid_span` is its `gridSpan` (≥1);
/// `num_grid_cols` is the table-wide grid column count. Together these
/// determine whether the cell is at the table's left or right edge — which
/// matters because §17.4.17/§17.4.16 (`gridBefore`/`gridAfter`) can leave
/// the row's first/last cell *not* at the table edge.
pub(super) fn resolve_cell_effective_borders(
    cell: &TableCellInput,
    table_borders: Option<&TableBorderConfig>,
    row_idx: usize,
    cell_grid_col: usize,
    cell_grid_span: usize,
    num_rows: usize,
    num_grid_cols: usize,
) -> (CellEdge, CellEdge, CellEdge, CellEdge) {
    // Start with table-level borders mapped to cell edges.
    let tb = table_borders;
    let is_first_row = row_idx == 0;
    // `row_idx + 1 == num_rows`, not `row_idx == num_rows - 1`: the latter
    // underflows on an empty table. No caller passes `num_rows == 0` today, but
    // the parameter is free and the guard would live entirely in the callers.
    let is_last_row = row_idx + 1 == num_rows;
    let is_first_col = cell_grid_col == 0;
    let is_last_col = cell_grid_col + cell_grid_span >= num_grid_cols;

    let mut top: CellEdge = if is_first_row {
        tb.and_then(|b| b.top)
    } else {
        tb.and_then(|b| b.inside_h)
    }
    .into();
    let mut bottom: CellEdge = if is_last_row {
        tb.and_then(|b| b.bottom)
    } else {
        tb.and_then(|b| b.inside_h)
    }
    .into();
    let mut left: CellEdge = if is_first_col {
        tb.and_then(|b| b.left)
    } else {
        tb.and_then(|b| b.inside_v)
    }
    .into();
    let mut right: CellEdge = if is_last_col {
        tb.and_then(|b| b.right)
    } else {
        tb.and_then(|b| b.inside_v)
    }
    .into();

    // Per-cell overrides. Only `nil` and a real border reach here — an explicit
    // `none` was mapped to "no override" upstream (§17.4.66: it inherits
    // exactly like an omitted edge), so it correctly leaves the table-level
    // border above untouched instead of erasing it.
    if let Some(ref cb) = cell.cell_borders {
        if let Some(v) = &cb.top {
            top = resolve_override(v);
        }
        if let Some(v) = &cb.bottom {
            bottom = resolve_override(v);
        }
        if let Some(v) = &cb.left {
            left = resolve_override(v);
        }
        if let Some(v) = &cb.right {
            right = resolve_override(v);
        }
    }

    (top, bottom, left, right)
}

/// Resolve a border conflict between two competing borders on a shared edge.
/// Returns the winning border (or `None` if both are `None`).
///
/// The algorithm is not in ISO/IEC 29500-1 — the standard only says a method
/// exists. It is spelled out in [MS-OI29500] §17.4.66 (`tcBorders`, note a):
///   1. An edge with no border yields to one that has it. `none` counts as
///      no border.
///   2. Weight = width in eighths of a point × style number. Higher wins.
///   3. Equal weight: the style earlier in the spec's precedence list wins —
///      `Single` over `Double`. See `style_precedence_index`.
///   4. Equal style: darker colour wins (`R+B+2G`, then `B+2G`, then `G`).
///
/// `nil` acts on its own cell only: it declines the inheritance in step 0
/// (style → `tblPrEx` → `tblBorders`), which is its whole difference from
/// `none`, but does not remove the facing cell's border. The note's literal
/// wording (*"If the conflicting table cell border is `nil`, then no border
/// shall be displayed"*) would delete borders Word draws — a cell paints one
/// border across its whole width, so a wide cell's `nil` cannot punch a hole in
/// the cell above it. So `Suppressed` yields here exactly like `Absent`. `nil`
/// is still not a no-op: with nothing facing it (a table's outer edge, or a
/// facing `nil`) declining inheritance is what removes the border.
///
/// The comparison is a **total order** so the result is independent of argument
/// order: the caller feeds (upper row's bottom, lower row's top) and (left
/// cell's right, right cell's left), and the winner must not depend on which
/// side of the edge a border came from.
///
/// Suppression is a third state — the reason the argument type is [`CellEdge`],
/// not `Option<TableBorderLine>`. When neither side paints, returning
/// `Suppressed` rather than `Absent` keeps them distinguishable: a suppressed
/// edge must not be revived by the page-split top-border restore in `emit.rs`.
pub(super) fn resolve_border_conflict(a: CellEdge, b: CellEdge) -> CellEdge {
    match (a, b) {
        (CellEdge::Line(la), CellEdge::Line(lb)) => {
            match border_precedence(&la).cmp(&border_precedence(&lb)) {
                std::cmp::Ordering::Less => b,
                _ => a,
            }
        }
        // One side paints: it does so regardless of what the other side says.
        // A facing `nil` removed *its* border, not this one.
        (CellEdge::Line(_), _) => a,
        (_, CellEdge::Line(_)) => b,
        // Neither side paints. Carry suppression forward so the page-split
        // restore cannot revive an edge the author explicitly emptied.
        (CellEdge::Suppressed, _) | (_, CellEdge::Suppressed) => CellEdge::Suppressed,
        (CellEdge::Absent, CellEdge::Absent) => CellEdge::Absent,
    }
}

/// Sort key for [MS-OI29500] §17.4.66 conflict resolution — greater wins.
///
/// Returns integers so the key is `Ord`: comparing `f32` weights directly would
/// need `partial_cmp`, and a `NaN` width (unreachable, but the type permits it)
/// would make the comparison non-transitive and reintroduce order-dependence.
///
/// Style and colour are both spec-ranked as "lower value wins" (earliest in the
/// precedence list, smallest brightness). This key is "greater wins", so each is
/// subtracted from its type's maximum.
fn border_precedence(b: &TableBorderLine) -> (u32, u8, u32, u32, u32) {
    let (l0, l1, l2) = colour_luminance(b);
    (
        // Weight in eighths of a point, rounded — the spec's `sz` unit.
        (border_weight(b) * 8.0).round().max(0.0) as u32,
        u8::MAX - style_precedence_index(b.style),
        u32::MAX - l0,
        u32::MAX - l1,
        u32::MAX - l2,
    )
}

/// [MS-OI29500] §17.4.66 style precedence: at equal weight, *"the higher of the
/// two on this precedence list shall be displayed"*, the list being
///
/// > single, thick, double, dotted, dashed, dotDash, dotDotDash, triple,
/// > thinThickSmallGap, … outset, inset
///
/// "Higher on the list" means **earlier**, so this returns the 0-based index
/// into it and **lower wins** — `border_precedence` inverts it.
///
/// So `Single` beats `Double` at equal weight (note the intuition runs the other
/// way: a double border has the greater style number, 3 vs 1, so at equal weight
/// the single is three times wider, and the spec prefers it).
///
/// Only `Single` and `Double` reach layout (the other 24 §17.4.38 styles are
/// approximated as `Single` — see `convert_model_border`), so only their two
/// positions are modelled: single is first, double is third.
fn style_precedence_index(style: TableBorderStyle) -> u8 {
    match style {
        TableBorderStyle::Single => 0,
        TableBorderStyle::Double => 2,
    }
}

/// [MS-OI29500] §17.4.66 darkness keys, compared in order: `R+B+2G`, then
/// `B+2G`, then `G`. Lower is darker.
fn colour_luminance(b: &TableBorderLine) -> (u32, u32, u32) {
    let (r, g, bl) = (b.color.r as u32, b.color.g as u32, b.color.b as u32);
    (r + bl + 2 * g, bl + 2 * g, g)
}

/// Emit all four borders for a cell as filled rectangles.
/// Borders are drawn INWARD from the cell edge per OOXML.
///
/// Horizontal borders (top/bottom) own the corner squares — they span the
/// full cell width. Vertical borders (left/right) fill only the space
/// between the horizontals. This eliminates anti-aliasing gaps at corners
/// that plagued the previous stroke-based approach.
pub(super) fn emit_cell_borders(
    commands: &mut Vec<DrawCommand>,
    b: CellBorders,
    cell_x: Pt,
    cell_w: Pt,
    row_y: Pt,
    row_h: Pt,
) {
    // Resolution is over by now, so `Suppressed` and `Absent` are the same
    // thing here: nothing to paint.
    let (top, bottom, left, right) = (b.top.line(), b.bottom.line(), b.left.line(), b.right.line());
    let top_w = top.map(|b| b.width).unwrap_or(Pt::ZERO);
    let bot_w = bottom.map(|b| b.width).unwrap_or(Pt::ZERO);
    let left_w = left.map(|b| b.width).unwrap_or(Pt::ZERO);
    let right_w = right.map(|b| b.width).unwrap_or(Pt::ZERO);

    // Horizontal borders: full cell width, covering corner squares.
    if let Some(ref border) = top {
        emit_border_rect(
            commands,
            border,
            PtRect::from_xywh(cell_x, row_y, cell_w, top_w),
            true,
        );
    }
    if let Some(ref border) = bottom {
        emit_border_rect(
            commands,
            border,
            PtRect::from_xywh(cell_x, row_y + row_h - bot_w, cell_w, bot_w),
            true,
        );
    }

    // Vertical borders: between horizontal borders (no corner overlap).
    let top_inset = if top.is_some() { top_w } else { Pt::ZERO };
    let bot_inset = if bottom.is_some() { bot_w } else { Pt::ZERO };
    let v_height = row_h - top_inset - bot_inset;
    if v_height > Pt::ZERO {
        if let Some(ref border) = left {
            emit_border_rect(
                commands,
                border,
                PtRect::from_xywh(cell_x, row_y + top_inset, left_w, v_height),
                false,
            );
        }
        if let Some(ref border) = right {
            emit_border_rect(
                commands,
                border,
                PtRect::from_xywh(
                    cell_x + cell_w - right_w,
                    row_y + top_inset,
                    right_w,
                    v_height,
                ),
                false,
            );
        }
    }
}

/// [MS-OI29500] §17.4.66: border weight = width × style number, in points.
///
/// The spec states the rule in eighths of a point (`w:sz`), but every use is a
/// *comparison* between two weights, and converting both to eighths scales both
/// by the same 8 — so the factor cancels. Keeping it in points avoids implying
/// that a unit conversion is load-bearing here. `border_precedence` scales to
/// eighths once, where rounding to an integer sort key does depend on the unit.
fn border_weight(b: &TableBorderLine) -> f32 {
    let style_number = match b.style {
        TableBorderStyle::Single => 1.0,
        TableBorderStyle::Double => 3.0,
    };
    b.width.raw() * style_number
}

/// Width of the line this edge paints, or zero when it paints none — which
/// includes a suppressed edge, since suppression reserves no space.
pub(super) fn border_width(b: CellEdge) -> Pt {
    b.line().map(|b| b.width).unwrap_or(Pt::ZERO)
}

fn resolve_override(ovr: &CellBorderOverride) -> CellEdge {
    match ovr {
        CellBorderOverride::Suppress => CellEdge::Suppressed,
        // The cell's own `<w:tcBorders>` — the provenance that beats a facing
        // `nil` in `resolve_border_conflict`.
        CellBorderOverride::Border(line) => CellEdge::Line(*line),
    }
}

/// Emit a border as filled rectangle(s).
/// `is_horizontal` controls double-border sub-rect orientation.
fn emit_border_rect(
    commands: &mut Vec<DrawCommand>,
    b: &TableBorderLine,
    rect: PtRect,
    is_horizontal: bool,
) {
    match b.style {
        TableBorderStyle::Single => {
            commands.push(DrawCommand::Rect {
                rect,
                color: b.color,
            });
        }
        TableBorderStyle::Double => {
            // §17.4.38: total = w:sz, each sub-line = sz/3, gap = sz/3.
            let sub = b.width * (1.0 / 3.0);
            if is_horizontal {
                // Two horizontal sub-rects: top and bottom of the border area.
                commands.push(DrawCommand::Rect {
                    rect: PtRect::from_xywh(rect.origin.x, rect.origin.y, rect.size.width, sub),
                    color: b.color,
                });
                commands.push(DrawCommand::Rect {
                    rect: PtRect::from_xywh(
                        rect.origin.x,
                        rect.origin.y + rect.size.height - sub,
                        rect.size.width,
                        sub,
                    ),
                    color: b.color,
                });
            } else {
                // Two vertical sub-rects: left and right of the border area.
                commands.push(DrawCommand::Rect {
                    rect: PtRect::from_xywh(rect.origin.x, rect.origin.y, sub, rect.size.height),
                    color: b.color,
                });
                commands.push(DrawCommand::Rect {
                    rect: PtRect::from_xywh(
                        rect.origin.x + rect.size.width - sub,
                        rect.origin.y,
                        sub,
                        rect.size.height,
                    ),
                    color: b.color,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::render::dimension::Pt;
    use crate::render::geometry::PtEdgeInsets;
    use crate::render::layout::draw_command::DrawCommand;
    use crate::render::layout::fragment::{FontProps, Fragment, TextMetrics};
    use crate::render::layout::paragraph::ParagraphStyle;
    use crate::render::layout::section::LayoutBlock;
    use crate::render::layout::table::{
        CellVAlign, TableBorderConfig, TableBorderLine, TableBorderStyle, TableCellInput,
        TableRowInput, layout_table,
    };
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn text_frag(text: &str, width: f32) -> Fragment {
        Fragment::Text {
            text: text.into(),
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

    fn simple_cell(text: &str) -> TableCellInput {
        TableCellInput {
            blocks: vec![LayoutBlock::Paragraph {
                fragments: vec![text_frag(text, 30.0)],
                style: ParagraphStyle::default(),
                page_break_before: false,
                footnotes: vec![],
                floating_images: vec![],
                floating_shapes: vec![],
            }],
            margins: PtEdgeInsets::ZERO,
            grid_span: 1,
            shading: None,
            cell_borders: None,
            vertical_merge: None,
            vertical_align: CellVAlign::Top,
        }
    }

    #[test]
    fn borders_emit_lines() {
        let rows = vec![TableRowInput {
            cells: vec![simple_cell("a"), simple_cell("b")],
            height_rule: None,
            is_header: None,
            cant_split: None,
            grid_before: 0,
            border_overrides: None,
        }];
        let col_widths = vec![Pt::new(100.0), Pt::new(100.0)];
        let result = layout_table(
            &rows,
            &col_widths,
            Pt::ZERO,
            Pt::new(14.0),
            Some(&TableBorderConfig {
                top: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
                bottom: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
                left: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
                right: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
                inside_h: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
                inside_v: Some(TableBorderLine {
                    width: Pt::new(0.5),
                    color: RgbColor::BLACK,
                    style: TableBorderStyle::Single,
                }),
            }),
            None,
            false,
        );

        // Borders are emitted as filled rects. Count border rects by
        // excluding cell shading rects (which use non-BLACK colors or
        // appear before borders in the command list).
        let border_rect_count = result
            .commands
            .iter()
            .filter(|c| matches!(c, DrawCommand::Rect { color, .. } if *color == RgbColor::BLACK))
            .count();
        // [MS-OI29500] §17.4.66: shared edges drawn once after conflict resolution.
        // Top(2) + bottom(2) + left(1) + insideV(1) + right(1) = 7 border rects.
        assert_eq!(border_rect_count, 7);
    }

    /// §17.4.61 tblPrEx — when a row carries a `tblBorders` override,
    /// it fully replaces the table's tblBorders for *that row only*.
    /// Here row 0 sets every side to "no border", row 1 doesn't.
    /// The table-wide config has all sides set to single. Expectation:
    /// row 0's cell contributes zero border rects, while row 1's cell
    /// produces the usual top/left/right/bottom set.
    #[test]
    fn row_border_override_replaces_table_borders_for_that_row() {
        let single = TableBorderLine {
            width: Pt::new(0.5),
            color: RgbColor::BLACK,
            style: TableBorderStyle::Single,
        };
        let all_single = TableBorderConfig {
            top: Some(single),
            bottom: Some(single),
            left: Some(single),
            right: Some(single),
            inside_h: Some(single),
            inside_v: Some(single),
        };
        let no_borders = TableBorderConfig {
            top: None,
            bottom: None,
            left: None,
            right: None,
            inside_h: None,
            inside_v: None,
        };
        let rows = vec![
            TableRowInput {
                cells: vec![simple_cell("opt-out")],
                height_rule: None,
                is_header: None,
                cant_split: None,
                grid_before: 0,
                border_overrides: Some(no_borders),
            },
            TableRowInput {
                cells: vec![simple_cell("normal")],
                height_rule: None,
                is_header: None,
                cant_split: None,
                grid_before: 0,
                border_overrides: None,
            },
        ];
        let col_widths = vec![Pt::new(100.0)];
        let result = layout_table(
            &rows,
            &col_widths,
            Pt::ZERO,
            Pt::new(14.0),
            Some(&all_single),
            None,
            false,
        );

        // Group border rects by their y position. The opt-out row is
        // first (lower y), the normal row second. We know the order
        // because layout_table walks rows top-down.
        let border_rects: Vec<_> = result
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Rect { rect, color } if *color == RgbColor::BLACK => Some(*rect),
                _ => None,
            })
            .collect();

        // No rect should sit entirely within row 0's vertical span —
        // not the cell's top, not its sides, not its bottom (with row 1
        // having a top border, conflict resolution gives row 0 a
        // bottom from row 1's top, but that's drawn at the boundary,
        // not inside row 0).
        // We exercise this by asserting that no rect's *vertical*
        // extent falls within (epsilon, row_0_height - epsilon) — the
        // strict interior of row 0.
        let row_0_height = Pt::new(14.0);
        let interior_eps = Pt::new(0.1);
        let interior_top = interior_eps;
        let interior_bottom = row_0_height - interior_eps;
        for rect in &border_rects {
            let r_top = rect.origin.y;
            let r_bottom = rect.origin.y + rect.size.height;
            let entirely_inside = r_top >= interior_top && r_bottom <= interior_bottom;
            assert!(
                !entirely_inside,
                "row 0 (border-override = all None) must not host a \
                 black border rect strictly inside its content area; got rect \
                 y=[{:.2}..{:.2}] (interior was ({:.2}..{:.2}))",
                r_top.raw(),
                r_bottom.raw(),
                interior_top.raw(),
                interior_bottom.raw(),
            );
        }
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;
    use crate::render::resolve::color::RgbColor;

    const BLACK: RgbColor = RgbColor { r: 0, g: 0, b: 0 };
    const PALE: RgbColor = RgbColor {
        r: 220,
        g: 220,
        b: 220,
    };

    fn line(width: f32, style: TableBorderStyle, color: RgbColor) -> TableBorderLine {
        TableBorderLine {
            width: Pt::new(width),
            color,
            style,
        }
    }

    /// A representative spread: both styles, several widths, both colours.
    fn sample_borders() -> Vec<TableBorderLine> {
        let mut v = Vec::new();
        for &w in &[0.5f32, 1.0, 2.0, 3.0, 6.0] {
            for &s in &[TableBorderStyle::Single, TableBorderStyle::Double] {
                for &c in &[BLACK, PALE] {
                    v.push(line(w, s, c));
                }
            }
        }
        v
    }

    /// The caller passes (upper row's bottom, lower row's top) and (left cell's
    /// right, right cell's left), so a resolution that depends on argument order
    /// would make the rendered border depend on which side of the edge it was
    /// declared on.
    #[test]
    fn resolution_is_independent_of_argument_order() {
        let borders = sample_borders();
        for a in &borders {
            for b in &borders {
                let ab = resolve_border_conflict(CellEdge::Line(*a), CellEdge::Line(*b));
                let ba = resolve_border_conflict(CellEdge::Line(*b), CellEdge::Line(*a));
                assert_eq!(
                    (ab.line().map(|x| (x.width, x.style, x.color))),
                    (ba.line().map(|x| (x.width, x.style, x.color))),
                    "order-dependent for {a:?} vs {b:?}"
                );
            }
        }
    }

    /// Step 2 — the heavier border wins outright.
    #[test]
    fn heavier_weight_wins() {
        let thin = line(0.5, TableBorderStyle::Single, BLACK);
        let thick = line(2.0, TableBorderStyle::Single, BLACK);
        assert_eq!(
            resolve_border_conflict(CellEdge::Line(thin), CellEdge::Line(thick))
                .line()
                .map(|b| b.width),
            Some(Pt::new(2.0))
        );
        assert_eq!(
            resolve_border_conflict(CellEdge::Line(thick), CellEdge::Line(thin))
                .line()
                .map(|b| b.width),
            Some(Pt::new(2.0))
        );
    }

    /// Step 3 — equal weight, so position in the spec's precedence list decides,
    /// and **`Single` wins**. 3pt single and 1pt double both weigh 3
    /// (width × style number). [MS-OI29500] §17.4.66 orders the list
    /// `single, thick, double, …` and displays *"the higher of the two on this
    /// precedence list"*, i.e. the earlier one.
    #[test]
    fn equal_weight_prefers_the_earlier_style_in_the_precedence_list() {
        let single = line(3.0, TableBorderStyle::Single, BLACK);
        let double = line(1.0, TableBorderStyle::Double, BLACK);
        assert_eq!(
            border_weight(&single),
            border_weight(&double),
            "same weight"
        );

        for (a, b) in [(single, double), (double, single)] {
            assert_eq!(
                resolve_border_conflict(CellEdge::Line(a), CellEdge::Line(b))
                    .line()
                    .map(|x| x.style),
                Some(TableBorderStyle::Single),
                "Single is earlier in the precedence list, so it wins at equal weight"
            );
        }
    }

    /// The tie-break must not leak into the *weight* comparison: a double
    /// border of equal width still outweighs a single (style number 3 vs 1) and
    /// wins at step 2, before precedence is consulted.
    ///
    /// Pins the two steps apart. Ranking `Single` above `Double` is only correct
    /// as a tie-break; applied one step earlier it would invert every ordinary
    /// single-vs-double edge in a table.
    #[test]
    fn precedence_does_not_override_weight() {
        let single = line(1.0, TableBorderStyle::Single, BLACK);
        let double = line(1.0, TableBorderStyle::Double, BLACK);
        assert!(
            border_weight(&double) > border_weight(&single),
            "equal width, double is heavier"
        );

        for (a, b) in [(single, double), (double, single)] {
            assert_eq!(
                resolve_border_conflict(CellEdge::Line(a), CellEdge::Line(b))
                    .line()
                    .map(|x| x.style),
                Some(TableBorderStyle::Double),
                "the heavier border wins outright, regardless of precedence"
            );
        }
    }

    /// Step 4 — equal weight and style, so the darker colour decides.
    #[test]
    fn equal_weight_and_style_prefers_the_darker_colour() {
        let dark = line(1.0, TableBorderStyle::Single, BLACK);
        let pale = line(1.0, TableBorderStyle::Single, PALE);
        for (a, b) in [(dark, pale), (pale, dark)] {
            assert_eq!(
                resolve_border_conflict(CellEdge::Line(a), CellEdge::Line(b))
                    .line()
                    .map(|x| x.color),
                Some(BLACK),
                "darker colour wins regardless of argument order"
            );
        }
    }

    /// The §17.4.66 darkness keys are compared in order `R+B+2G`, then `B+2G`,
    /// then `G` — so two colours with the same total brightness are separated by
    /// the later keys rather than by argument order.
    #[test]
    fn darkness_tie_breaks_on_the_secondary_keys() {
        // R+B+2G equal (both 255*2 = 510... constructed to match), differing in
        // the B+2G term.
        let a = line(
            1.0,
            TableBorderStyle::Single,
            RgbColor { r: 100, g: 0, b: 0 },
        );
        let b = line(
            1.0,
            TableBorderStyle::Single,
            RgbColor { r: 0, g: 0, b: 100 },
        );
        assert_eq!(
            colour_luminance(&a).0,
            colour_luminance(&b).0,
            "primary key ties"
        );
        // a has B+2G = 0, b has B+2G = 100 → a is "darker" by the second key.
        let winner = resolve_border_conflict(CellEdge::Line(a), CellEdge::Line(b))
            .line()
            .expect("some");
        assert_eq!(winner.color, RgbColor { r: 100, g: 0, b: 0 });
        // And symmetric.
        assert_eq!(
            resolve_border_conflict(CellEdge::Line(b), CellEdge::Line(a))
                .line()
                .map(|x| x.color),
            Some(RgbColor { r: 100, g: 0, b: 0 })
        );
    }

    /// Step 1 — an absent border yields to a present one, in both directions,
    /// and two absent borders stay absent.
    #[test]
    fn absent_yields_to_present() {
        let some = line(1.0, TableBorderStyle::Single, BLACK);
        assert_eq!(
            resolve_border_conflict(CellEdge::Absent, CellEdge::Line(some))
                .line()
                .map(|b| b.width),
            Some(Pt::new(1.0))
        );
        assert_eq!(
            resolve_border_conflict(CellEdge::Line(some), CellEdge::Absent)
                .line()
                .map(|b| b.width),
            Some(Pt::new(1.0))
        );
        assert_eq!(
            resolve_border_conflict(CellEdge::Absent, CellEdge::Absent),
            CellEdge::Absent
        );
    }

    /// `nil` does not reach across the edge. [MS-OI29500] §17.4.66 says *"If the
    /// conflicting table cell border is nil, then no border shall be displayed"*,
    /// and read literally that is wrong: `nil` empties its own cell's edge and
    /// leaves the facing cell's border alone. It loses from either side and at
    /// any weight — even a hairline survives it, because a cell paints one border
    /// across its whole width and so a wide cell's `nil` cannot punch a hole in
    /// the cell above it.
    #[test]
    fn nil_yields_to_the_facing_border() {
        let hair = line(0.25, TableBorderStyle::Single, BLACK);
        for (a, b) in [
            (CellEdge::Suppressed, CellEdge::Line(hair)),
            (CellEdge::Line(hair), CellEdge::Suppressed),
        ] {
            assert_eq!(
                resolve_border_conflict(a, b).line(),
                Some(hair),
                "the facing border must survive the nil: {a:?} vs {b:?}"
            );
        }
    }

    /// …and yet `nil` is not a no-op, because it declined **inheritance**
    /// upstream in `resolve_cell_effective_borders`. With nothing facing it —
    /// another `nil`, or an edge nobody spoke for — nothing is painted, and the
    /// result stays `Suppressed` rather than collapsing to `Absent`.
    ///
    /// That last part is load-bearing: `emit.rs` may revive an `Absent` top when
    /// a row starts a page slice, and must not revive an emptied one.
    #[test]
    fn nil_stays_suppressed_when_nothing_faces_it() {
        for (a, b) in [
            (CellEdge::Suppressed, CellEdge::Absent),
            (CellEdge::Absent, CellEdge::Suppressed),
            (CellEdge::Suppressed, CellEdge::Suppressed),
        ] {
            assert_eq!(
                resolve_border_conflict(a, b),
                CellEdge::Suppressed,
                "suppression must survive where nothing paints: {a:?} vs {b:?}"
            );
        }
        assert_eq!(
            resolve_border_conflict(CellEdge::Absent, CellEdge::Absent),
            CellEdge::Absent,
            "…but two silent edges stay restorable"
        );
    }

    /// The counterpart, and the half that is easy to get wrong when fixing the
    /// other: an edge declared `none` is **not** suppression. §17.4.66 puts it
    /// with the omitted case — *"If the conflicting table cell border is none
    /// (no border), then the opposing border shall be displayed."*
    ///
    /// `none` never reaches the resolver as its own state; it arrives as
    /// `Absent` because `convert_cell_border_override` maps it to "no override".
    /// This test pins the consequence at the level the resolver sees.
    #[test]
    fn an_absent_edge_never_suppresses() {
        let border = line(1.0, TableBorderStyle::Single, BLACK);
        assert_eq!(
            resolve_border_conflict(CellEdge::Absent, CellEdge::Line(border)),
            CellEdge::Line(border),
            "absent (which is what `none` becomes) must yield, not suppress"
        );
    }

    /// Identical borders resolve to themselves — the reflexive case, which a
    /// comparison built on `partial_cmp` of `f32` could get wrong.
    #[test]
    fn identical_borders_resolve_to_themselves() {
        for b in sample_borders() {
            let r = resolve_border_conflict(CellEdge::Line(b), CellEdge::Line(b))
                .line()
                .expect("some");
            assert_eq!((r.width, r.style, r.color), (b.width, b.style, b.color));
        }
    }
}

/// §17.4.38 edge mapping: which of the six table-level borders each cell edge
/// draws from, given the cell's position in the grid.
#[cfg(test)]
mod edge_mapping_tests {
    use super::*;
    use crate::render::geometry::PtEdgeInsets;
    use crate::render::layout::table::CellVAlign;
    use crate::render::resolve::color::RgbColor;

    /// Every edge gets its own width, so a resolved border names the config
    /// field it came from.
    const TOP: f32 = 1.0;
    const BOTTOM: f32 = 2.0;
    const LEFT: f32 = 3.0;
    const RIGHT: f32 = 4.0;
    const INSIDE_H: f32 = 5.0;
    const INSIDE_V: f32 = 6.0;

    fn edge(width: f32) -> Option<TableBorderLine> {
        Some(TableBorderLine {
            width: Pt::new(width),
            color: RgbColor::BLACK,
            style: TableBorderStyle::Single,
        })
    }

    fn config() -> TableBorderConfig {
        TableBorderConfig {
            top: edge(TOP),
            bottom: edge(BOTTOM),
            left: edge(LEFT),
            right: edge(RIGHT),
            inside_h: edge(INSIDE_H),
            inside_v: edge(INSIDE_V),
        }
    }

    fn plain_cell() -> TableCellInput {
        TableCellInput {
            blocks: vec![],
            margins: PtEdgeInsets::ZERO,
            grid_span: 1,
            shading: None,
            cell_borders: None,
            vertical_merge: None,
            vertical_align: CellVAlign::Top,
        }
    }

    /// `(top, bottom, left, right)` widths, so a failure reads as which edges
    /// were mis-mapped rather than as four separate assertions.
    fn widths(
        row_idx: usize,
        grid_col: usize,
        num_rows: usize,
        num_grid_cols: usize,
    ) -> (Option<f32>, Option<f32>, Option<f32>, Option<f32>) {
        let (t, b, l, r) = resolve_cell_effective_borders(
            &plain_cell(),
            Some(&config()),
            row_idx,
            grid_col,
            1,
            num_rows,
            num_grid_cols,
        );
        let w = |e: CellEdge| e.line().map(|e| e.width.raw());
        (w(t), w(b), w(l), w(r))
    }

    /// A 3×3 grid: the corners take the outer borders, the middle takes
    /// `insideH`/`insideV` on all four sides.
    #[test]
    fn outer_edges_use_outer_borders_and_interior_edges_use_inside() {
        assert_eq!(
            widths(0, 0, 3, 3),
            (Some(TOP), Some(INSIDE_H), Some(LEFT), Some(INSIDE_V)),
            "top-left cell"
        );
        assert_eq!(
            widths(1, 1, 3, 3),
            (
                Some(INSIDE_H),
                Some(INSIDE_H),
                Some(INSIDE_V),
                Some(INSIDE_V)
            ),
            "centre cell"
        );
        assert_eq!(
            widths(2, 2, 3, 3),
            (Some(INSIDE_H), Some(BOTTOM), Some(INSIDE_V), Some(RIGHT)),
            "bottom-right cell"
        );
    }

    /// A single-row, single-column table is both first and last on both axes,
    /// so it takes all four outer borders and neither inside border.
    #[test]
    fn a_one_cell_table_takes_all_four_outer_borders() {
        assert_eq!(
            widths(0, 0, 1, 1),
            (Some(TOP), Some(BOTTOM), Some(LEFT), Some(RIGHT))
        );
    }

    /// `num_rows == 0` is unreachable through `layout_table` (it returns early on
    /// empty input, and every other caller is inside a row loop), but `num_rows`
    /// is a free parameter of a `pub(super)` function, so the last-row check must
    /// not depend on a caller having validated it. `row_idx == num_rows - 1`
    /// underflows here; `row_idx + 1 == num_rows` answers "no row is the last row
    /// of an empty table".
    #[test]
    fn an_empty_table_does_not_underflow_the_last_row_check() {
        assert_eq!(
            widths(0, 0, 0, 3),
            (Some(TOP), Some(INSIDE_H), Some(LEFT), Some(INSIDE_V))
        );
    }
}
