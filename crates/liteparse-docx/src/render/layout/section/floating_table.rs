//! §17.4.58 — page placement for a floating table that may span pages.
//!
//! When a `<w:tbl>` with `<w:tblpPr>` (a floating-table anchor) is taller
//! than the available height on its anchor page, Word breaks it at row
//! boundaries and continues the table on subsequent pages. The anchor's
//! `tblpY` only positions the **first** slice; continuation slices start
//! at the top of their pages' content area.
//!
//! This module owns the pure placement decision. Row splitting is handled
//! upstream by [`crate::render::layout::table::layout_table_paginated`];
//! we take its `Vec<TableSlice>` and assign each slice to a page slot.
//!
//! OOXML §17.4.58 specifies `tblpY` semantics for the anchor itself but
//! does not formally describe overflow behavior. The continuation-at-top
//! rule mirrors Microsoft Word's observable behavior and is the
//! convention every consumer that handles overflow follows.
//!
//! Text wrapping (`<w:tblpPr>` floats register an [`ActiveFloat`]) is
//! anchored to the **first** page only — text on continuation pages does
//! not wrap, since the only content on those pages is the table itself.
//! Encoded by the `Anchor`/`Continuation` enum split below.

use crate::model::TableOverlap;
use crate::render::dimension::Pt;
use crate::render::layout::float::ActiveFloat;
use crate::render::layout::table::TableSlice;

/// §17.4.57 — outcome of resolving a floating table's requested
/// anchor against prior floats on the same page.
#[derive(Debug, PartialEq)]
pub(super) enum FloatingTableAnchor {
    /// The requested y is free of prior-float collision (or overlap
    /// is permitted by `TableOverlap::Overlap`). Place the table at
    /// this y on the current page.
    OnCurrentPage(Pt),
    /// `tblOverlap=Never` collision: the table was pushed below the
    /// last conflicting float. `from`/`to` are kept for diagnostics.
    Shifted { from: Pt, to: Pt },
    /// A *collision-induced* shift pushed the table past `page_bottom`.
    /// The caller must push a new page and re-resolve.
    ///
    /// Only ever returned when prior floats actually moved the anchor.
    /// A table that overflows purely because it is taller than the body
    /// is **not** a spillover — see [`resolve_floating_anchor`].
    Spillover,
}

/// §17.4.57 — resolve a floating table's requested anchor y against
/// prior `ActiveFloat`s registered for the current page.
///
/// - `overlap == Some(Never)`: iteratively push the anchor below any
///   *floating table* whose y-range intersects `[anchor, anchor +
///   height]`. If a shift happened *and* the shifted anchor would extend
///   the table past `page_bottom`, return `Spillover`.
/// - `overlap == Some(Overlap)` or `None` (the §17.4.57 default): the
///   anchor is returned unchanged on the current page; overlap with
///   prior floats is permitted.
///
/// `prior` is the page's whole float list, so it also holds image and
/// shape floats. Those are skipped: §17.4.57 is defined between floating
/// tables only, and a table is free to overlap a floating image
/// ([`FloatSource::participates_in_table_overlap`]).
///
/// # Overflow is not spillover
///
/// `Spillover` is conditioned on `shifted_any`, and that condition is
/// load-bearing rather than cosmetic. The caller answers `Spillover` by
/// pushing a fresh page and calling back in; `push_new_page` clears the
/// page's float list, so the retry sees `prior == []` and cannot shift.
///
/// Without the `shifted_any` guard, a table taller than the body area
/// re-reports `Spillover` forever: `requested` collapses to the page
/// top, and `page_top + height > page_bottom` holds on every page, so
/// each iteration allocates one more `LayoutedPage` and retries. That
/// is an unbounded loop — it was reachable from an ordinary document
/// (a long table plus `<w:tblOverlap w:val="never"/>`) and ran until
/// the OS killed the process.
///
/// A table that simply doesn't fit needs *pagination*, not relocation,
/// so it is returned as `OnCurrentPage` and sliced at row boundaries by
/// `layout_table_paginated_with_page_heights` — the same path the
/// permitted-overlap case already takes. Pairing the guard with the
/// cleared float list bounds the caller at one page push per table.
pub(super) fn resolve_floating_anchor(
    requested: Pt,
    height: Pt,
    overlap: Option<TableOverlap>,
    prior: &[ActiveFloat],
    page_bottom: Pt,
) -> FloatingTableAnchor {
    if !matches!(overlap, Some(TableOverlap::Never)) {
        return FloatingTableAnchor::OnCurrentPage(requested);
    }

    let mut anchor = requested;
    let mut shifted_any = false;
    loop {
        // Find the deepest `y_end` among floats whose range intersects
        // [anchor, anchor + height]. One pass is enough only if floats
        // are non-overlapping themselves; iterate to a fixed point.
        let mut max_blocking_end: Option<Pt> = None;
        for f in prior {
            // §17.4.57 governs table-vs-table overlap only. `page_floats`
            // also carries image and shape floats, which a floating table
            // is free to overlap.
            if !f.source.participates_in_table_overlap() {
                continue;
            }
            let overlaps = anchor < f.page_y_end && anchor + height > f.page_y_start;
            if overlaps {
                let candidate = f.page_y_end;
                max_blocking_end = Some(match max_blocking_end {
                    Some(prev) if prev >= candidate => prev,
                    _ => candidate,
                });
            }
        }
        match max_blocking_end {
            None => break,
            Some(new_anchor) => {
                anchor = new_anchor;
                shifted_any = true;
            }
        }
    }

    if !shifted_any {
        // Nothing moved the anchor, so a fresh page would resolve to the
        // same place. Overflow here is a pagination problem, not a
        // placement one.
        return FloatingTableAnchor::OnCurrentPage(anchor);
    }

    if anchor + height > page_bottom {
        FloatingTableAnchor::Spillover
    } else {
        FloatingTableAnchor::Shifted {
            from: requested,
            to: anchor,
        }
    }
}

/// One page-slot in a floating table's multi-page placement plan. The
/// enum distinguishes the two semantic roles a slice can play, so
/// downstream code (text-wrap registration, debug logging) doesn't have
/// to inspect the slice index.
#[derive(Debug)]
pub(super) enum FloatingTablePagePlacement {
    /// First slice — drawn at the resolved `tblpY` anchor on the
    /// anchoring page. Registers as an [`ActiveFloat`] so subsequent
    /// body text wraps around it.
    Anchor { y_start: Pt, slice: TableSlice },
    /// Subsequent slice — drawn at the top of the next page's content
    /// area. Does not register a float (no body content interleaves on
    /// the continuation page).
    Continuation { y_start: Pt, slice: TableSlice },
}

impl FloatingTablePagePlacement {
    /// Page-local y where this slice draws. Test helper; the layout
    /// path destructures the variant directly instead of going through
    /// this accessor.
    #[cfg(test)]
    pub(super) fn y_start(&self) -> Pt {
        match self {
            Self::Anchor { y_start, .. } | Self::Continuation { y_start, .. } => *y_start,
        }
    }

    /// Borrow the slice. Test helper for the planner's pass-through
    /// invariants; the layout path destructures the variant.
    #[cfg(test)]
    pub(super) fn slice(&self) -> &TableSlice {
        match self {
            Self::Anchor { slice, .. } | Self::Continuation { slice, .. } => slice,
        }
    }
}

/// Multi-page placement plan for a floating table. Ordered: index 0 is
/// the anchor page, indices 1..n are continuation pages.
///
/// Invariant: if `pages` is non-empty, `pages[0]` is always `Anchor`
/// and every subsequent entry is `Continuation`. Constructed only via
/// [`plan_floating_table_pages_with_page_tops`], which preserves this.
#[derive(Debug)]
pub(super) struct FloatingTablePlan {
    pub(super) pages: Vec<FloatingTablePagePlacement>,
}

/// Map a sequence of paginated table slices to page placements.
///
/// - The first slice is wrapped in [`FloatingTablePagePlacement::Anchor`]
///   at `anchor_y` (the resolved `tblpY`).
/// - Every subsequent slice is wrapped in
///   [`FloatingTablePagePlacement::Continuation`] at `continuation_y`
///   (the top of the content area on continuation pages).
/// - An empty input yields an empty plan (no pages).
///
/// This function is pure — no I/O, no global state, no measurement.
/// Row-splitting and slice production are handled by
/// `layout_table_paginated`; this function only assigns slices to
/// page slots.
pub(super) fn plan_floating_table_pages_with_page_tops(
    slices: Vec<TableSlice>,
    anchor_y: Pt,
    mut page_top_for_index: impl FnMut(usize) -> Pt,
) -> FloatingTablePlan {
    let pages = slices
        .into_iter()
        .enumerate()
        .map(|(idx, slice)| {
            if idx == 0 {
                FloatingTablePagePlacement::Anchor {
                    y_start: anchor_y,
                    slice,
                }
            } else {
                FloatingTablePagePlacement::Continuation {
                    y_start: page_top_for_index(idx),
                    slice,
                }
            }
        })
        .collect();
    FloatingTablePlan { pages }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::geometry::PtSize;
    use crate::render::layout::float::{FloatSource, WrapTextSide};

    fn slice(height: f32) -> TableSlice {
        TableSlice {
            commands: Vec::new(),
            size: PtSize::new(Pt::new(100.0), Pt::new(height)),
        }
    }

    fn float_at_source(y_start: f32, y_end: f32, source: FloatSource) -> ActiveFloat {
        ActiveFloat {
            page_x: Pt::ZERO,
            page_y_start: Pt::new(y_start),
            page_y_end: Pt::new(y_end),
            width: Pt::new(100.0),
            source,
            wrap_text: WrapTextSide::BothSides,
        }
    }

    /// A prior *table* float — the only kind §17.4.57 governs.
    fn float_at(y_start: f32, y_end: f32) -> ActiveFloat {
        float_at_source(y_start, y_end, FloatSource::Table { owner_block_idx: 0 })
    }

    /// Single-slice case (table fits on its anchor page). Placement is
    /// a single `Anchor` at `anchor_y`.
    #[test]
    fn plan_single_slice_is_one_anchor_placement() {
        let plan =
            plan_floating_table_pages_with_page_tops(vec![slice(50.0)], Pt::new(100.0), |_| {
                Pt::new(40.0)
            });
        assert_eq!(plan.pages.len(), 1);
        assert!(matches!(
            plan.pages[0],
            FloatingTablePagePlacement::Anchor { .. }
        ));
        assert_eq!(plan.pages[0].y_start().raw(), 100.0);
    }

    /// Two-slice case (table overflows once). First at anchor, second
    /// at continuation y.
    #[test]
    fn plan_two_slices_anchor_then_continuation() {
        let plan = plan_floating_table_pages_with_page_tops(
            vec![slice(700.0), slice(100.0)],
            Pt::new(100.0),
            |_| Pt::new(40.0),
        );
        assert_eq!(plan.pages.len(), 2);
        assert!(matches!(
            plan.pages[0],
            FloatingTablePagePlacement::Anchor { .. }
        ));
        assert!(matches!(
            plan.pages[1],
            FloatingTablePagePlacement::Continuation { .. }
        ));
        assert_eq!(plan.pages[0].y_start().raw(), 100.0);
        assert_eq!(plan.pages[1].y_start().raw(), 40.0);
    }

    /// N-slice case (n ≥ 3). Only the first slice is the Anchor;
    /// every subsequent slice is a Continuation at `continuation_y`.
    /// This pins the "no second anchor page" rule.
    #[test]
    fn plan_n_slices_only_first_is_anchor_page() {
        let plan = plan_floating_table_pages_with_page_tops(
            vec![slice(700.0), slice(700.0), slice(100.0)],
            Pt::new(150.0),
            |_| Pt::new(40.0),
        );
        assert_eq!(plan.pages.len(), 3);
        let anchor_count = plan
            .pages
            .iter()
            .filter(|p| matches!(p, FloatingTablePagePlacement::Anchor { .. }))
            .count();
        assert_eq!(anchor_count, 1, "at most one Anchor in a plan");
        assert_eq!(plan.pages[0].y_start().raw(), 150.0);
        for p in &plan.pages[1..] {
            assert_eq!(p.y_start().raw(), 40.0);
            assert!(matches!(p, FloatingTablePagePlacement::Continuation { .. }));
        }
    }

    #[test]
    fn continuation_slices_use_each_page_top_boundary() {
        let plan = plan_floating_table_pages_with_page_tops(
            vec![slice(50.0), slice(50.0), slice(50.0)],
            Pt::new(100.0),
            |page_index| match page_index {
                1 => Pt::new(40.0),
                _ => Pt::new(60.0),
            },
        );

        assert_eq!(plan.pages[0].y_start(), Pt::new(100.0));
        assert_eq!(plan.pages[1].y_start(), Pt::new(40.0));
        assert_eq!(plan.pages[2].y_start(), Pt::new(60.0));
    }

    /// Empty slice list → empty plan. Edge case: never happens via
    /// `layout_table_paginated` (which returns at least one slice for
    /// non-empty input), but guarded for safety.
    #[test]
    fn plan_empty_slices_returns_empty_plan() {
        let plan =
            plan_floating_table_pages_with_page_tops(Vec::new(), Pt::new(100.0), |_| Pt::new(40.0));
        assert!(plan.pages.is_empty());
    }

    /// Slice contents (commands, size) are passed through intact —
    /// the planner only assigns placement, never modifies the slice.
    #[test]
    fn plan_preserves_slice_size_through_placement() {
        let plan = plan_floating_table_pages_with_page_tops(
            vec![slice(123.4), slice(56.7)],
            Pt::new(100.0),
            |_| Pt::new(40.0),
        );
        assert_eq!(plan.pages[0].slice().size.height.raw(), 123.4);
        assert_eq!(plan.pages[1].slice().size.height.raw(), 56.7);
    }

    // ── §17.4.57 — anchor resolution against prior floats ──────────────

    /// Default behavior (`overlap == None`, the §17.4.57 default
    /// `Overlap`): the anchor is returned unchanged even when prior
    /// floats would intersect.
    #[test]
    fn anchor_accepts_overlap_when_overlap_permitted() {
        let prior = vec![float_at(80.0, 130.0)];
        let resolved =
            resolve_floating_anchor(Pt::new(100.0), Pt::new(50.0), None, &prior, Pt::new(700.0));
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(100.0)));

        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0),
            Some(TableOverlap::Overlap),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(100.0)));
    }

    /// No prior floats → no shifting, no spillover, returns
    /// `OnCurrentPage` even with `Never`.
    #[test]
    fn anchor_no_priors_returns_requested_y() {
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &[],
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(100.0)));
    }

    /// `Never` + overlap with one float: shift the anchor to the
    /// float's `y_end`. Spec §17.4.57 — two tables anchored on the
    /// same page that both forbid overlap must be repositioned to
    /// avoid drawing over each other.
    #[test]
    fn anchor_shifts_below_overlapping_float() {
        let prior = vec![float_at(80.0, 130.0)];
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(
            resolved,
            FloatingTableAnchor::Shifted {
                from: Pt::new(100.0),
                to: Pt::new(130.0),
            }
        );
    }

    /// `Never` + anchor below all priors: no shift, returns
    /// `OnCurrentPage`.
    #[test]
    fn anchor_below_priors_is_accepted_unchanged() {
        let prior = vec![float_at(80.0, 130.0)];
        let resolved = resolve_floating_anchor(
            Pt::new(200.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(200.0)));
    }

    /// `Never` + multiple overlapping priors: anchor shifts past the
    /// deepest blocking float, iterating until no overlap remains.
    /// Tests the fixed-point iteration in the resolver.
    #[test]
    fn anchor_shifts_past_chain_of_floats() {
        let prior = vec![
            float_at(80.0, 130.0),
            float_at(125.0, 180.0), // overlaps both the prior float and the shifted anchor
        ];
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        // First iteration: anchor 100..150 collides with both floats.
        // Shift to max y_end = 180 (the second float). Re-check from
        // 180: no remaining collision.
        assert_eq!(
            resolved,
            FloatingTableAnchor::Shifted {
                from: Pt::new(100.0),
                to: Pt::new(180.0),
            }
        );
    }

    /// `Never` + shifted anchor + height extends past page bottom →
    /// `Spillover`. The caller pushes a new page and re-anchors.
    #[test]
    fn anchor_spillover_when_shift_exceeds_page_bottom() {
        let prior = vec![float_at(80.0, 680.0)]; // huge prior float
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0), // 680 + 50 = 730 > 700
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::Spillover);
    }

    /// A table taller than the page body, with nothing to collide
    /// with, is **not** a spillover.
    ///
    /// This is the termination guard. The caller answers `Spillover`
    /// by pushing a fresh page and re-resolving; a fresh page has an
    /// empty float list, so it would resolve to exactly this call and
    /// report `Spillover` again — one `LayoutedPage` allocated per
    /// iteration, forever, until the process is OOM-killed. Overflow
    /// with no shift means "paginate me", not "move me".
    #[test]
    fn anchor_taller_than_page_is_not_spillover() {
        let resolved = resolve_floating_anchor(
            Pt::new(40.0),
            Pt::new(2000.0), // far taller than the 700pt body
            Some(TableOverlap::Never),
            &[],
            Pt::new(700.0),
        );
        assert_eq!(
            resolved,
            FloatingTableAnchor::OnCurrentPage(Pt::new(40.0)),
            "overflow without a collision-induced shift must not spill"
        );
    }

    /// Same guard, but with prior floats present that simply don't
    /// block the anchor. `shifted_any` is still false, so the presence
    /// of floats must not turn overflow into a spillover.
    #[test]
    fn anchor_taller_than_page_with_non_blocking_priors_is_not_spillover() {
        // Float sits entirely above the requested anchor.
        let prior = vec![float_at(10.0, 30.0)];
        let resolved = resolve_floating_anchor(
            Pt::new(40.0),
            Pt::new(2000.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(40.0)));
    }

    /// The caller's retry is bounded: whatever spills on a populated
    /// page resolves on the next one, because `push_new_page` clears
    /// the float list. Re-resolving a spilled table against `&[]` must
    /// therefore always terminate the loop — even when the table is
    /// too tall for the fresh page too.
    #[test]
    fn spillover_retry_on_empty_page_always_terminates() {
        let prior = vec![float_at(80.0, 680.0)];
        let first = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(2000.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(first, FloatingTableAnchor::Spillover);

        // What the caller does next: fresh page, empty float list,
        // anchor collapsed to the page top.
        let retry = resolve_floating_anchor(
            Pt::new(40.0),
            Pt::new(2000.0),
            Some(TableOverlap::Never),
            &[],
            Pt::new(700.0),
        );
        assert!(
            !matches!(retry, FloatingTableAnchor::Spillover),
            "a second spillover would loop the caller forever"
        );
    }

    /// §17.4.57 governs table-vs-table overlap only. Image and shape
    /// floats sit in the same `page_floats` list, but a floating table
    /// is free to overlap them, so they must not move the anchor.
    #[test]
    fn image_and_shape_floats_do_not_shift_the_anchor() {
        for source in [FloatSource::Image, FloatSource::Shape] {
            // Squarely overlapping the requested 100..150 band.
            let prior = vec![float_at_source(80.0, 130.0, source)];
            let resolved = resolve_floating_anchor(
                Pt::new(100.0),
                Pt::new(50.0),
                Some(TableOverlap::Never),
                &prior,
                Pt::new(700.0),
            );
            assert_eq!(
                resolved,
                FloatingTableAnchor::OnCurrentPage(Pt::new(100.0)),
                "{source:?} is not a floating table; tblOverlap must ignore it"
            );
        }
    }

    /// The sharp case: a table float and a *deeper* image float both
    /// overlap. The anchor must land past the table only. Taking the
    /// deepest of all floats would overshoot to 400.
    #[test]
    fn mixed_floats_shift_past_the_table_and_ignore_the_image() {
        let prior = vec![
            float_at(80.0, 130.0),
            float_at_source(90.0, 400.0, FloatSource::Image),
        ];
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(
            resolved,
            FloatingTableAnchor::Shifted {
                from: Pt::new(100.0),
                to: Pt::new(130.0),
            },
            "shift past the table's y_end (130), not the image's (400)"
        );
    }

    /// An image float alone can no longer produce `Spillover`, since it
    /// cannot shift the anchor in the first place.
    #[test]
    fn image_float_cannot_cause_spillover() {
        let prior = vec![float_at_source(80.0, 680.0, FloatSource::Image)];
        let resolved = resolve_floating_anchor(
            Pt::new(100.0),
            Pt::new(50.0), // 680 + 50 = 730 > 700 if the image counted
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(100.0)));
    }

    /// Edge: anchor at the exact `y_end` of a float is **not** an
    /// overlap (touching edges are allowed). Returns `OnCurrentPage`
    /// unchanged.
    #[test]
    fn anchor_at_prior_y_end_is_not_overlap() {
        let prior = vec![float_at(80.0, 130.0)];
        let resolved = resolve_floating_anchor(
            Pt::new(130.0),
            Pt::new(50.0),
            Some(TableOverlap::Never),
            &prior,
            Pt::new(700.0),
        );
        assert_eq!(resolved, FloatingTableAnchor::OnCurrentPage(Pt::new(130.0)));
    }
}
