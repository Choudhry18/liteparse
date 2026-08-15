use crate::render::dimension::Pt;

use super::types::{
    CellLayoutEntry, MeasuredTable, TableCellInput, TableRowInput, VerticalMergeState,
};

/// A group of rows that must stay together during page splitting.
pub(super) struct RowGroup {
    pub(super) start: usize,
    pub(super) end: usize, // exclusive
    pub(super) height: Pt,
    /// §17.4.1: row content may be broken across pages. False if any row in
    /// the group has `cantSplit`, if the group is a vMerge span (multiple
    /// rows stacked here), or if any cell contains a nested table or floated
    /// content that would be ambiguous to split.
    pub(super) splittable: bool,
}

/// §17.4.14: compute column widths by scaling the declared `w:tblGrid` values
/// to the available width.
///
/// A grid that is absent *or unusable* falls back to equal distribution. Both
/// are the same situation: a grid summing to zero or less (every `w:gridCol`
/// zero, or negative from a malformed file) carries no proportions to scale,
/// and scaling it by any factor leaves every column at zero — cells laid out
/// at zero width, with their content unreadable rather than merely misplaced.
pub fn compute_column_widths(grid_cols: &[Pt], num_cols: usize, available_width: Pt) -> Vec<Pt> {
    let total: Pt = grid_cols.iter().copied().sum();
    if !grid_cols.is_empty() && total > Pt::ZERO {
        let scale = available_width / total;
        return grid_cols.iter().map(|w| *w * scale).collect();
    }
    // Prefer the caller's column count; fall back to the degenerate grid's own
    // length so a zero-summing grid still yields one column per `w:gridCol`.
    let n = if num_cols > 0 {
        num_cols
    } else {
        grid_cols.len()
    };
    if n > 0 {
        vec![available_width / n as f32; n]
    } else {
        Vec::new()
    }
}

/// Build atomic row groups for pagination.
///
/// Groups are formed by: vMerge groups (Restart through consecutive Continue
/// rows) and §17.4.1 cantSplit rows. Each group is an indivisible unit for
/// page-break decisions.
pub(super) fn build_row_groups(rows: &[TableRowInput], measured: &MeasuredTable) -> Vec<RowGroup> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let start = i;
        i = row_group_end(rows, start);
        let height: Pt = measured.rows[start..i]
            .iter()
            .map(|mr| mr.height + mr.border_gap_below)
            .sum();

        // §17.4.1: a row may be split across pages unless it opts out via
        // `cantSplit` or lives inside a vMerge span (grouping multiple rows).
        // Nested tables inside cells aren't splittable either — the cell's
        // commands would be hard to bisect cleanly.
        let is_vmerge_span = i - start > 1;
        let any_cant_split = rows[start..i].iter().any(|r| r.cant_split == Some(true));
        let has_nested_table = rows[start..i]
            .iter()
            .flat_map(|r| r.cells.iter())
            .any(cell_has_nested_table);
        let splittable = !is_vmerge_span && !any_cant_split && !has_nested_table;

        groups.push(RowGroup {
            start,
            end: i,
            height,
            splittable,
        });
    }
    groups
}

/// Return the exclusive end of the paginator's atomic row group at `start`.
pub(super) fn row_group_end(rows: &[TableRowInput], start: usize) -> usize {
    let mut end = start + 1;
    while end < rows.len()
        && rows[end]
            .cells
            .iter()
            .any(|cell| cell.vertical_merge == Some(VerticalMergeState::Continue))
    {
        end += 1;
    }
    end
}

fn cell_has_nested_table(cell: &TableCellInput) -> bool {
    use crate::render::layout::section::LayoutBlock;
    cell.blocks
        .iter()
        .any(|b| matches!(b, LayoutBlock::Table { .. }))
}

/// §17.4.85: grow the rows of each vertical merge group so the `Restart`
/// cell's content fits within their combined height. The shortfall is spread
/// **evenly** over the spanned rows.
///
/// Even distribution is a choice, not a spec rule, and ECMA-376 cannot settle
/// it. All three places a rule would have to live were checked: §17.4.85
/// `vMerge` defines which cells merge and carries no height language at all;
/// §17.4.81 `trHeight`/`auto` defers to "the height required by its contents"
/// without ever defining "contents" for a cell that spans rows; and §17.4.21
/// `hideMark` — the spec's only row-height *rule* — says a row's height is
/// "determined by the height of all glyphs in all cells in that row", without
/// mentioning `vMerge`.
///
/// §17.4.21 constrains the answer without giving it. The model it describes is
/// a **per-row maximum** over that row's cells, not a budget divided among
/// rows, and even distribution (`overflow / rows`) is not expressible as a
/// per-row maximum of anything — so of the candidates it is the one the spec's
/// own sentence structurally disfavours. It does *not* follow that the restart
/// row takes the excess: applied literally to a span, §17.4.21 sizes the
/// restart row to the whole content while each `continue` row still carries its
/// own end-of-cell mark, making the merged box taller than its content by the
/// sum of the continue rows. No implementation does that, which is the tell
/// that the sentence was written without merged cells in mind. The spec is
/// silent by omission, not by implication.
///
/// So even distribution is disfavoured and last-row and first-row both remain
/// open; last-row has the better structural argument (a single-pass
/// top-to-bottom sizer can only enforce a span's total once the span closes)
/// but no spec text. The choice is *observable* — it changes rendered output on
/// one real corpus document — so settling it needs a Word-exported PDF of a
/// two-row vertical merge whose restart cell overflows both rows. Until then
/// the behaviour is pinned by `expand_spreads_overflow_across_the_merge_span`,
/// so it cannot change silently.
///
/// A lone `Restart` with no `Continue` below it is not a span, and is sized by
/// the normal row-height path in `measure_table_rows` instead.
pub(super) fn expand_rows_for_vmerge(
    rows: &[TableRowInput],
    row_cell_layouts: &[Vec<CellLayoutEntry>],
    row_heights: &mut [Pt],
) {
    for (row_idx, row) in rows.iter().enumerate() {
        for (cell_ci, cell) in row.cells.iter().enumerate() {
            if cell.vertical_merge != Some(VerticalMergeState::Restart) {
                continue;
            }

            let entry = &row_cell_layouts[row_idx][cell_ci];
            let content_h = entry.layout.content_height + cell.margins.vertical();

            // Find last row in this merge group.
            let mut last_merged_row = row_idx;
            for (r, row_below) in rows.iter().enumerate().skip(row_idx + 1) {
                if is_vmerge_continue(row_below, entry.grid_col) {
                    last_merged_row = r;
                } else {
                    break;
                }
            }
            if last_merged_row == row_idx {
                continue;
            }

            // Distribute overflow evenly across all rows in the merge group.
            let spanned: Pt = row_heights[row_idx..=last_merged_row].iter().copied().sum();
            if content_h > spanned {
                let overflow = content_h - spanned;
                let num_rows = (last_merged_row - row_idx + 1) as f32;
                let per_row = overflow / num_rows;
                for h in &mut row_heights[row_idx..=last_merged_row] {
                    *h += per_row;
                }
            }
        }
    }
}

/// Find the cell in a row that covers the given absolute grid column index.
///
/// Returns `None` for grid columns inside the row's `gridBefore` / `gridAfter`
/// regions (§17.4.17 / §17.4.16) — those columns have no cell in this row.
pub(super) fn find_cell_at_grid_col(
    row: &TableRowInput,
    target_grid_col: usize,
) -> Option<&TableCellInput> {
    let mut col = row.grid_before as usize;
    if target_grid_col < col {
        return None;
    }
    for cell in &row.cells {
        let span = cell.grid_span.max(1) as usize;
        if target_grid_col < col + span {
            return Some(cell);
        }
        col += span;
    }
    None
}

/// Check if the cell at `grid_col` in `row` is a vMerge Continue cell.
pub(super) fn is_vmerge_continue(row: &TableRowInput, grid_col: usize) -> bool {
    find_cell_at_grid_col(row, grid_col)
        .is_some_and(|c| c.vertical_merge == Some(VerticalMergeState::Continue))
}

/// Return the cell index (not grid column) for the cell covering `grid_col`.
/// Returns `None` for grid columns inside the row's gridBefore/gridAfter regions.
pub(super) fn cell_index_at_grid_col(row: &TableRowInput, target_grid_col: usize) -> Option<usize> {
    let mut col = row.grid_before as usize;
    if target_grid_col < col {
        return None;
    }
    for (i, cell) in row.cells.iter().enumerate() {
        let span = cell.grid_span.max(1) as usize;
        if target_grid_col < col + span {
            return Some(i);
        }
        col += span;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_column_widths ────────────────────────────────────────────

    #[test]
    fn equal_distribution_when_no_grid() {
        let widths = compute_column_widths(&[], 3, Pt::new(300.0));
        assert_eq!(widths.len(), 3);
        assert_eq!(widths[0].raw(), 100.0);
        assert_eq!(widths[1].raw(), 100.0);
        assert_eq!(widths[2].raw(), 100.0);
    }

    #[test]
    fn grid_cols_scaled_to_fit() {
        let grid = vec![Pt::new(100.0), Pt::new(200.0)];
        let widths = compute_column_widths(&grid, 2, Pt::new(600.0));
        // Scale = 600/300 = 2.0
        assert_eq!(widths[0].raw(), 200.0);
        assert_eq!(widths[1].raw(), 400.0);
    }

    #[test]
    fn grid_cols_already_fit() {
        let grid = vec![Pt::new(150.0), Pt::new(150.0)];
        let widths = compute_column_widths(&grid, 2, Pt::new(300.0));
        assert_eq!(widths[0].raw(), 150.0);
        assert_eq!(widths[1].raw(), 150.0);
    }

    #[test]
    fn zero_cols_empty_result() {
        let widths = compute_column_widths(&[], 0, Pt::new(300.0));
        assert!(widths.is_empty());
    }

    /// §17.4.14: a grid that sums to zero carries no proportions, so it is
    /// treated as absent. Scaling it instead leaves every column at zero and
    /// every cell laying out at zero width.
    #[test]
    fn zero_summing_grid_falls_back_to_equal_distribution() {
        let grid = vec![Pt::ZERO, Pt::ZERO, Pt::ZERO];
        let widths = compute_column_widths(&grid, 3, Pt::new(300.0));
        assert_eq!(
            widths.iter().map(|w| w.raw()).collect::<Vec<_>>(),
            vec![100.0, 100.0, 100.0]
        );
    }

    /// Same for a negative total, which only a malformed file produces — the
    /// old `scale = 1.0` fallback passed the negative widths straight through.
    #[test]
    fn negative_grid_total_falls_back_to_equal_distribution() {
        let grid = vec![Pt::new(-40.0), Pt::new(10.0)];
        let widths = compute_column_widths(&grid, 2, Pt::new(300.0));
        assert_eq!(
            widths.iter().map(|w| w.raw()).collect::<Vec<_>>(),
            vec![150.0, 150.0],
            "no column may come out negative"
        );
    }

    /// With no caller column count, the degenerate grid's own length is used
    /// so the table still has the right number of columns.
    #[test]
    fn zero_summing_grid_uses_its_own_length_when_num_cols_is_zero() {
        let grid = vec![Pt::ZERO, Pt::ZERO];
        let widths = compute_column_widths(&grid, 0, Pt::new(300.0));
        assert_eq!(
            widths.iter().map(|w| w.raw()).collect::<Vec<_>>(),
            vec![150.0, 150.0]
        );
    }

    // ── grid_before lookups ──────────────────────────────────────────────────────────────────────────────

    use super::super::types::CellVAlign;
    use crate::render::geometry::PtEdgeInsets;

    fn empty_cell(grid_span: u32) -> TableCellInput {
        TableCellInput {
            blocks: Vec::new(),
            margins: PtEdgeInsets::ZERO,
            grid_span,
            shading: None,
            cell_borders: None,
            vertical_merge: None,
            vertical_align: CellVAlign::Top,
        }
    }

    fn row_with_offsets(cells: Vec<TableCellInput>, grid_before: u32) -> TableRowInput {
        TableRowInput {
            cells,
            height_rule: None,
            is_header: None,
            cant_split: None,
            grid_before,
            border_overrides: None,
        }
    }

    #[test]
    fn find_cell_skips_grid_before() {
        // §17.4.17: gridBefore=1 means cell 0 starts at grid_col 1.
        // §17.4.16: gridAfter=1 means the row leaves grid_col 3 empty.
        let row = row_with_offsets(vec![empty_cell(1), empty_cell(1)], 1);
        assert!(find_cell_at_grid_col(&row, 0).is_none());
        assert!(find_cell_at_grid_col(&row, 1).is_some());
        assert!(find_cell_at_grid_col(&row, 2).is_some());
        assert!(find_cell_at_grid_col(&row, 3).is_none());
    }

    #[test]
    fn cell_index_at_grid_col_skips_grid_before() {
        let row = row_with_offsets(vec![empty_cell(1), empty_cell(1)], 1);
        assert_eq!(cell_index_at_grid_col(&row, 0), None);
        assert_eq!(cell_index_at_grid_col(&row, 1), Some(0));
        assert_eq!(cell_index_at_grid_col(&row, 2), Some(1));
        assert_eq!(cell_index_at_grid_col(&row, 3), None);
    }

    #[test]
    fn find_cell_with_grid_span_after_grid_before() {
        // gridBefore=2; one cell with span=2 should occupy grid cols 2 and 3.
        let row = row_with_offsets(vec![empty_cell(2)], 2);
        assert!(find_cell_at_grid_col(&row, 0).is_none());
        assert!(find_cell_at_grid_col(&row, 1).is_none());
        assert!(find_cell_at_grid_col(&row, 2).is_some());
        assert!(find_cell_at_grid_col(&row, 3).is_some());
        assert!(find_cell_at_grid_col(&row, 4).is_none());
    }

    // ── §17.4.85 row_group_end / §17.4.1 build_row_groups ────────────────
    //
    // These three functions decide every table page break and the whole
    // vertical-merge height distribution, and had no direct tests — the hole
    // a zero-height row was found hiding in.

    use super::super::types::MeasuredRow;
    use crate::render::layout::cell::CellLayout;
    use crate::render::layout::section::LayoutBlock;

    fn merged_cell(vmerge: Option<VerticalMergeState>) -> TableCellInput {
        TableCellInput {
            vertical_merge: vmerge,
            ..empty_cell(1)
        }
    }

    fn plain_row(cells: Vec<TableCellInput>) -> TableRowInput {
        row_with_offsets(cells, 0)
    }

    /// A `MeasuredTable` carrying only the per-row heights `build_row_groups`
    /// reads — `(height, border_gap_below)` pairs.
    fn measured(rows: &[(f32, f32)]) -> MeasuredTable {
        MeasuredTable {
            rows: rows
                .iter()
                .map(|&(h, gap)| MeasuredRow {
                    entries: Vec::new(),
                    borders: Vec::new(),
                    height: Pt::new(h),
                    leading_gap: Pt::ZERO,
                    border_gap_below: Pt::new(gap),
                })
                .collect(),
            table_width: Pt::new(100.0),
        }
    }

    #[test]
    fn row_group_end_is_one_row_when_nothing_continues() {
        let rows = vec![
            plain_row(vec![merged_cell(None)]),
            plain_row(vec![merged_cell(None)]),
        ];
        assert_eq!(row_group_end(&rows, 0), 1);
        assert_eq!(row_group_end(&rows, 1), 2, "last row still yields len");
    }

    #[test]
    fn row_group_end_spans_consecutive_continue_rows() {
        let rows = vec![
            plain_row(vec![merged_cell(Some(VerticalMergeState::Restart))]),
            plain_row(vec![merged_cell(Some(VerticalMergeState::Continue))]),
            plain_row(vec![merged_cell(Some(VerticalMergeState::Continue))]),
            plain_row(vec![merged_cell(None)]),
        ];
        assert_eq!(row_group_end(&rows, 0), 3, "restart + two continues");
        assert_eq!(row_group_end(&rows, 3), 4);
    }

    /// A `Continue` anywhere in the row extends the group — the merge need not
    /// be in the first column.
    #[test]
    fn row_group_end_triggers_on_a_continue_in_any_column() {
        let rows = vec![
            plain_row(vec![merged_cell(None), merged_cell(None)]),
            plain_row(vec![
                merged_cell(None),
                merged_cell(Some(VerticalMergeState::Continue)),
            ]),
        ];
        assert_eq!(row_group_end(&rows, 0), 2);
    }

    #[test]
    fn build_row_groups_treats_plain_rows_as_separate_splittable_groups() {
        let rows = vec![
            plain_row(vec![merged_cell(None)]),
            plain_row(vec![merged_cell(None)]),
        ];
        let groups = build_row_groups(&rows, &measured(&[(20.0, 0.0), (30.0, 0.0)]));
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.splittable));
        assert_eq!(groups[0].height.raw(), 20.0);
        assert_eq!(groups[1].height.raw(), 30.0);
    }

    /// §17.4.38: the gap left for a row's bottom border counts toward the
    /// group height, or a group would be judged to fit in less space than it
    /// occupies.
    #[test]
    fn build_row_groups_height_includes_the_border_gap_below() {
        let rows = vec![plain_row(vec![merged_cell(None)])];
        let groups = build_row_groups(&rows, &measured(&[(20.0, 3.0)]));
        assert_eq!(groups[0].height.raw(), 23.0);
    }

    /// §17.4.85: a merge span is indivisible, so its group covers every row
    /// and is not splittable.
    #[test]
    fn build_row_groups_marks_a_vmerge_span_unsplittable() {
        let rows = vec![
            plain_row(vec![merged_cell(Some(VerticalMergeState::Restart))]),
            plain_row(vec![merged_cell(Some(VerticalMergeState::Continue))]),
        ];
        let groups = build_row_groups(&rows, &measured(&[(20.0, 0.0), (20.0, 0.0)]));
        assert_eq!(groups.len(), 1);
        assert_eq!((groups[0].start, groups[0].end), (0, 2));
        assert_eq!(groups[0].height.raw(), 40.0, "span height is the sum");
        assert!(!groups[0].splittable);
    }

    /// §17.4.1 `cantSplit` — a single row can opt out on its own.
    #[test]
    fn build_row_groups_marks_a_cant_split_row_unsplittable() {
        let mut row = plain_row(vec![merged_cell(None)]);
        row.cant_split = Some(true);
        let groups = build_row_groups(&[row], &measured(&[(20.0, 0.0)]));
        assert!(!groups[0].splittable);
    }

    /// A nested table's commands can't be bisected cleanly, so its row is
    /// atomic even without `cantSplit`.
    #[test]
    fn build_row_groups_marks_a_nested_table_row_unsplittable() {
        let nested = TableCellInput {
            blocks: vec![LayoutBlock::Table {
                rows: Vec::new(),
                col_widths: Vec::new(),
                cell_spacing: Pt::ZERO,
                border_config: None,
                indent: Pt::ZERO,
                alignment: None,
                float_info: None,
                style_id: None,
            }],
            ..empty_cell(1)
        };
        let groups = build_row_groups(&[plain_row(vec![nested])], &measured(&[(20.0, 0.0)]));
        assert!(!groups[0].splittable);
    }

    // ── §17.4.85 expand_rows_for_vmerge ──────────────────────────────────

    fn layout_entry(content_height: f32, grid_col: usize) -> CellLayoutEntry {
        CellLayoutEntry {
            layout: CellLayout {
                commands: Vec::new(),
                content_height: Pt::new(content_height),
                lines: Vec::new(),
            },
            cell_x: Pt::ZERO,
            cell_w: Pt::new(100.0),
            grid_col,
        }
    }

    /// The core §17.4.85 behaviour: when a `Restart` cell's content exceeds the
    /// rows it spans, the shortfall is spread across them.
    ///
    /// Note this pins **even distribution**, which is what the code does and
    /// not obviously what Word does — see `expand_rows_for_vmerge` for why
    /// ECMA-376 cannot settle it and why even distribution is in fact the
    /// candidate §17.4.21 structurally disfavours. This test will need updating
    /// if a Word reference render settles it the other way; it exists to make
    /// that a deliberate change rather than a silent one.
    #[test]
    fn expand_spreads_overflow_across_the_merge_span() {
        let rows = vec![
            plain_row(vec![merged_cell(Some(VerticalMergeState::Restart))]),
            plain_row(vec![merged_cell(Some(VerticalMergeState::Continue))]),
        ];
        let layouts = vec![vec![layout_entry(84.0, 0)], vec![layout_entry(0.0, 0)]];
        let mut heights = [Pt::new(14.0), Pt::new(14.0)];

        expand_rows_for_vmerge(&rows, &layouts, &mut heights);

        // 84pt of content over a 28pt span → 56pt spread over 2 rows.
        assert_eq!(heights[0].raw(), 42.0);
        assert_eq!(heights[1].raw(), 42.0);
        assert_eq!(
            heights[0].raw() + heights[1].raw(),
            84.0,
            "the span must total the restart cell's content height"
        );
    }

    #[test]
    fn expand_leaves_heights_alone_when_the_content_already_fits() {
        let rows = vec![
            plain_row(vec![merged_cell(Some(VerticalMergeState::Restart))]),
            plain_row(vec![merged_cell(Some(VerticalMergeState::Continue))]),
        ];
        let layouts = vec![vec![layout_entry(10.0, 0)], vec![layout_entry(0.0, 0)]];
        let mut heights = [Pt::new(30.0), Pt::new(30.0)];

        expand_rows_for_vmerge(&rows, &layouts, &mut heights);

        assert_eq!(heights[0].raw(), 30.0);
        assert_eq!(heights[1].raw(), 30.0);
    }

    /// A `Restart` with nothing continuing under it is left completely alone —
    /// the early return when the group is a single row.
    ///
    /// This is the companion to the fix in `measure.rs`: because this function
    /// contributes nothing here, the row's height has to come from the normal
    /// `max_height` path instead, or the row collapses to zero while still
    /// drawing its content.
    #[test]
    fn expand_does_nothing_for_a_lone_restart() {
        let rows = vec![plain_row(vec![merged_cell(Some(
            VerticalMergeState::Restart,
        ))])];
        let layouts = vec![vec![layout_entry(84.0, 0)]];
        let mut heights = [Pt::new(14.0)];

        expand_rows_for_vmerge(&rows, &layouts, &mut heights);

        assert_eq!(
            heights[0].raw(),
            14.0,
            "a one-row span is not expanded — measure.rs must size this row"
        );
    }

    /// The span is tracked by **grid column**, not by cell index.
    ///
    /// §17.4.17 `gridBefore` is what makes those differ: the lower row's
    /// *first* cell is a `Continue`, but it sits in grid column 1, so it does
    /// not continue a merge that started in column 0. Indexing by cell
    /// position instead would wrongly join them — and a test where the restart
    /// happens to sit at `grid_col == cell_index` cannot tell the two apart.
    #[test]
    fn expand_matches_continues_by_grid_column_not_cell_index() {
        let rows = vec![
            plain_row(vec![
                merged_cell(Some(VerticalMergeState::Restart)), // grid col 0
                merged_cell(None),                              // grid col 1
            ]),
            // gridBefore=1: this row's cells[0] is at grid column 1, not 0.
            row_with_offsets(vec![merged_cell(Some(VerticalMergeState::Continue))], 1),
        ];
        let layouts = vec![
            vec![layout_entry(84.0, 0), layout_entry(0.0, 1)],
            vec![layout_entry(0.0, 1)],
        ];
        let mut heights = [Pt::new(14.0), Pt::new(14.0)];

        expand_rows_for_vmerge(&rows, &layouts, &mut heights);

        assert_eq!(
            [heights[0].raw(), heights[1].raw()],
            [14.0, 14.0],
            "the Continue is in column 1; column 0's restart has no continuation"
        );
    }
}
