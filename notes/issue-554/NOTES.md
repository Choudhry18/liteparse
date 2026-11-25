# Issue 554: ruled table rows that span columns

Work notes for https://github.com/run-llama/liteparse/issues/554.
Branch: `fix/ruled-colspan-missing-vrule`, based on upstream `main` at `a684a4a` (v2.20.0).
The code on the branch is **v17** (section 6 gives the history). v17 is v10 plus regression
fixes. The v11 rule (6b) is reverted, because it fixed pages that `main` also parsed wrongly.

**Scope.** This branch fixes issue 554 and the regressions that the fix causes. Problems that
`main` also has are for later pull requests (section 8), for example boxes above a table that
the grid pulls in.
A persistent worktree for this branch is at `~/Desktop/Choudhry/liteparse-554`. Do not work
in `/tmp`: macOS deletes files there that are older than 3 days.

**Drop the `notes:` commits before you open a pull request.**

## 1. The problem

A ruled table can have a row where the interior vertical rules stop. That row is one cell
that spans the columns. The markdown output split each line of that row at the column
positions of the rows above. The text then came out in the wrong order.

The bug occurs only when all of these conditions are true:

1. The text is inside a ruled grid. The outer border connects to the table rules.
2. A vertical rule exists at the column position somewhere in the grid, but not in that row.
3. The text lines are long enough to cross those positions.
4. The grid passes the gates: the straddle census (45% or less), the running-prose stop, and
   the density gate. When a gate rejects the grid, the text comes out as paragraphs.

Typical layouts: a section row inside a table, a page frame with a ruled header (the issue),
a partial merge, and a ruled header plus a ruled footer around a page body.

The cause is in `crates/liteparse/src/markdown_layout/tables.rs`:

- `build_ruled_table_from` uses one set of column boundaries (`xs`) for all rows.
- `assign_cells` splits each span at every boundary that the span crosses.
- `rowspan_mask` finds vertical merges. No code found horizontal merges.
- `cluster_v_segments` unions the y-ranges of all rules at one x. This fills the gap that a
  spanning row leaves in a rule. Only the raw segments (`raw_vs`) show the gap.

## 2. The rules on the branch (v11)

All rules are in `tables.rs`, except for the output rules. Tables without covered cells give
the same output as `main`.

### Merge detection: `colspan_mask`

For each row and each interior boundary `xs[c]`:

1. **No rule in the row.** The raw vertical segments within 6pt of the boundary cover less
   than 20% of the row band (`COLSPAN_MIN_RULE_COVERAGE`). The band is the row minus 3pt at
   the top and at the bottom.
2. **The text crosses the boundary.** A span crosses `xs[c]` by 3pt on each side
   (`STRADDLE_MARGIN_PT`).
3. **The text flows across the boundary** (`text_flows_across`). A word box covers the
   boundary, or the gap at the boundary is less than half the font height. Without word boxes,
   the span box decides.
4. **No line contradicts the merge.** No line of the row has separate text on both sides of
   the boundary without flowing across it.
5. A boundary merges when 1, 2 and 3 are true and 4 is false.
6. **Growth.** Each merged group grows across unruled boundaries into the empty cells next to
   it. It stops at the first cell with text.
6a. **Numeric glue guard** (`unmerge_glued_numbers`, v10). A merged group is cancelled when it
   holds two neighbouring numbers on one line that are separate, and both numbers align with a
   number in another row of the grid (left or right edges within 2.5pt). Two numbers are one
   number only as a digit-group pair: the gap is at most half the font size, the left part
   ends in 1 to 3 digits, and the right part is exactly 3 digits ("10 000"). The words come
   from the word boxes, or from the span split at whitespace.
6b. (v11 only, reverted in v12.) No merges above the header band.
6c. **Lines outside the grid** (v17). A line that goes more than `GRID_X_SLACK_PT` (6pt) past
   the grid's left or right edge makes no merge. `assign_cells` skips such a line, so before
   v17 its row got an empty merged cell.
6d. **Ruled groups** (v16). `colspan_mask` also marks each merged group that has a drawn rule or
   the grid edge on its left and right, and that does not span all columns.

Performance: the function stops early when no text crosses a boundary. It puts each raw
segment in the bucket of its nearest boundary, in one pass. It checks only rows with a
crossing.

### Text placement: `assign_cells`

7. The text of a merged group goes into the first cell of the group, in reading order.
8. The `repl` grid still receives the text in all covered columns, and `row_alpha_spanner` is
   set as on `main`. Header folding needs both.
9. Covered cells get `covered = true`. The box of the first cell grows to cover the group.

### Table decision: `build_ruled_table_from`

10. The build without merges decides whether the grid becomes a table, the same as on `main`.
    When `main` accepts a grid, the merged build gives the output. The merged build skips the
    density gate. When it fails another gate, the unmerged output is used.

### Header passes

11. `HeaderLayout` gives `flatten_header_band` and `merge_stacked_header` a fill value for each
    row. A covered cell counts as filled when its group holds text. A row whose text is only in
    spanning cells is a group-label row. Its fill is 0, so it never ends the header band.
12. `merge_stacked_header` does not apply its height test to group-label rows.
12a. (v13) `merge_stacked_header` does not fold when its anchor row (the first dense row) has a
    merged cell. A merged note row is not a data row. Before, a note row at the bottom of a
    table made all the data rows above it into one header row (`Coca Cola 10-k_page48`).
12b. (v14) In `flatten_header_band`, the bottom row of the band gives the split cells, not the
    `repl` cells, where it has no merged cell. One text run can hold the labels of several
    columns ("Starter Growth Scale"), and `repl` copied the full run into each column
    (`dense_table_074`).
12c. (v16) In `flatten_header_band`, a cell of a ruled group (rule 6d) gives its group's label
    to each column of the group. The label text can cover fewer columns than its rules
    ("Six Liability PMF's" over 4 columns, `SERFF_TX page847`).
12d. (v17) A row is a group-label row only when its height is at most 3 times the median row
    height (`GROUP_LABEL_MAX_HEIGHT`). A taller row holds a paragraph. Before, cover-page text
    in a grid was folded into each column header (`Kimco ... page11`).
12e. (v17) `lines_left_out` uses the source text items of the cells when they have them. A
    line stays with the table only when a cell holds one of its items. Before, a folded header
    that repeats "Premium" made a line below with the same word look taken, and the line was
    lost (`4ef83fb72676cd65_p5`).

### Output: `blocks.rs` and `layout.rs`

13. `Cell` has the field `covered`. JSON omits it when it is false.
14. Markdown renders a table with a covered cell as an HTML `<table>` with `colspan`.
15. The layout JSON keeps `kind: "table"` and one cell per column. The first cell of a group
    gets `colspan`. A covered cell is an empty cell without a `bbox`.

### Repeated-header split: `split_repeated_header_tables` in `blocks.rs` (v10)

16. A group-label row (`is_group_label_row`: it has covered cells, and each filled cell after
    the first column spans columns) moves with a repeated header row below it, the same as a
    title row. Before, the cut came between the group labels and their column header.
17. A part that starts with a group-label row and its column header gets one folded header
    row (`fold_group_header`): "Current Base Rates F.R", the same form as
    `flatten_header_band`.

### Tests: `tests/ruled_colspan.rs`

There are 12 tests with synthetic PDFs. The v11 test was removed with rule 6b. Two tests are
from v13 and v14, and each fails without its fix: a merged note row under sparse data rows
(rule 12a), and a header text run over narrow columns (rule 12b). The others cover: section rows in four drawing styles, the
issue frame, a partial merge, dividers in the header only, a text run across a drawn rule, a
joined run with a column gap, a spanning label next to separate values, a spanning label next
to a footnote marker, a repeated group header (rules 16 and 17), and right-aligned numbers
that header-only rules cut through (rule 6a).

## 3. Alternative merge rules that were tested

`make_pdfs.py` makes 14 synthetic cases. A temporary switch ran each rule on the same build.

| Case | Straddle only | Rules only | Junction point | Both (chosen) |
|---|---|---|---|---|
| 01-02 issue frame | yes | yes | no, misses | yes |
| 05-07, 09 section row, 4 drawing styles | yes | yes | no, misses | yes |
| 08 partial merge, columns 0-1 | yes | yes | no | yes |
| 10 dividers in header only, body in columns | yes | **no, merges each body row** | no | yes |
| 11 one text run across a drawn rule | **no, merges** | yes | yes | yes |
| 12-14 plain, rowspan, dashed | yes | yes | yes | yes |

The junction-point test fails because a rule that ends on the row's top edge touches the
junction. When rules are drawn one row at a time, both junctions exist, but the edge does not.

## 4. Performance

| Stress page (`make_stress.py`) | main | first version | v4 and later |
|---|---|---|---|
| Ruled table + 40k other strokes | 96-101ms | 99ms | 95-101ms |
| 30x40 lattice + 40k strokes | 97-104ms | 132ms | 97-105ms |
| Same lattice, text crosses each row | 97-103ms | 132ms | 103-106ms |

## 5. Benchmark results

### ParseBench v1 (in-sample: the rules were tuned on these documents)

2,049 documents, no OCR. `lit` output is deterministic.

| Category | main | v1 | v2 | v3 | v4 | v5 | v6 | **v7** |
|---|---|---|---|---|---|---|---|---|
| Tables | 0.4581 | 0.4603 | 0.4667 | 0.4658 | 0.4662 | 0.4661 | 0.4671 | **0.4671** |
| Charts | 0.0123 | 0.0194 | 0.0118 | 0.0123 | 0.0118 | 0.0152 | 0.0152 | 0.0147 |
| Text content | 0.7060 | 0.7065 | 0.7063 | 0.7061 | 0.7063 | 0.7064 | 0.7062 | 0.7062 |
| Text formatting | 0.4440 | 0.4430 | 0.4467 | 0.4426 | 0.4467 | 0.4441 | 0.4441 | 0.4441 |
| Layout | 0.3203 | 0.3112 | 0.3327 | 0.3189 | 0.3327 | 0.3205 | 0.3205 | 0.3205 |
| **Overall** | 0.3881 | 0.3881 | 0.3929 | 0.3891 | 0.3928 | 0.3905 | 0.3906 | **0.3905** |
| Documents better / worse | | | 104 / 33 | 47 / 23 | 95 / 26 | 68 / 13 | 66 / 10 | **65 / 10** |
| Largest loss | | | -0.76 | | -0.76 | -0.32 | -0.14 | **-0.14** |

The layout gain of v2 and v4 comes from rule changes that reject decorative grids. Those
rules also made 6 tables disappear. v5 and later do not change the table decision, so the
layout gain is not on the branch. That gain is possible as a separate change.

### ParseBench-2 (held out: not used during development)

An independent reviewer ran this evaluation. The `tables` subset is v1 documents. Also,
10 `complex_table_headers` pages and 4 `dense_tables` pages are files from v1. Those pages are
excluded here (`results/overlap.json`). The `insurance_filing_tables` subset has other filings
than v1, but the same type of document.

| Held-out subset | Pages | main | v6 | **v7** | v7 better / worse |
|---|---|---|---|---|---|
| complex_table_headers | 139 | 0.0568 | 0.0574 | 0.0571 | 1 / 2 |
| financial_tables | 319 | 0.2702 | 0.2816 | 0.2831 | 18 / 3 |
| insurance_filing_tables | 412 | 0.3540 | 0.3656 | 0.3670 | 46 / 15 |
| dense_tables | 369 | 0.4700 | 0.4678 | 0.4696 | 31 / 13 |
| forms | 500 | 0.6601 | 0.6686 | 0.6688 | 49 / 6 |
| fillable_forms | 100 | 0.8623 | 0.8674 | 0.8665 | 10 / 1 |
| **Tables (4 subsets, equal weight)** | | 0.2877 | 0.2931 | **0.2942** | |

- Per-page change over the 1,239 held-out table pages for v7: **+0.0076**. The 95% bootstrap
  interval is [+0.0024, +0.0129]. 96 pages are better and 33 are worse.
- **Glued number cells** (a colspan cell with 2 or more numbers): main 0, v6 62 cells in 14
  documents, **v7 31 cells in 12 documents**.
- **Visual check of v6 by the reviewer:** 30 rendered held-out pages gave 22 better, 3 worse,
  3 mixed and 2 unclear. 7 of the 11 sampled pages that lost more than 0.2 look better on the
  page. The scorer misread them (`results/judgments.tsv`).
- **Constant sensitivity** (`results/sensitivity_scores.txt`): the 20% coverage, the gap of
  half the font height and the 3pt margin are not fragile. The held-out Tables score moves by
  at most 0.0006 when one constant changes.

### ParseBench-2 again, for v8 to v10 (2026-11-24)

These runs use a newer ParseBench clone (`63e926f`), so the numbers differ a little from the
table above. `main`, v7 and v10 were all scored with this clone. The v1 data was not available,
so the in-sample check uses the ParseBench-2 `tables` subset (v1 documents).

**Caution:** the v8 to v10 decisions used these held-out results. Thus ParseBench-2 is no longer
a clean held-out set, and the v10 numbers are a little optimistic.

| | main | v7 | **v10** |
|---|---|---|---|
| Held-out Tables (4 subsets, equal weight) | 0.2879 | 0.2944 | **0.2968** |
| complex_table_headers | 0.0568 | 0.0571 | 0.0589 |
| financial_tables | 0.2702 | 0.2831 | 0.2831 |
| insurance_filing_tables | 0.3546 | 0.3676 | 0.3755 |
| dense_tables | 0.4700 | 0.4696 | 0.4696 |
| In-sample `tables` subset | 0.4310 | 0.4453 | 0.4453 |
| Per-page change vs main (1,239 held-out pages) | | +0.0076 | **+0.0104**, 95% CI [+0.0051, +0.0160] |
| Pages better / worse | | 96 / 33 | **102 / 26** |
| Glued number cells (4 table subsets) | 0 | 22 on 6 pages | **3 on 2 pages** |
| Pages with another table count than main | | 12 | 13 |

- On each page where v10 scores lower than `main`, v10 has the same score as v7. Thus v10 adds
  no new loss.
- The 3 glued cells that remain do not come from a merge. `dense_table_393` has date headers
  ("30 June 2025"), which the scan reads as numbers. `ab7d5a005dc3c5f3_p11` has a grid with only
  2 columns, and `main` also puts the 3 values in one cell.

**Ablation (`results/ablation.txt`).** v9 had three new changes. Each run removes one of them:

| Run | Held-out per page vs main | 95% CI | Better / worse | In-sample |
|---|---|---|---|---|
| v7 | +0.0076 | [+0.0028, +0.0126] | 96 / 33 | 0.4453 |
| v9 (all three) | +0.0044 | [-0.0004, +0.0101] | 95 / 51 | 0.4475 |
| v9 without the form-band cut (= v10) | **+0.0104** | [+0.0048, +0.0158] | **102 / 26** | 0.4453 |
| v9 without the group-label split | +0.0039 | [-0.0015, +0.0093] | 95 / 51 | 0.4475 |
| v9 without the glue guard | +0.0044 | [-0.0003, +0.0097] | 95 / 52 | 0.4475 |
| v9, glue guard without alignment | +0.0040 | [-0.0009, +0.0093] | 93 / 51 | |

### v11: no merges above the header band (2026-11-24, `results/compare_v11.txt`)

v11 is v10 plus rule 6b.

| | main | v7 | v10 | **v11** |
|---|---|---|---|---|
| Held-out Tables (4 subsets, equal weight) | 0.2879 | 0.2944 | **0.2968** | 0.2943 |
| Per-page change vs main | | +0.0076 | **+0.0104** | +0.0075 |
| 95% CI | | [+0.0025, +0.0127] | [+0.0051, +0.0160] | [+0.0044, +0.0109] |
| Pages better / worse | | 96 / 33 | 102 / 26 | 91 / **16** |
| Largest loss | | -0.93 | -0.93 | **-0.43** |
| In-sample `tables` subset | 0.4310 | 0.4453 | 0.4453 | 0.4446 |
| In-sample pages worse than main | | 9 | 9 | 4 |

**v11 is a trade-off, not an improvement.** It gives up a large part of the v10 gains to fix
pages that `main` also parsed wrongly:

- **It removes or reduces 11 v10 losses (+4.38 in total).** Examples: `dense_table_289` and
  `dense_table_290` (0 to 0.93), `010b43388a0c67b0_p8` (0 to 0.79), `1fea2f2540c835bf_p1`
  (0.14 to 0.76). On these pages, `main` also did not read the form text above the table: it
  cut the text into pieces. v11 gives the same pieces again. The score goes up because the
  scorer skips the pieces and finds the real header row.
- **It gives up or reduces 38 v10 gains (-8.03 in total).** Examples: four insurance pages
  (0.73 to 0.02, `main` 0.00), `qei-2026-08-12_page1` (0.49 to 0.06), several dense tables
  (0.95 to 1.00 back to `main` at 0.75 to 0.77).
- **The cause of most lost gains is the weakness of rule 6b.** For example, the header of
  `02f7022a1f08f606_p7` ("2nd Prior Calendar Year | 1st Prior ... | Most Recent ...") fills only
  3 of 7 columns. The rule finds the header too far down. Thus it drops the correct merges of
  the body rows ("Countrywide direct earned premium for lines of business subject to
  Proposition 103 ($):" over 4 columns), and these rows are cut into pieces again.

### v12 to v17: regression fixes on v10 (2026-11-25, `results/compare_v17.txt`)

For each page that scores lower than `main`, a rendered image of the page was compared with
both outputs. A subagent did 18 of these pages, and the main session did the others. The fixes
are rules 6c, 6d and 12a to 12e.

| | main | v10 | **v17** |
|---|---|---|---|
| Held-out Tables (4 subsets, equal weight) | 0.2879 | 0.2968 | **0.2988** |
| Per-page change vs main, held out | | +0.0104 | **+0.0129** |
| 95% CI | | [+0.0050, +0.0157] | [+0.0075, +0.0186] |
| Pages better / worse, held out | | 102 / 26 | 108 / 24 |
| In-sample `tables` subset | 0.4310 | 0.4453 | **0.4495** |
| In-sample pages better / worse | | 41 / 9 | 47 / 5 |

Versions that did not stay:
- **v12** took the header fill from the cell placement without merges. It fixed Coca Cola,
  but it lost the v10 header folds on 4 pages (`150bc31d9c681a52_p10`: 0.59 to 0). v13
  replaced it with rule 12a.
- **v15** gave a merged label to all columns of its merged group. It fixed `SERFF_TX page847`,
  but a group that grew into empty cells is wider than its label (`dense_table_233`: 0.77 to
  0.53). v16 replaced it with rule 12c, which uses only groups with drawn rules on both sides.

**Image check of the losses that remain** (v13 output, the same on v17 unless noted):
- **The branch reads the page better, and the scorer gives a lower score (13 pages).** Examples:
  - `dense_table_289` and `290`: the scorer cannot split the HTML table of 3 side-by-side tax
    tables. It splits the `main` pipe table and reports 178 rows as missing on the branch.
  - `dense_table_041`: the same side-by-side problem. Both outputs have the 3 tables merged.
  - `dense_table_304` and `344`: the county cells are whole.
  - `currency-2026-08-13_page1`: the US $ and Yen rows are in reading order. `main`
    interleaves the words of two lines.
  - `64a4c79de95f6871_p8`: the group headers "Coverage: Bodily Injury" and "Coverage: Property
    Damage" each span 3 columns. This agrees with the image.
  - `SERFF_TX page706`, `test_page1`: the title becomes one `<th>` row. The data is identical.
  - `dense_table_313` (v16): "Component A" to "Component M" agrees with the image.
- **Neutral:** `51abb0b9601cd8df_p9` (5 tables are now 1 HTML table, and the section rows are
  whole), `sampo` (the row labels are whole, but the title takes the Q3/2024 header cell),
  `1 timetable (1)_page27` (one title bar "Sundays ... to Manhattan" over all columns; neither
  version reads it as a title), `b4dd261dd192264f_p10` (title box above the table).
- **Real regression that stays:** `dense_table_443` (0.001 to 0). The page has tracked changes
  and comment marks on the table. The header fold moves 4 rows out of the body. Then
  `merge_continuation_rows` passes its "half of the rows have a first cell" test, and it joins
  separate rows. Also, "2 2/3*" share a cell across a rule that a comment connector hides.
- **Boxes above the table** (`010b43388a0c67b0_p8` and others): this is a `main` bug. See
  section 8, item 1.

### The other ParseBench-2 subsets (2026-11-25, `results/compare_other.txt`)

The markdown changes on 206 pages outside the table subsets.

| Subset | Metric | Change vs main, per page | Better / worse |
|---|---|---|---|
| forms (500) | `forms_v2` | +0.0092 [+0.0059, +0.0129] | 50 / 7 |
| fillable_forms (100) | `forms_v2` | +0.0039 [+0.0001, +0.0082] | 10 / 1 |
| text (508) | `text_next` | +0.0005 | 4 / 2 |
| charts, layout, headings, strikeout, underline | | about 0 | |

21 pages score lower on some metric. A check against the page images gave this result:
- **10 pages: the branch reads better.** The scorer reads leftover text in a caption cell that is
  now whole as an "invented value" (for example REFUND on `740-2025__p3`).
- **2 pages: neutral** (`singapore--02_FormV39A--p1`, `clinical-trial-results-ebc_p3`).
- **8 pages: broken on `main` too, broken in a different way on the branch.** These are out of
  scope. They are `main` bugs in form grids, for a later pull request (section 8, item 14).
- **1 page where `main` is right and the branch is wrong:** `canada--37_ISP-1610--p1`.
  `main` gives `Is this your first request? Yes ✓ | No ...`. On the branch, the table ends
  before that row, the row prints as text, and the ✓ moves to the line "date of first request".
  This is out of scope for this pull request (section 8, item 15).

## 6. Version history

- **v1.** This was the first merge rule. The density gate excused covered cells, and the JSON used
  `kind: "merged_table"`. There were three regressions:
  - Header folding stopped (`AZ LIC Rate Tables 2.0.v2_page78`: 0.95 to 0.45).
  - Decorative page grids became one large table (`AONR32314_page1`: 0.59 to 0.07).
  - ParseBench drops `merged_table` blocks.
- **v2.** This version fixed the three regressions of v1.
- **v3.** This was v2 plus a fallback to the scrambled `main` output. It scored worse and was removed.
- **v4.** This was v2 plus `text_flows_across` (`USPS 10-k_page49` back to 1.00). It is commit `6fdbe7f`.
- **v5.** In this version, the build without merges decides acceptance. No table disappears now.
- **v6.** This was v5 plus the split evidence (rule 4) and `HeaderLayout` (rules 11 and 12). It is commit `2be666e`.
  `SERFF_CA ... page1724` went from -0.32 to +0.05.
- **v7.** In this version, the growth stops at a cell with text (rule 6). Before, every unruled boundary next to an
  empty cell merged in a row with a merge. That glued separate values: `qei-2026-08-12_page1`
  gave `-1.01 -0.13` in one cell. An independent review found this defect. It is commit `8164c9c`.
- **v8 and v9.** These versions added three rules:
  - The numeric glue guard (rule 6a).
  - The group-label split and the folded header (rules 16 and 17). `f21bd303b0de617f_p20` went
    from 0.54 to 0.83.
  - A form-band cut. The rows above the first dense row without merges left the table when one
    of their merged rows was not a group label.

  The form-band cut fixed the Ratemaking pages (`010b43388a0c67b0_p8`: 0.79 to 0.90). But on many
  other pages it cut real header and data rows (`dense_table_411`: 0.68 to 0.05). The v8 rule
  had one more defect: a column header with an empty first cell was not dense, so the rule also
  cut group-label headers. v9 fixed that defect.
- **v10.** This is v9 without the form-band cut (see the ablation in section 5). It is commit
  `ef9b90a`, and it has the best average score.
- **v11.** This is v10 plus rule 6b: no merges above the header band. It has fewer and smaller
  losses, but it gives up many gains (section 5). v11 does not cut rows from the table, so the
  affected rows read the same as on `main`.
- **v12 to v17.** v11 is reverted. These versions fix regressions that the merges cause
  (section 5). v17 has the best score.

## 7. The issue PDFs on the branch

- `frame.pdf` gives a 2-row header table and one `colspan="3"` cell. The body is in reading
  order.
- `frame_one_row.pdf` gives the same form, because `main` accepts this grid. This is the "HTML
  table with a colspan cell" option of the issue. The reporter prefers headings and
  paragraphs, so see open item 7.

## 8. Open items, by priority

1. **Boxes above a table join the grid (a `main` bug, for a later pull request).** Small boxes
   above a table (the grey "CDI File #" box, an input box) have an edge at the same x as the
   table's border. `cluster_v_segments` joins all vertical segments at one x into one line,
   across any gap. `split_component_at_grid_gaps` finds the cut, but it keeps the grid whole
   when one part has fewer than 3 horizontal lines, and a box has 2. Then the title and the
   form text between the box and the table become table rows. 155 of 1,753 table pages have a
   cancelled cut, 90 of them with the small part above the table (`vgap_classify.py`,
   `results/vgap_classified.txt`). Fix: when the small part is at the top or bottom edge of the
   grid, remove it and keep the cut. `1fea2f2540c835bf_p1` is a related case with a gap of
   only 10pt, below the 30pt minimum.
2. **Side-by-side tables in one grid** (`dense_table_289`, `dense_table_290`: 0.93 to 0). The
   output is more faithful, but the HTML form stops the scorer from splitting the two tables.
3. **Text glued across a gap** (`dense_table_041`, `currency-2026-08-13_page1`). Rule 6a looks
   only at numbers.
4. **Table segmentation changes in 13 held-out pages.** 9 score higher, and 1 is lower
   (`51abb0b9601cd8df_p9`: 0.66 to 0.61).
5. **Title row** (`SERFF_CA ... page96`, -0.14). A full-width title above the real header gives
   a title header and a body row. `Block::Table` has only one header row.
6. **Two header lines in one ruled row** (`SERFF_TX ... page1284`, -0.04). The grid cannot
   separate them, and `main` is also wrong there.
7. **A long body under a frame** still rejects the whole table (cases 03 and 04). A fix splits the grid at a tall
   row that spans all columns. `split_component_at_grid_gaps` must then look only at interior
   verticals.
8. The group-label rule (rules 11 and 12) comes from one example. Test it on more layouts.
9. **Possible bug, not tested.** `cluster_h_segments` unions the x-ranges of rules at one y.
   `rowspan_mask` can then miss a rowspan that has a gap in its rule.
10. Add tests for side-by-side tables and for form text above a table.
11. Do an independent check on documents that no version used, because the v8 to v11
    decisions used ParseBench-2.
12. `dense_table_443`: see section 5. `merge_continuation_rows` uses a row count, and the
    header fold changes the count.
13. `sampo`: a title in the header row takes the cell of the Q3/2024 column label.
14. **Form grids, broken on `main` and on the branch (out of scope).** The pages are
    `canada--30_ISP-1002--p1` and `--p3`, `canada--23_ISP-1300--p5`, `us--18_8822--p1`,
    `us--50_I-912--p8`, `uk--20_PA1P--p1` and `text_dense__livingWord`. The causes on the branch:
    - A form draws its own box edges at x positions that are not grid boundaries. The span test
      looks only for a rule at the grid boundary, so a span can cross a drawn edge (2A and 2B in
      one cell, and then all captions before all marks).
    - A merged cell that holds two text columns is read line by line, so the two columns mix
      (the 22A date inside the note, the Adoration times).
    - The header fold copies a title or note into 2 or 3 header cells, and it can pull in a data
      row or a title above the box.
    - Header text without rules is cut at every column boundary (8822 items 1 and 2, I-912).
    Possible fix: no span when a raw vertical rule lies inside the merged cell's x-range in that
    row.
15. **`canada--37_ISP-1610--p1` (out of scope):** the table ends at a different row on the
    branch, and the ✓ leaves "Yes". The cause is not known yet.

## 9. How to continue

### Build and test

```sh
cargo build --release -p liteparse --bin lit
cargo test -p liteparse --test ruled_colspan
cargo test -p liteparse --no-fail-fast   # 3 tests need LibreOffice (.doc conversion)
LITEPARSE_DEBUG_RULED=1 target/release/lit parse --no-ocr -q --format markdown file.pdf
```

`LITEPARSE_DEBUG_RULED=1` prints one line for each merged row:
`colspan row R y=..: merge across x=[...]`.

### Synthetic PDFs

```sh
python3 notes/issue-554/make_pdfs.py /tmp/pdfs              # 14 cases
python3 notes/issue-554/make_mid.py /tmp/pdfs_mid           # mid-table rows, header + footer
python3 notes/issue-554/make_stress.py /tmp/stress          # performance pages
python3 notes/issue-554/make_adversarial.py /tmp/adv notes/issue-554   # reviewer cases a-f
notes/issue-554/run_all.sh target/release/lit /tmp/pdfs > after.md
notes/issue-554/timeit.sh target/release/lit /tmp/stress/lattice_40000.pdf 9
```

The issue attachments are
https://github.com/user-attachments/files/34475842/frame.pdf and
https://github.com/user-attachments/files/34475858/frame_one_row.pdf.

### ParseBench

Build one `lit` binary for `main` and one for the branch. Then run each one:

```sh
git clone --depth 1 https://github.com/run-llama/ParseBench.git && cd ParseBench && uv sync
# v1 (0.59 GB, in-sample):
LITEPARSE_BIN=/path/to/lit .venv/bin/parse-bench run liteparse_markdown \
  --open_report=False --output_dir ../out_X --force -m 6
python3 notes/issue-554/compare_parsebench.py ../out_main ../out_branch
# ParseBench-2 (held out). Use only the subsets that are not v1:
LITEPARSE_BIN=/path/to/lit .venv/bin/parse-bench run liteparse_markdown --dataset parsebench-2 \
  --group complex_table_headers,dense_tables,financial_tables,insurance_filing_tables,forms,fillable_forms \
  --open_report=False --output_dir ../out2_X --force -m 6
```

After the first download, add `--input_dir <ParseBench>/data_parsebench-2` to a ParseBench-2
run. Without it, the harness checks the dataset on Hugging Face again, and it can stop with
HTTP 429 (rate limit). The ParseBench-2 download is the full dataset (about 1.6 GB), even with
`--group`. Newer clones of ParseBench do not keep each document's output; run `lit` on the PDF
to see the markdown. `compare_v10.py` and `ablate.py` read the newer output.

A v1 run takes 3 to 4 minutes on an Apple Silicon laptop. Each document's raw `lit` output is in
`<out>/liteparse_markdown/<category>/<name>.raw.json`. The v1 ground truth is in
`data/<category>.jsonl` (`expected_markdown`). `results/overlap.json` marks the ParseBench-2
pages that are also in v1.

### Files in `results/`

| File | Content |
|---|---|
| `compare_v7.txt` | v1 per-document changes, main against v7 (the v4 list was lost) |
| `compare_v9.txt` | ParseBench-2 (2026-11-24): main, v7, v9 and v9 without alignment |
| `heldout_v7.txt` | The held-out table of section 5 |
| `v2_summary.txt` | ParseBench-2 scores, main against v6 (from the review) |
| `judgments.tsv` | The reviewer's visual judgments of 35 pages |
| `glue_scan_variants.txt` | Glued number cells for each variant |
| `sensitivity_scores.txt` | Held-out scores for each constant variant |
| `overlap.json` | ParseBench-2 pages that are also in v1 |
| `compare_v10.txt` | ParseBench-2 (2026-11-24): main, v7 and v10, the glue scan, the table-count changes and each loss |
| `ablation.txt` | The ablation table of section 5 |
| `compare_v11.txt` | ParseBench-2 (2026-11-24): main, v7, v10 and v11, the glue scan, the table-count changes and each loss |

The repository ignores `*.txt`. Add result files with `git add -f`.

## 10. Fork note

A push to `main` of the fork `Choudhry18/liteparse` starts the upstream workflows. On
2026-11-18, GitHub did not start the jobs because of a billing lock on the account. A push of a
branch starts no workflow.
