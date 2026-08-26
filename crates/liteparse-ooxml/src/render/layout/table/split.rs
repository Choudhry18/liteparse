//! Row splitting for page pagination.
//!
//! §17.4.1: a row without `cantSplit` may have its content broken across
//! page boundaries. This module derives safe cut points from a row's
//! already-laid-out cell commands and partitions those commands into a
//! first slice (stays on the current page) and a second slice (flows to
//! the next page). Both halves preserve the cell's top/bottom margins so
//! the text doesn't collide with the cut-edge border.

use crate::render::dimension::Pt;
use crate::render::layout::draw_command::DrawCommand;
use crate::render::layout::section::CellLine;

use super::borders::CellBorders;
use super::types::{CellLayoutEntry, MeasuredRow, TableRowInput};

/// Options for finding a row cut.
pub(super) struct RowCutInput<'a> {
    pub(super) mr: &'a MeasuredRow,
    pub(super) row: &'a TableRowInput,
    /// Space available for the row on the current page, measured from the
    /// row's top edge.
    pub(super) available: Pt,
}

/// Per-cell split decision: where to partition the commands and how far
/// to shift the tail commands so the continuation half has the cell's
/// natural top margin.
struct CellCut {
    /// Partition threshold: commands with primary-Y strictly less than
    /// this value stay on the first half (cell-box coordinates).
    content_cut_y: Pt,
    /// Partition threshold for the [`CellLine`] cut model, in cell-content
    /// coordinates (i.e. `content_cut_y - margin_top`): lines whose box top is
    /// strictly less stay on the first half.
    line_cut_y: Pt,
    /// Amount by which second-half commands (and continuation line tops) are
    /// shifted up so that the first surviving line lands at the continuation
    /// cell's natural top margin.
    shift: Pt,
}

impl CellCut {
    /// "Don't split" sentinel — all commands and lines stay on the first half.
    fn keep_all() -> Self {
        Self {
            content_cut_y: Pt::new(f32::INFINITY),
            line_cut_y: Pt::new(f32::INFINITY),
            shift: Pt::ZERO,
        }
    }
}

/// Aggregate split decision for a whole row.
pub(super) struct SplitCut {
    /// Row-level first-half height (max across cells of per-cell first
    /// half heights). Each cell's visible box in the first half extends
    /// from the row top to this Y.
    first_half_height: Pt,
    /// One per cell, in the same order as `row.cells`.
    cells: Vec<CellCut>,
}

/// Pick a row cut that fits in `available`, honoring each cell's top and
/// bottom margins so the cut-edge border gets the natural padding Word
/// preserves on split cells.
///
/// Returns `None` when no cell has at least one line that fits within
/// `available - margins.vertical()`.
pub(super) fn find_row_cut(input: &RowCutInput<'_>) -> Option<SplitCut> {
    let mut cells: Vec<CellCut> = Vec::with_capacity(input.row.cells.len());
    let mut first_half_height = Pt::ZERO;
    let mut any_fits = false;
    // Cells that can't be split text-wise (too few baselines or
    // image/shape-only cells) force the row to leave room for their full
    // visible height on the first half; otherwise their content (image,
    // path, etc.) would overflow the cut-edge border.
    let mut non_splittable_heights: Vec<Pt> = Vec::with_capacity(input.row.cells.len());

    for (entry, cell) in input.mr.entries.iter().zip(&input.row.cells) {
        match cut_for_cell(
            entry,
            cell.margins.top,
            cell.margins.bottom,
            input.available,
        ) {
            Some((cut, half_h)) => {
                any_fits = true;
                if half_h > first_half_height {
                    first_half_height = half_h;
                }
                cells.push(cut);
                non_splittable_heights.push(Pt::ZERO);
            }
            None => {
                cells.push(CellCut::keep_all());
                // Cell's natural visible height (content + vertical margins)
                // must fit on the first half — we can't cut through an
                // image or shape.
                let required = entry.layout.content_height + cell.margins.top + cell.margins.bottom;
                non_splittable_heights.push(required);
            }
        }
    }

    if !any_fits {
        return None;
    }

    // Raise first_half_height so every non-splittable cell fits. If that
    // exceeds `available`, splitting isn't safe — caller should spill the
    // whole row to the next page.
    let non_splittable_max = non_splittable_heights
        .iter()
        .copied()
        .fold(Pt::ZERO, Pt::max);
    if non_splittable_max > first_half_height {
        first_half_height = non_splittable_max;
    }
    if first_half_height > input.available {
        return None;
    }
    Some(SplitCut {
        first_half_height,
        cells,
    })
}

/// For a single cell, choose the largest legal prefix of lines that fits in
/// `available`, honoring the cell paragraphs' §17.3.1.14 keepLines,
/// §17.3.1.44 widow/orphan control, and §17.3.1.15 keepNext — the same policy
/// body across-page splitting applies. Returns the `CellCut` and the first-half
/// height this cell needs: the retained content plus the cell's top and bottom
/// margins, so the cut edge gets the natural padding Word preserves (variant 2
/// of the OOXML split-edge options; §17.4.40 re-applied at the cut edge).
///
/// Returns `None` when the cell exposes no legal cut point within `available`
/// (empty/one-line, image/shape-only, keepLines, or every fitting cut would
/// strand a widow) — the caller then keeps the cell whole on the first half.
fn cut_for_cell(
    entry: &CellLayoutEntry,
    margin_top: Pt,
    margin_bottom: Pt,
    available: Pt,
) -> Option<(CellCut, Pt)> {
    // Space available for the content between both margins, since the first
    // half reproduces the cell's natural layout (top margin, retained lines,
    // bottom margin) — same as a non-split cell rendering those lines.
    let budget = available - margin_top - margin_bottom;
    if budget <= Pt::ZERO {
        return None;
    }

    let cont_top = largest_legal_cut(&entry.layout.lines, budget)?;
    // Load-bearing for **termination**, not tidiness.
    //
    // `cont_top` becomes this cell's `shift`, and `split_row_at` derives
    // `second.height = mr.height - max(shift)`. `mod.rs`'s continuation loop
    // re-splits `second` until it fits, so a cut with `shift == 0` would hand
    // back a continuation identical to its input and spin forever. This check
    // is what makes "`find_row_cut` returned `Some`" imply "the row strictly
    // shrank", which is the loop's whole termination argument.
    //
    // Today it is unreachable: `largest_legal_cut` returns a line's `top_y`,
    // and the stacker emits strictly increasing tops, so offset 0 never comes
    // back for a real cell. That makes this a backstop for an invariant owned
    // by another module rather than by this one — keep it, and note that only
    // `zero_offset_cut_is_rejected_so_the_shift_is_never_zero` covers it,
    // because it builds the degenerate entry by hand. Nothing routed through
    // `measure_table_rows` can.
    if cont_top <= Pt::ZERO {
        return None;
    }

    // `cont_top` is the box top of the first continuation line, in cell-content
    // coordinates — equivalently the content height the first half retains.
    // Shifting the continuation up by it lands its first line at the cell's top
    // margin; the partition threshold sits at `margin_top + cont_top` (draw
    // commands are margin-shifted, the cut model is not).
    let shift = cont_top;
    let half_h = cont_top + margin_top + margin_bottom;
    Some((
        CellCut {
            content_cut_y: margin_top + cont_top,
            line_cut_y: cont_top,
            shift,
        },
        half_h,
    ))
}

/// The largest legal cut offset (`cont_top`, cell-content coords) not exceeding
/// `budget`: the most content the first half may retain while leaving a
/// spec-legal split point. A cut "after line L" keeps `lines[0..=L]` and flows
/// the rest to the continuation.
///
/// Legality (§17.3.1.14 / §17.3.1.44 / §17.3.1.15):
/// - **interior** (L and L+1 share a paragraph): illegal if the paragraph is
///   internally atomic (keepLines / bordered / shaded / drop cap); otherwise,
///   under widow control, both sides must keep `>= 2` of the paragraph's lines.
/// - **boundary** (L and L+1 are different paragraphs): legal unless the earlier
///   paragraph is keepNext (bound to the block that follows).
///
/// Line box tops increase monotonically, so the scan stops once a candidate
/// exceeds `budget`.
fn largest_legal_cut(lines: &[CellLine], budget: Pt) -> Option<Pt> {
    let n = lines.len();
    if n < 2 {
        return None;
    }
    let mut best: Option<Pt> = None;
    for l in 0..n - 1 {
        let a = &lines[l];
        let b = &lines[l + 1];
        let cont_top = b.top_y;
        if cont_top > budget {
            break;
        }
        let legal = if a.para == b.para {
            if a.interior_atomic {
                false
            } else if a.widow_control {
                let first = lines.iter().position(|x| x.para == a.para).unwrap_or(l);
                let last = lines.iter().rposition(|x| x.para == a.para).unwrap_or(l);
                let head = l + 1 - first; // this paragraph's lines kept
                let tail = last - l; // this paragraph's lines continued
                head >= 2 && tail >= 2
            } else {
                true
            }
        } else {
            !a.keep_next
        };
        if legal {
            best = Some(best.map_or(cont_top, |x: Pt| x.max(cont_top)));
        }
    }
    best
}

/// A row cut into two halves. Each half is a full `MeasuredRow` ready to
/// pass to `emit_one_row`.
pub(super) struct SplitRow {
    pub(super) first: MeasuredRow,
    pub(super) second: MeasuredRow,
}

/// Split a row using `cut`. Each cell's commands are partitioned at the
/// cell's own `content_cut_y`; the tail is shifted so its first line
/// lands at the cell's natural `margin_top + ascent` in the
/// continuation.
///
/// Borders: Word preserves the full border box on each half. First half
/// keeps all original borders. Second half inherits a top border from
/// the original top if set, otherwise falls back to the original bottom
/// (same inside-horizontal style used by conflict resolution on
/// mid-table rows).
pub(super) fn split_row_at(mr: &MeasuredRow, cut: &SplitCut) -> SplitRow {
    let first_h = cut.first_half_height;
    // The largest shift across cells is the amount of content consumed by
    // the first half. Whatever's left in the original row height is what
    // the continuation needs to render (top/bottom margins are already
    // included in `mr.height`, so no additional padding is needed).
    let max_shift = cut.cells.iter().map(|c| c.shift).fold(Pt::ZERO, Pt::max);
    let second_h = (mr.height - max_shift).max(Pt::ZERO);

    let mut first_entries: Vec<CellLayoutEntry> = Vec::with_capacity(mr.entries.len());
    let mut second_entries: Vec<CellLayoutEntry> = Vec::with_capacity(mr.entries.len());

    for (entry, cc) in mr.entries.iter().zip(cut.cells.iter()) {
        let (first_cmds, second_cmds) =
            partition_commands(&entry.layout.commands, cc.content_cut_y, cc.shift);
        // Partition the cut model too, rebasing the continuation's line tops by
        // the same shift, so a further split of the continuation (mod.rs's
        // iterative loop) re-evaluates §17.3.1.44 widow control against the
        // lines that actually remain.
        let (first_lines, second_lines) =
            partition_lines(&entry.layout.lines, cc.line_cut_y, cc.shift);
        first_entries.push(CellLayoutEntry {
            layout: crate::render::layout::cell::CellLayout {
                commands: first_cmds,
                content_height: entry.layout.content_height.min(first_h),
                lines: first_lines,
            },
            cell_x: entry.cell_x,
            cell_w: entry.cell_w,
            grid_col: entry.grid_col,
        });
        second_entries.push(CellLayoutEntry {
            layout: crate::render::layout::cell::CellLayout {
                commands: second_cmds,
                content_height: (entry.layout.content_height - cc.shift).max(Pt::ZERO),
                lines: second_lines,
            },
            cell_x: entry.cell_x,
            cell_w: entry.cell_w,
            grid_col: entry.grid_col,
        });
    }

    let first_borders: Vec<CellBorders> = mr.borders.to_vec();
    let second_borders: Vec<CellBorders> = mr
        .borders
        .iter()
        .map(|b| CellBorders {
            // The continuation's top is the *cut* edge the split invented, not
            // the author's top edge, so it takes the row's bottom border
            // whenever the original top paints nothing. That deliberately
            // includes a suppressed top: `nil` on the row's own top edge says
            // nothing about a boundary that only exists because of the split.
            top: if b.top.line().is_some() {
                b.top
            } else {
                b.bottom
            },
            bottom: b.bottom,
            left: b.left,
            right: b.right,
        })
        .collect();

    SplitRow {
        // §17.4.44: both halves are row boxes placed against a cursor, so each
        // keeps the original leading gap — the continuation sits one spacing
        // below the top of the table area on its page, exactly as the first
        // half sits one spacing below the row above it.
        first: MeasuredRow {
            entries: first_entries,
            borders: first_borders,
            height: first_h,
            leading_gap: mr.leading_gap,
            border_gap_below: Pt::ZERO,
        },
        second: MeasuredRow {
            entries: second_entries,
            borders: second_borders,
            height: second_h,
            leading_gap: mr.leading_gap,
            border_gap_below: mr.border_gap_below,
        },
    }
}

/// Split a command list at `cut_y`. Commands whose primary Y < `cut_y`
/// go to the first half; the rest land in the second half shifted up by
/// `shift` so they start at the continuation cell's natural top margin.
fn partition_commands(
    commands: &[DrawCommand],
    cut_y: Pt,
    shift: Pt,
) -> (Vec<DrawCommand>, Vec<DrawCommand>) {
    let mut first = Vec::new();
    let mut second = Vec::new();
    for cmd in commands {
        if command_primary_y(cmd) < cut_y {
            first.push(cmd.clone());
        } else {
            let mut c = cmd.clone();
            c.shift_y(-shift);
            second.push(c);
        }
    }
    (first, second)
}

/// Partition the [`CellLine`] cut model at `cut_y` (cell-content coords).
/// Lines whose box top is `< cut_y` stay on the first half; the rest form the
/// continuation, with their tops rebased up by `shift` so the model tracks the
/// continuation's own coordinates for any further split.
fn partition_lines(lines: &[CellLine], cut_y: Pt, shift: Pt) -> (Vec<CellLine>, Vec<CellLine>) {
    let mut first = Vec::new();
    let mut second = Vec::new();
    for line in lines {
        if line.top_y < cut_y {
            first.push(line.clone());
        } else {
            let mut l = line.clone();
            l.top_y -= shift;
            second.push(l);
        }
    }
    (first, second)
}

/// The Y used to decide which side of the cut a command belongs to. For
/// `Text` we use the baseline; for rect/line/image we use the top edge.
fn command_primary_y(cmd: &DrawCommand) -> Pt {
    match cmd {
        DrawCommand::Text { position, .. } | DrawCommand::NamedDestination { position, .. } => {
            position.y
        }
        DrawCommand::Underline { line, .. } | DrawCommand::Line { line, .. } => line.start.y,
        DrawCommand::Image { rect, .. }
        | DrawCommand::EmojiCluster { rect, .. }
        | DrawCommand::Rect { rect, .. }
        | DrawCommand::LinkAnnotation { rect, .. }
        | DrawCommand::InternalLink { rect, .. } => rect.origin.y,
        DrawCommand::Path { origin, .. } => origin.y,
        // §17.3.1.19: a marked-content boundary has no y of its own. It takes
        // the top of the cell so it stays with the first half of a split — a
        // heading whose paragraph splits keeps its outline entry on the page
        // it starts on, which is the same rule the paragraph splitter applies.
        DrawCommand::Outline(_) => Pt::ZERO,
        // §20.1.7.6: a placement marker. A DOCX table's cell commands never
        // contain one, so this arm exists for exhaustiveness; taking the top
        // of the cell keeps a bracket with the half its shape starts in, the
        // same rule the outline marker follows.
        DrawCommand::Transform(_) => Pt::ZERO,
        // Pass routing, XLSX only — a DOCX cell never carries one either.
        DrawCommand::Float(_) => Pt::ZERO,
    }
}

#[cfg(test)]
mod tests {
    use super::super::borders::CellEdge;
    use super::*;
    use crate::render::geometry::PtEdgeInsets;
    use crate::render::layout::fragment::{FontProps, Fragment, TextMetrics};
    use crate::render::layout::paragraph::ParagraphStyle;
    use crate::render::layout::section::LayoutBlock;
    use crate::render::layout::table::types::{CellVAlign, TableCellInput};
    use crate::render::resolve::color::RgbColor;
    use std::rc::Rc;

    fn text_frag(text: &str) -> Fragment {
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

    /// One cell, `n` lines (each fragment wraps to its own line at width 40).
    fn row_n_lines(n: usize, margin: f32) -> TableRowInput {
        let m = PtEdgeInsets::new(
            Pt::new(margin),
            Pt::new(margin),
            Pt::new(margin),
            Pt::new(margin),
        );
        TableRowInput {
            cells: vec![TableCellInput {
                blocks: vec![LayoutBlock::Paragraph {
                    fragments: (0..n).map(|i| text_frag(&format!("L{i} "))).collect(),
                    style: ParagraphStyle::default(),
                    page_break_before: false,
                    footnotes: vec![],
                    floating_images: vec![],
                    floating_shapes: vec![],
                }],
                margins: m,
                grid_span: 1,
                shading: None,
                cell_borders: None,
                vertical_merge: None,
                vertical_align: CellVAlign::Top,
            }],
            height_rule: None,
            is_header: None,
            cant_split: None,
            grid_before: 0,
            border_overrides: None,
        }
    }

    fn measure(rows: &[TableRowInput]) -> super::super::types::MeasuredTable {
        super::super::measure::measure_table_rows(
            rows,
            &[Pt::new(40.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            false,
        )
    }

    /// **The termination invariant** for `mod.rs`'s continuation loop.
    ///
    /// That loop re-splits the continuation half until it fits, so it only
    /// terminates if every accepted cut strictly shrinks the row. Here that
    /// property is asserted directly, across a grid of line counts, available
    /// heights straddling the line-height and widow-control boundaries, and
    /// cell margins large enough to swallow the budget.
    ///
    /// If this ever fails, `layout_table_paginated` hangs — the same class of
    /// defect as the floating-table spillover loop (E4c).
    #[test]
    fn every_accepted_cut_strictly_shrinks_the_row() {
        for n_lines in [2usize, 3, 5, 12] {
            for avail in [1.0f32, 5.0, 13.9, 14.0, 14.1, 27.9, 28.0, 100.0, 1000.0] {
                for margin in [0.0f32, 3.0, 20.0] {
                    let rows = vec![row_n_lines(n_lines, margin)];
                    let measured = measure(&rows);
                    let input = RowCutInput {
                        mr: &measured.rows[0],
                        row: &rows[0],
                        available: Pt::new(avail),
                    };
                    let Some(cut) = find_row_cut(&input) else {
                        continue;
                    };
                    let parts = split_row_at(&measured.rows[0], &cut);
                    assert!(
                        parts.second.height < measured.rows[0].height,
                        "cut made no progress (lines={n_lines} avail={avail} \
                         margin={margin}): {:.2} -> {:.2}",
                        measured.rows[0].height.raw(),
                        parts.second.height.raw(),
                    );
                }
            }
        }
    }

    /// The mechanism behind the invariant: an accepted cut always carries a
    /// strictly positive shift, because `cut_for_cell` rejects `cont_top == 0`.
    /// `split_row_at` subtracts `max(shift)` from the row height, so a zero
    /// shift would return the continuation unchanged.
    #[test]
    fn accepted_cuts_carry_a_positive_shift() {
        let rows = vec![row_n_lines(6, 0.0)];
        let measured = measure(&rows);
        let cut = find_row_cut(&RowCutInput {
            mr: &measured.rows[0],
            row: &rows[0],
            available: Pt::new(60.0),
        })
        .expect("a 6-line row must be splittable at 60pt");
        let max_shift = cut.cells.iter().map(|c| c.shift).fold(Pt::ZERO, Pt::max);
        assert!(
            max_shift > Pt::ZERO,
            "shift must be positive or the continuation loop cannot progress"
        );
    }

    /// The `cont_top <= 0` guard, exercised directly.
    ///
    /// It cannot be reached through `measure_table_rows`: the stacker emits
    /// strictly increasing line tops, so `largest_legal_cut` never returns 0
    /// for a real cell. Deleting the guard therefore breaks no test that goes
    /// through the normal path — which is exactly why this one bypasses it and
    /// hands `cut_for_cell` a synthetic entry whose two lines share `top_y = 0`.
    ///
    /// Accepting that cut would give `shift == 0`, and `mod.rs`'s continuation
    /// loop would re-split an unchanged row forever. The guard backstops a
    /// stacker invariant this module cannot enforce, so its test must not
    /// depend on that invariant holding.
    #[test]
    fn zero_offset_cut_is_rejected_so_the_shift_is_never_zero() {
        let flat_line = |para: usize| CellLine {
            top_y: Pt::ZERO,
            para,
            interior_atomic: false,
            widow_control: false,
            keep_next: false,
        };
        let lines = vec![flat_line(0), flat_line(1)];

        // The cut model does offer offset 0 as "legal" ...
        assert_eq!(
            largest_legal_cut(&lines, Pt::new(100.0)),
            Some(Pt::ZERO),
            "two lines sharing a top expose a zero-offset cut"
        );

        // ... and `cut_for_cell` must refuse it anyway.
        let entry = CellLayoutEntry {
            layout: crate::render::layout::cell::CellLayout {
                commands: Vec::new(),
                content_height: Pt::new(28.0),
                lines,
            },
            cell_x: Pt::ZERO,
            cell_w: Pt::new(40.0),
            grid_col: 0,
        };
        assert!(
            cut_for_cell(&entry, Pt::ZERO, Pt::ZERO, Pt::new(100.0)).is_none(),
            "a zero-offset cut must be rejected - accepting it gives shift == 0 \
             and the continuation loop never progresses"
        );
    }

    /// A single line offers no cut at all (`n < 2`), so an unsplittable row
    /// spills whole rather than looping.
    #[test]
    fn fewer_than_two_lines_yields_no_cut() {
        assert_eq!(largest_legal_cut(&[], Pt::new(100.0)), None);
        let one = vec![CellLine {
            top_y: Pt::ZERO,
            para: 0,
            interior_atomic: false,
            widow_control: false,
            keep_next: false,
        }];
        assert_eq!(largest_legal_cut(&one, Pt::new(100.0)), None);
    }

    /// End-to-end bound: a 40-line row on a page that fits two lines produces
    /// 20 slices and halts. Pins that the loop consumes lines rather than
    /// merely shrinking by an epsilon.
    #[test]
    fn iterative_continuation_split_halts_with_a_bounded_slice_count() {
        use crate::render::layout::table::{TablePaginationConfig, layout_table_paginated};

        let rows = vec![row_n_lines(40, 0.0)];
        let slices = layout_table_paginated(
            &rows,
            &[Pt::new(40.0)],
            Pt::ZERO,
            Pt::new(14.0),
            None,
            None,
            &TablePaginationConfig {
                available_height: Pt::new(28.0), // exactly two 14pt lines
                page_height: Pt::new(28.0),
                suppress_first_row_top: false,
            },
        );
        assert_eq!(slices.len(), 20, "40 lines at 2 per page");
    }

    // ── Cut correctness (E5b#4) ──────────────────────────────────────────
    //
    // The tests above pin *termination*. These pin what the cut actually
    // produces: which lines land on each side, where the partition boundary
    // falls, that both halves keep their margins, and that a continuation
    // inherits a visible top edge.

    /// Text of every `Text` command, in order.
    fn texts(cmds: &[DrawCommand]) -> Vec<String> {
        cmds.iter()
            .filter_map(|c| match c {
                DrawCommand::Text { text, .. } => Some(text.to_string()),
                _ => None,
            })
            .collect()
    }

    /// Baseline y of every `Text` command, in order.
    fn baselines(cmds: &[DrawCommand]) -> Vec<f32> {
        cmds.iter()
            .filter_map(|c| match c {
                DrawCommand::Text { position, .. } => Some(position.y.raw()),
                _ => None,
            })
            .collect()
    }

    /// A 6-line cell cut with room for 4 lines splits 4 / 2, and each line
    /// appears on exactly one side — no duplication, no loss.
    ///
    /// The cut model and the draw commands are partitioned by *separate*
    /// thresholds (`line_cut_y` in content coordinates, `content_cut_y` in
    /// cell-box coordinates, differing by the top margin), so the two can
    /// disagree. Asserting the actual line texts is what catches that.
    #[test]
    fn cut_partitions_lines_without_duplicating_or_dropping_any() {
        let rows = vec![row_n_lines(6, 0.0)];
        let measured = measure(&rows);
        let cut = find_row_cut(&RowCutInput {
            mr: &measured.rows[0],
            row: &rows[0],
            available: Pt::new(4.0 * 14.0),
        })
        .expect("6 lines with room for 4 must be splittable");
        let parts = split_row_at(&measured.rows[0], &cut);

        let first = texts(&parts.first.entries[0].layout.commands);
        let second = texts(&parts.second.entries[0].layout.commands);

        assert_eq!(first.len() + second.len(), 6, "no line lost or duplicated");
        assert_eq!(first, vec!["L0 ", "L1 ", "L2 ", "L3 "]);
        assert_eq!(second, vec!["L4 ", "L5 "]);
    }

    /// §17.4.40: the continuation's first line sits at the cell's natural top
    /// margin, not at the cut offset — the tail is shifted up by exactly the
    /// content the first half retained, so the cut edge keeps its padding.
    #[test]
    fn continuation_first_line_is_rebased_to_the_cell_top() {
        let rows = vec![row_n_lines(6, 0.0)];
        let measured = measure(&rows);
        let cut = find_row_cut(&RowCutInput {
            mr: &measured.rows[0],
            row: &rows[0],
            available: Pt::new(4.0 * 14.0),
        })
        .expect("splittable");
        let parts = split_row_at(&measured.rows[0], &cut);

        let before = baselines(&measured.rows[0].entries[0].layout.commands);
        let first = baselines(&parts.first.entries[0].layout.commands);
        let second = baselines(&parts.second.entries[0].layout.commands);

        assert_eq!(
            first,
            before[..4],
            "first half keeps its original baselines"
        );
        // The tail is shifted up by the retained content height (4 lines).
        let shift = before[4] - second[0];
        assert!(
            (shift - 4.0 * 14.0).abs() < 0.01,
            "tail shifted by the retained content height, got {shift}"
        );
        assert_eq!(
            second[0], before[0],
            "the continuation's first line lands where the original first line did"
        );
    }

    /// The continuation's line *model* is rebased by the same shift as its
    /// commands, so a further split re-evaluates §17.3.1.44 widow control
    /// against the lines that actually remain rather than stale offsets.
    #[test]
    fn continuation_line_model_is_rebased_with_the_commands() {
        let rows = vec![row_n_lines(6, 0.0)];
        let measured = measure(&rows);
        let cut = find_row_cut(&RowCutInput {
            mr: &measured.rows[0],
            row: &rows[0],
            available: Pt::new(4.0 * 14.0),
        })
        .expect("splittable");
        let parts = split_row_at(&measured.rows[0], &cut);

        let second_lines = &parts.second.entries[0].layout.lines;
        assert_eq!(second_lines.len(), 2, "two lines continue");
        assert_eq!(
            second_lines[0].top_y,
            Pt::ZERO,
            "the continuation's first line starts at content offset 0"
        );
        assert_eq!(parts.first.entries[0].layout.lines.len(), 4);
    }

    /// `partition_lines` uses a strict `<`, so a line whose top sits exactly on
    /// the threshold belongs to the **continuation**. That boundary is what
    /// makes the retained height equal `cont_top` rather than overshooting by a
    /// line; getting it backwards would put `cont_top`'s own line on both the
    /// first half and the continuation's height budget.
    #[test]
    fn a_line_exactly_at_the_threshold_goes_to_the_continuation() {
        let line_at = |top: f32, para: usize| CellLine {
            top_y: Pt::new(top),
            para,
            interior_atomic: false,
            widow_control: false,
            keep_next: false,
        };
        let lines = vec![line_at(0.0, 0), line_at(14.0, 0), line_at(28.0, 0)];
        let (first, second) = partition_lines(&lines, Pt::new(14.0), Pt::new(14.0));

        assert_eq!(first.len(), 1, "only the line strictly above the cut stays");
        assert_eq!(second.len(), 2);
        assert_eq!(
            second[0].top_y,
            Pt::ZERO,
            "the threshold line becomes the continuation's first, rebased to 0"
        );
        assert_eq!(second[1].top_y, Pt::new(14.0));
    }

    /// §17.4.40: both halves preserve the cell's vertical margins, which is the
    /// stated reason the cut keeps `margin_top + margin_bottom` in its height
    /// budget — text must not collide with the cut-edge border.
    #[test]
    fn both_halves_account_for_the_cell_margins() {
        const MARGIN: f32 = 5.0;
        let rows = vec![row_n_lines(6, MARGIN)];
        let measured = measure(&rows);
        // Room for 2 lines of content plus both margins.
        let available = Pt::new(2.0 * 14.0 + 2.0 * MARGIN);
        let cut = find_row_cut(&RowCutInput {
            mr: &measured.rows[0],
            row: &rows[0],
            available,
        })
        .expect("splittable with margins");
        let parts = split_row_at(&measured.rows[0], &cut);

        assert!(
            parts.first.height <= available,
            "first half ({:.1}) must fit the space it was given ({:.1})",
            parts.first.height.raw(),
            available.raw()
        );
        // The retained content is 2 lines; the half is that plus both margins.
        assert!(
            (parts.first.height.raw() - (2.0 * 14.0 + 2.0 * MARGIN)).abs() < 0.01,
            "first-half height is retained content + both margins, got {:.1}",
            parts.first.height.raw()
        );
        assert!(
            parts.second.height > Pt::ZERO,
            "the continuation keeps real height"
        );
    }

    /// A continuation inherits a visible top edge: `top: b.top.or(b.bottom)`.
    /// Without the fallback a row whose top border was resolved away (the
    /// common mid-table case, where the edge is drawn as the row above's
    /// bottom) would continue onto the next page with no top line at all.
    #[test]
    fn continuation_inherits_a_top_border_from_the_original_bottom() {
        let bottom = super::super::types::TableBorderLine {
            width: Pt::new(2.0),
            color: crate::render::resolve::color::RgbColor::BLACK,
            style: super::super::types::TableBorderStyle::Single,
        };
        let mut mr = measure(&[row_n_lines(6, 0.0)]).rows.pop().expect("one row");
        mr.borders[0] = CellBorders {
            // Absent, not Suppressed: resolved away by conflict resolution, not
            // by an author `nil` — the fallback applies to both, but this test
            // is about the conflict-resolution case.
            top: CellEdge::Absent,
            bottom: CellEdge::Line(bottom),
            left: CellEdge::Absent,
            right: CellEdge::Absent,
        };
        let rows = [row_n_lines(6, 0.0)];
        let cut = find_row_cut(&RowCutInput {
            mr: &mr,
            row: &rows[0],
            available: Pt::new(4.0 * 14.0),
        })
        .expect("splittable");
        let parts = split_row_at(&mr, &cut);

        assert!(
            parts.first.borders[0].top.line().is_none(),
            "the first half keeps the original borders verbatim"
        );
        assert_eq!(
            parts.second.borders[0].top.line().map(|b| b.width),
            Some(Pt::new(2.0)),
            "the continuation falls back to the original bottom for its top edge"
        );
    }

    /// An explicit top border is *not* replaced by the bottom — `or` must not
    /// become an unconditional overwrite.
    #[test]
    fn continuation_keeps_an_explicit_top_border() {
        let thin = super::super::types::TableBorderLine {
            width: Pt::new(1.0),
            color: crate::render::resolve::color::RgbColor::BLACK,
            style: super::super::types::TableBorderStyle::Single,
        };
        let thick = super::super::types::TableBorderLine {
            width: Pt::new(4.0),
            ..thin
        };
        let mut mr = measure(&[row_n_lines(6, 0.0)]).rows.pop().expect("one row");
        mr.borders[0] = CellBorders {
            top: CellEdge::Line(thin),
            bottom: CellEdge::Line(thick),
            left: CellEdge::Absent,
            right: CellEdge::Absent,
        };
        let rows = [row_n_lines(6, 0.0)];
        let cut = find_row_cut(&RowCutInput {
            mr: &mr,
            row: &rows[0],
            available: Pt::new(4.0 * 14.0),
        })
        .expect("splittable");
        let parts = split_row_at(&mr, &cut);

        assert_eq!(
            parts.second.borders[0].top.line().map(|b| b.width),
            Some(Pt::new(1.0)),
            "an existing top border wins over the bottom fallback"
        );
    }

    /// The command partition boundary, exercised directly.
    ///
    /// It cannot be reached through real layout: `command_primary_y` uses a
    /// text command's *baseline* (box top + ascent) while `content_cut_y` is a
    /// box top, so no text command ever lands exactly on the threshold —
    /// flipping `<` to `<=` breaks no end-to-end test. Rect-shaped commands
    /// (cell shading, images, paragraph borders) *are* keyed on their top edge,
    /// so the boundary is reachable in principle; this pins it with synthetic
    /// commands rather than relying on metrics that happen to avoid it.
    ///
    /// A command exactly at the threshold belongs to the continuation, matching
    /// `partition_lines` — the two must agree or the halves disagree about what
    /// they contain.
    #[test]
    fn a_command_exactly_at_the_threshold_goes_to_the_continuation() {
        use crate::render::geometry::PtRect;
        let rect_at = |y: f32| DrawCommand::Rect {
            rect: PtRect::from_xywh(Pt::ZERO, Pt::new(y), Pt::new(10.0), Pt::new(2.0)),
            color: crate::render::resolve::color::RgbColor::BLACK,
        };
        let cmds = vec![rect_at(0.0), rect_at(14.0), rect_at(28.0)];

        let (first, second) = partition_commands(&cmds, Pt::new(14.0), Pt::new(14.0));

        let ys = |v: &[DrawCommand]| -> Vec<f32> {
            v.iter()
                .map(|c| match c {
                    DrawCommand::Rect { rect, .. } => rect.origin.y.raw(),
                    _ => unreachable!(),
                })
                .collect()
        };
        assert_eq!(
            ys(&first),
            vec![0.0],
            "only the command strictly above the cut stays"
        );
        assert_eq!(
            ys(&second),
            vec![0.0, 14.0],
            "the threshold command continues, rebased by the shift"
        );
    }
}
