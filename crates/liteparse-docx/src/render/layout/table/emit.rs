//! Table command emission — positions cells and emits border commands.

use crate::render::dimension::Pt;
use crate::render::geometry::PtRect;

use crate::render::layout::draw_command::DrawCommand;

use super::borders::{CellBorders, CellEdge, border_width, emit_cell_borders};
use super::grid::is_vmerge_continue;
use super::types::{
    CellVAlign, MeasuredRow, MeasuredTable, TableBorderLine, TableRowInput, VerticalMergeState,
};

/// Layered command buffers for table rendering: shading, content, borders.
pub(super) struct TableCommandBuffers<'a> {
    pub(super) commands: &'a mut Vec<DrawCommand>,
    pub(super) content_commands: &'a mut Vec<DrawCommand>,
    pub(super) border_commands: &'a mut Vec<DrawCommand>,
}

/// Emit draw commands for a range of measured rows.
///
/// `top_border_override`: if `Some`, the first row in the range gets this border
/// as its top edge. Used for page-split tables where the measured top borders were
/// suppressed (adjacent table collapse) or resolved away (conflict resolution),
/// but the continuation slice still needs a visible top boundary.
pub(super) fn emit_table_rows(
    measured: &MeasuredTable,
    rows: &[TableRowInput],
    row_range: std::ops::Range<usize>,
    cursor_y: &mut Pt,
    bufs: &mut TableCommandBuffers<'_>,
    top_border_override: Option<TableBorderLine>,
) {
    let num_rows = measured.rows.len();
    let range_start = row_range.start;
    for row_idx in row_range {
        let mr = &measured.rows[row_idx];
        let is_first_in_range = row_idx == range_start;
        // Deliberately the *table's* row count, not the range's: this mirrors
        // `measure_table_rows`' own condition for reserving `border_gap_below`
        // (`row_idx + 1 < num_rows`), so the two always agree about whether a
        // gap exists. A row that ends a page slice but not the table still has
        // its reserved gap, and its bottom border belongs in it.
        let has_reserved_bottom_gap = row_idx + 1 < num_rows;
        emit_one_row(
            mr,
            &rows[row_idx],
            cursor_y,
            bufs,
            if is_first_in_range {
                top_border_override
            } else {
                None
            },
            has_reserved_bottom_gap,
            Some((measured, rows, row_idx)),
        );
    }
}

/// Emit a custom `MeasuredRow` (produced by `split::split_row_at`). Unlike
/// the range-based emit above, this takes a single already-built
/// `MeasuredRow` and the matching `TableRowInput`. `vmerge_ctx` is always
/// `None` here — split rows can't contain vMerge (the group is flagged
/// not splittable if any cell is merged).
pub(super) fn emit_split_row(
    mr: &MeasuredRow,
    row: &TableRowInput,
    cursor_y: &mut Pt,
    bufs: &mut TableCommandBuffers<'_>,
    top_border_override: Option<TableBorderLine>,
    has_reserved_bottom_gap: bool,
) {
    emit_one_row(
        mr,
        row,
        cursor_y,
        bufs,
        top_border_override,
        has_reserved_bottom_gap,
        None,
    );
}

fn emit_one_row(
    mr: &MeasuredRow,
    row: &TableRowInput,
    cursor_y: &mut Pt,
    bufs: &mut TableCommandBuffers<'_>,
    top_border_override: Option<TableBorderLine>,
    // Whether space was reserved below this row for its bottom border, so the
    // border is drawn *under* the content rather than inset into it.
    //
    // Not a positional fact, which is why the two callers derive it
    // differently and both are right: `emit_table_rows` repeats
    // `measure_table_rows`' reservation condition, while a split row's halves
    // carry their own (`split_row_at` zeroes the first half's gap — a cut edge
    // has none — and passes the original's to the second).
    has_reserved_bottom_gap: bool,
    // For vMerge=Restart cells, resolve the full merged span height.
    // `None` disables the lookup (used for split rows and for standalone
    // emission paths that don't carry a merge context).
    vmerge_ctx: Option<(&MeasuredTable, &[TableRowInput], usize)>,
) {
    // §17.4.44: the row's box starts one cell-spacing below the cursor; its
    // content box is what remains. Both are zero-cost when no spacing is set.
    let leading = mr.leading_gap;
    let row_top = *cursor_y + leading;
    let row_height = mr.height - leading;
    for (cell_ci, (entry, cell_input)) in mr.entries.iter().zip(row.cells.iter()).enumerate() {
        // §17.4.85: the merged span, used below for vAlign and here for
        // shading. Hoisted above the shading so both read the same height —
        // shading used `row_height` while vAlign used the span, so a shaded
        // merged cell was coloured across its first row only.
        let effective_h = if cell_input.vertical_merge == Some(VerticalMergeState::Restart) {
            vmerge_ctx
                .map(|(m, rs, row_idx)| merged_span_height(m, rs, row_idx, entry.grid_col))
                .unwrap_or(row_height)
        } else {
            row_height
        };

        // §17.4.33 / §17.4.85: a merged cell's shading covers the whole span,
        // and the `Continue` rows do not paint their own. Word treats the
        // continuation cells as part of the `Restart` cell, so its `<w:shd>`
        // governs the merged region; letting a continuation paint over the span
        // would let a stale or differing `shd` on a row that has no independent
        // existence win for that row.
        if cell_input.vertical_merge != Some(VerticalMergeState::Continue) {
            if let Some(color) = cell_input.shading {
                bufs.commands.push(DrawCommand::Rect {
                    rect: PtRect::from_xywh(entry.cell_x, row_top, entry.cell_w, effective_h),
                    color,
                });
            }
        }

        // §17.4.38: restore the top border when this row starts a slice and the
        // resolved top was removed by conflict resolution or adjacent-table
        // collapse. An edge the author set to `nil` must NOT be restored — they
        // asked for no border — and `CellEdge` already says which is which, so
        // this reads the resolved edge instead of re-deriving intent from
        // `cell_input`. That also fixes the `none` case for free: `none` now
        // resolves to `Absent` and is restorable, where the old re-derivation
        // lumped it in with `nil` and left a continuation slice with no top.
        let cell_top = match mr.borders[cell_ci].top {
            CellEdge::Absent => top_border_override.into(),
            resolved => resolved,
        };
        let b_left = mr.borders[cell_ci].left;
        let b_right = mr.borders[cell_ci].right;
        let b_bottom = mr.borders[cell_ci].bottom;

        let dx = (border_width(b_left) - cell_input.margins.left).max(Pt::ZERO);
        let dy_border = (border_width(cell_top) - cell_input.margins.top).max(Pt::ZERO);

        // §17.4.85: for vMerge=Restart cells, vAlign operates over the whole
        // merged span (`effective_h`, computed above with the shading).
        let content_h = entry.layout.content_height + cell_input.margins.vertical();
        let dy_valign = match cell_input.vertical_align {
            CellVAlign::Bottom => (effective_h - content_h - dy_border).max(Pt::ZERO),
            CellVAlign::Center => ((effective_h - content_h - dy_border) * 0.5).max(Pt::ZERO),
            CellVAlign::Top => Pt::ZERO,
        };

        for cmd in &entry.layout.commands {
            let mut cmd = cmd.clone();
            cmd.shift(entry.cell_x + dx, row_top + dy_border + dy_valign);
            bufs.content_commands.push(cmd);
        }

        let continues_below = vmerge_ctx.is_some_and(|(_, rows, row_idx)| {
            row_idx + 1 < rows.len() && is_vmerge_continue(&rows[row_idx + 1], entry.grid_col)
        });
        let bottom_border_gap = if has_reserved_bottom_gap {
            if continues_below {
                mr.border_gap_below
            } else {
                border_width(b_bottom)
            }
        } else {
            Pt::ZERO
        };
        emit_cell_borders(
            bufs.border_commands,
            CellBorders {
                top: cell_top,
                bottom: b_bottom,
                left: b_left,
                right: b_right,
            },
            entry.cell_x,
            entry.cell_w,
            row_top,
            row_height + bottom_border_gap,
        );
    }

    *cursor_y += mr.height + mr.border_gap_below;
}

/// Total vertical space owned by a vMerge=Restart cell at `grid_col`.
/// Includes the restart row's height and every `Continue` row below it,
/// plus the `border_gap_below` of intermediate rows (the cell's own top/
/// bottom borders between merged rows were suppressed in measurement, so
/// the gap is driven by sibling columns only).
fn merged_span_height(
    measured: &MeasuredTable,
    rows: &[TableRowInput],
    start_row: usize,
    grid_col: usize,
) -> Pt {
    // The span starts below its own leading gap; every row it swallows
    // contributes that row's full height, gap included, because a merged cell
    // covers the spacing between the rows it spans.
    let mut total = measured.rows[start_row].height - measured.rows[start_row].leading_gap;
    let mut row = start_row + 1;
    while row < rows.len() && is_vmerge_continue(&rows[row], grid_col) {
        total += measured.rows[row - 1].border_gap_below;
        total += measured.rows[row].height;
        row += 1;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::geometry::{PtEdgeInsets, PtSize};
    use crate::render::layout::fragment::{FontProps, Fragment, TextMetrics};
    use crate::render::layout::paragraph::ParagraphStyle;
    use crate::render::layout::section::LayoutBlock;
    use crate::render::layout::table::types::TableCellInput;
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    const GREY: RgbColor = RgbColor {
        r: 200,
        g: 200,
        b: 200,
    };

    fn frag(text: &str) -> Fragment {
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
            width: Pt::new(30.0),
            trimmed_width: Pt::new(30.0),
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

    fn cell(
        n_lines: usize,
        vmerge: Option<VerticalMergeState>,
        shading: Option<RgbColor>,
    ) -> TableCellInput {
        TableCellInput {
            blocks: (0..n_lines)
                .map(|i| LayoutBlock::Paragraph {
                    fragments: vec![frag(&format!("L{i}"))],
                    style: ParagraphStyle::default(),
                    page_break_before: false,
                    footnotes: vec![],
                    floating_images: vec![],
                    floating_shapes: vec![],
                })
                .collect(),
            margins: PtEdgeInsets::ZERO,
            grid_span: 1,
            shading,
            cell_borders: None,
            vertical_merge: vmerge,
            vertical_align: CellVAlign::Top,
        }
    }

    fn row(cells: Vec<TableCellInput>) -> TableRowInput {
        TableRowInput {
            cells,
            height_rule: None,
            is_header: None,
            cant_split: None,
            grid_before: 0,
            border_overrides: None,
        }
    }

    fn shading_rects(commands: &[DrawCommand]) -> Vec<(f32, f32)> {
        commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Rect { rect, color } if *color == GREY => {
                    Some((rect.origin.y.raw(), rect.size.height.raw()))
                }
                _ => None,
            })
            .collect()
    }

    /// Two rows: col 0 is a shaded `Restart` over a `Continue`, col 1 carries
    /// two 2-line cells so each row measures 28pt — a 56pt merged span.
    fn merged_shading_table(continue_shading: Option<RgbColor>) -> Vec<TableRowInput> {
        vec![
            row(vec![
                cell(1, Some(VerticalMergeState::Restart), Some(GREY)),
                cell(2, None, None),
            ]),
            row(vec![
                cell(0, Some(VerticalMergeState::Continue), continue_shading),
                cell(2, None, None),
            ]),
        ]
    }

    /// §17.4.85 + §17.4.33: a shaded merged cell is shaded across the whole
    /// span, not just its first row.
    ///
    /// The shading rect used `row_height` while vAlign used the span height, so
    /// only the first row was coloured — the lower half of a shaded merged cell
    /// rendered as unshaded background.
    #[test]
    fn shaded_vmerge_restart_cell_shades_the_whole_span() {
        let rows = merged_shading_table(None);
        let table = crate::render::layout::table::layout_table(
            &rows,
            &[Pt::new(50.0), Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );

        assert_eq!(table.size, PtSize::new(Pt::new(100.0), Pt::new(56.0)));
        assert_eq!(
            shading_rects(&table.commands),
            vec![(0.0, 56.0)],
            "one rect covering the full 56pt merged span"
        );
    }

    /// A `Continue` cell does not paint its own shading: Word treats it as part
    /// of the `Restart` cell, whose `<w:shd>` governs the merged region. Without
    /// this the continuation would paint over the span rect for its own row.
    #[test]
    fn vmerge_continue_cell_does_not_paint_its_own_shading() {
        let rows = merged_shading_table(Some(RgbColor { r: 1, g: 2, b: 3 }));
        let table = crate::render::layout::table::layout_table(
            &rows,
            &[Pt::new(50.0), Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );

        // The span rect survives whole …
        assert_eq!(shading_rects(&table.commands), vec![(0.0, 56.0)]);
        // … and the continuation's own colour is never emitted.
        assert!(
            !table.commands.iter().any(|c| matches!(
                c,
                DrawCommand::Rect {
                    color: RgbColor { r: 1, g: 2, b: 3 },
                    ..
                }
            )),
            "a Continue cell's shading must not override the merged cell's"
        );
    }

    /// An unmerged shaded cell is unaffected — the fix must not widen every
    /// cell's shading to some span.
    #[test]
    fn unmerged_shaded_cell_shades_only_its_own_row() {
        let rows = vec![
            row(vec![cell(1, None, Some(GREY))]),
            row(vec![cell(1, None, None)]),
        ];
        let table = crate::render::layout::table::layout_table(
            &rows,
            &[Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );
        assert_eq!(shading_rects(&table.commands), vec![(0.0, 14.0)]);
    }

    /// `merged_span_height` stops at the first row that does not continue *this*
    /// grid column — the row below the span is not absorbed into it.
    ///
    /// Observed through the shading rect, since that (with vAlign) is what the
    /// function feeds; it does not affect row heights. Rows 0-1 form the span
    /// and split the restart cell's 14pt of content 7/7 (§17.4.85
    /// distribution), so the span is 14pt while the table is 28pt. A walk that
    /// ran past the `Continue` would shade all 28.
    #[test]
    fn merged_span_height_stops_at_the_first_non_continue_row() {
        let rows = vec![
            row(vec![cell(1, Some(VerticalMergeState::Restart), Some(GREY))]),
            row(vec![cell(0, Some(VerticalMergeState::Continue), None)]),
            row(vec![cell(1, None, None)]),
        ];
        let table = crate::render::layout::table::layout_table(
            &rows,
            &[Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );

        assert_eq!(
            table.size.height,
            Pt::new(28.0),
            "span 14pt + row 2 at 14pt"
        );
        assert_eq!(
            shading_rects(&table.commands),
            vec![(0.0, 14.0)],
            "the span ends at the Continue row; row 2 is not part of it"
        );
    }

    // ── Buffer layering and the nil/override interaction (E5b#3) ─────────

    const RED: RgbColor = RgbColor { r: 255, g: 0, b: 0 };

    fn single(width: f32, color: RgbColor) -> TableBorderLine {
        TableBorderLine {
            width: Pt::new(width),
            color,
            style: crate::render::layout::table::types::TableBorderStyle::Single,
        }
    }

    fn all_edges(line: TableBorderLine) -> crate::render::layout::table::TableBorderConfig {
        crate::render::layout::table::TableBorderConfig {
            top: Some(line),
            bottom: Some(line),
            left: Some(line),
            right: Some(line),
            inside_h: Some(line),
            inside_v: Some(line),
        }
    }

    /// Index of the first command matching `pred`.
    fn index_of(cmds: &[DrawCommand], pred: impl Fn(&DrawCommand) -> bool) -> usize {
        cmds.iter()
            .position(pred)
            .expect("expected command not emitted")
    }

    /// The three buffers concatenate as shading → content → borders, and that
    /// order is load-bearing: a cell's background is painted before its
    /// neighbours' borders, so a background can never cover a shared edge, and
    /// text is never hidden under a background.
    ///
    /// Asserted on the *final* command list, which is what the painter walks —
    /// the buffers themselves are an implementation detail.
    #[test]
    fn commands_layer_shading_then_content_then_borders() {
        let rows = vec![row(vec![cell(1, None, Some(GREY))])];
        let table = crate::render::layout::table::layout_table(
            &rows,
            &[Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            Some(&all_edges(single(1.0, RED))),
            None,
            false,
        );

        let shading = index_of(
            &table.commands,
            |c| matches!(c, DrawCommand::Rect { color, .. } if *color == GREY),
        );
        let text = index_of(&table.commands, |c| matches!(c, DrawCommand::Text { .. }));
        let border = index_of(
            &table.commands,
            |c| matches!(c, DrawCommand::Rect { color, .. } if *color == RED),
        );

        assert!(
            shading < text && text < border,
            "expected shading({shading}) < content({text}) < borders({border})"
        );
    }

    /// §17.4.38: a continuation slice restores a top border that *resolution*
    /// removed — but never one the author removed with `<w:top w:val="nil"/>`.
    ///
    /// Both cells resolve to no top border, so both are candidates for the
    /// override; only the one without an explicit nil may take it. Driving
    /// `emit_table_rows` directly is what makes the two cases comparable in a
    /// single row.
    #[test]
    fn top_border_override_skips_a_cell_that_explicitly_suppressed_its_top() {
        use crate::render::layout::table::types::{CellBorderConfig, CellBorderOverride};

        let nil_top = CellBorderConfig {
            top: Some(CellBorderOverride::Suppress),
            bottom: None,
            left: None,
            right: None,
        };
        let mut suppressed = cell(1, None, None);
        suppressed.cell_borders = Some(nil_top);

        // Cell 0 says "no top border, deliberately"; cell 1 says nothing.
        let rows = vec![row(vec![suppressed, cell(1, None, None)])];
        // No table borders at all, so both cells resolve `top: None`.
        let measured = crate::render::layout::table::measure::measure_table_rows(
            &rows,
            &[Pt::new(50.0), Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );
        assert!(
            measured.rows[0]
                .borders
                .iter()
                .all(|b| b.top.line().is_none()),
            "both cells must resolve to no top border for this test to mean anything"
        );

        let mut commands = Vec::new();
        let mut content = Vec::new();
        let mut borders = Vec::new();
        let mut cursor_y = Pt::ZERO;
        emit_table_rows(
            &measured,
            &rows,
            0..1,
            &mut cursor_y,
            &mut TableCommandBuffers {
                commands: &mut commands,
                content_commands: &mut content,
                border_commands: &mut borders,
            },
            Some(single(3.0, RED)),
        );

        let restored_xs: Vec<f32> = borders
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Rect { rect, color } if *color == RED => Some(rect.origin.x.raw()),
                _ => None,
            })
            .collect();
        assert_eq!(
            restored_xs,
            vec![50.0],
            "only cell 1 (x=50) takes the override; cell 0 asked for no top border"
        );
    }

    /// The override applies only to the *first* row of the emitted range — a
    /// continuation slice gets one restored top edge, not one per row.
    #[test]
    fn top_border_override_applies_only_to_the_first_row_of_the_range() {
        let rows = vec![
            row(vec![cell(1, None, None)]),
            row(vec![cell(1, None, None)]),
        ];
        let measured = crate::render::layout::table::measure::measure_table_rows(
            &rows,
            &[Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        );

        let mut commands = Vec::new();
        let mut content = Vec::new();
        let mut borders = Vec::new();
        let mut cursor_y = Pt::ZERO;
        emit_table_rows(
            &measured,
            &rows,
            0..2,
            &mut cursor_y,
            &mut TableCommandBuffers {
                commands: &mut commands,
                content_commands: &mut content,
                border_commands: &mut borders,
            },
            Some(single(3.0, RED)),
        );

        let count = borders
            .iter()
            .filter(|c| matches!(c, DrawCommand::Rect { color, .. } if *color == RED))
            .count();
        assert_eq!(count, 1, "exactly one restored top edge, on row 0");
    }

    /// A row that ends a page slice but not the *table* keeps its reserved
    /// bottom-border gap.
    ///
    /// `has_reserved_bottom_gap` is derived from the table's row count, not the
    /// emitted range's — it has to be, because `measure_table_rows` reserves
    /// `border_gap_below` on exactly that condition and `cursor_y` advances by
    /// it. Deriving it per slice instead would inset the border into the last
    /// row of every page, leaving the reserved gap empty.
    ///
    /// Three 14pt rows with 2pt inside borders, split so rows 0-1 land on the
    /// first slice: row 1 ends that slice, and its border must still sit in the
    /// gap below it (y = 30..32), not inside its content box.
    #[test]
    fn a_row_ending_a_slice_keeps_its_reserved_bottom_gap() {
        let line = single(2.0, RED);
        let borders = crate::render::layout::table::TableBorderConfig {
            top: None,
            bottom: Some(line),
            left: None,
            right: None,
            inside_h: Some(line),
            inside_v: None,
        };
        let rows: Vec<TableRowInput> = (0..3).map(|_| row(vec![cell(1, None, None)])).collect();
        let slices = crate::render::layout::table::layout_table_paginated(
            &rows,
            &[Pt::new(50.0)],
            Pt::ZERO,
            Pt::new(14.0),
            Some(&borders),
            None,
            &crate::render::layout::table::TablePaginationConfig {
                available_height: Pt::new(34.0),
                page_height: Pt::new(34.0),
                suppress_first_row_top: false,
            },
        );
        assert_eq!(slices.len(), 2, "3 rows at 16pt each over a 34pt page");

        let border_bands: Vec<(f32, f32)> = slices[0]
            .commands
            .iter()
            .filter_map(|c| match c {
                DrawCommand::Rect { rect, color } if *color == RED => Some((
                    rect.origin.y.raw(),
                    rect.origin.y.raw() + rect.size.height.raw(),
                )),
                _ => None,
            })
            .collect();

        assert!(
            border_bands.contains(&(30.0, 32.0)),
            "row 1 ends the slice at y=30; its bottom border belongs in the \
             reserved gap 30..32, got {border_bands:?}"
        );
    }
}
