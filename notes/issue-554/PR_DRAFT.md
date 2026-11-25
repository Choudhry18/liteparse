# fix(markdown): span ruled-table cells where a vertical rule stops (#554)

## Summary

This change fixes the scrambled reading order of issue #554. In a ruled table, a row can have
no vertical rule at a column boundary: a section row, a note row, a header label over two
columns, or a long body under a framed header. `main` cut the text of such a row at each column
boundary. The words went into different cells, and the row lost its reading order.

With this change, the row becomes one cell that spans the columns (`colspan`). Tables without
such a row give the same output as before.

- Markdown: a table with a spanning cell is an HTML `<table>` with `colspan`. Other tables stay
  pipe tables, the same as on `main`.
- `--extract-blocks` JSON: the block kind stays `"table"`, with one cell per column. The first
  cell of a span gets `colspan`. A covered cell is empty and has no box.
- Plain text and default `--format json` do not change. They were the same on all 1,753
  ParseBench-2 table pages.

## Strategy

The parser already has the drawn rules and the text positions. A row merges across a column
boundary only when the two signals agree:

1. **No drawn rule.** No vertical segment covers more than 20% of the row band at that
   boundary. The check uses the raw segments, because the clustered segments fill the gap
   that a spanning row leaves.
2. **The text runs across the boundary.** A span crosses the boundary by 3pt on each side, and a
   word box covers the boundary, or the gap at the boundary is less than half the font height.

A merged cell then grows into the empty cells next to it, up to the next drawn rule. Its text
stays in reading order in the first cell of the group.

## Safeguards against regressions

Each rule below comes from a regression that we found on ParseBench or in a review:

| Rule | It prevents |
|---|---|
| **The build without merges decides if a grid is a table.** This is the same decision as on `main`. The merges only change how an accepted table reads. | Tables that disappear, and decorative page frames that become tables. |
| **Split evidence.** No merge when one line of the row has separate text on both sides of the boundary. | Two header lines in one ruled row that join (`Change at Minimum Change at Maximum % %`). |
| **Gap test.** Text near a boundary is not enough: a word must cover it, or the gap must be small. | Separate amounts that join (`$ (32,766) $ (16,634)` on USPS 10-K p49). |
| **Number guard.** A merge is cancelled when it joins two separate numbers that each align with numbers in other rows. A digit-group space (`10 000`) stays one number. | Values from different columns in one cell (`533,324,048 533,324,046 ...`). |
| **Growth stops at text.** A merged cell grows only into empty cells. | Labels joined through an empty cell (`All Sectors` plus four sector labels). |
| **Lines outside the grid make no merge.** | Empty spanning cells from text that starts left of the grid. |

## Header passes

Merges change what the header passes see. These rules keep the header correct:

- A row with text only in spanning cells is a group-label row. It folds into the header below
  ("North America" over "Revenue | Units" gives "North America Revenue").
- When a table repeats its header, the group-label row above the repeat moves with it.
- A merged note row is not a data row. Before, a note row at the bottom made all data rows into
  one header (Coca-Cola 10-K p48).
- The bottom header row takes the split cells, because one text run can hold several labels
  ("Starter Growth Scale").
- A label covers all columns of its group only when drawn rules bound the group.
- A row more than 3 times the median row height is a paragraph, not a group label (a 10-K
  cover page inside a grid).
- A line stays with the table only when a cell holds its source text. Before, a repeated word
  in a folded header could make a line below the table look taken, and the line was lost.

## Benchmark results (ParseBench-2)

The official ParseBench-2 score uses 6 dimensions. All 13 subsets ran with the same `main` and
branch binaries:

| Dimension | main | this PR | Change |
|---|---|---|---|
| Tables | 0.3587 | 0.3734 | +0.0147 |
| Forms | 0.6856 | 0.6942 | +0.0086 |
| Text | 0.5510 | 0.5515 | +0.0005 |
| Layout | 0.4860 | 0.4862 | +0.0002 |
| Charts | 0.0107 | 0.0108 | +0.0001 |
| Formatting | 0.6622 | 0.6623 | +0.0000 |
| **Overall** | **0.4590** | **0.4631** | **+0.0040** |

The per-subset results follow. The score is `table_next` for tables and `forms_v2` for forms.
The "all pages" columns are the official subset scores. For the 4 table subsets that are new in
ParseBench-2, the change per page uses only the pages that are not also in ParseBench v1
(1,239 pages). Each interval is a 95% bootstrap interval of the mean change per page.

| Subset | Pages | main, all pages | this PR, all pages | Change per page | Better / worse |
|---|---|---|---|---|---|
| `complex_table_headers` | 149 | 0.0538 | 0.0557 | | |
| `financial_tables` | 319 | 0.2702 | 0.2838 | | |
| `insurance_filing_tables` | 412 | 0.3546 | 0.3791 | | |
| `dense_tables` | 373 | 0.4670 | 0.4705 | | |
| The 4 subsets above, pages not in v1 | 1,239 | | | +0.0129 [+0.0075, +0.0186] | 108 / 24 |
| `tables` (the ParseBench v1 pages) | 500 | 0.4310 | 0.4495 | +0.0186 [+0.0115, +0.0265] | 47 / 5 |
| `forms` | 500 | 0.6601 | 0.6693 | +0.0092 [+0.0059, +0.0129] | 50 / 7 |
| `fillable_forms` | 100 | 0.8640 | 0.8679 | +0.0039 [+0.0001, +0.0082] | 10 / 1 |
| `text`, `charts`, `layout`, `headings`, `strikeout`, `underline` | 1,943 | | | about 0 | |

**Note:** the development used ParseBench-2 to find regressions. Thus these subsets are not a
clean held-out test.

**Speed:** on 240 random table and form pages, the total time goes up by 1 to 2%: 3.06 s on
`main`, 3.11 s with this PR. A `main` against `main` run gives +0.5%, so the real cost is about
1%. That is about 0.15 ms on a 12 ms page. Most of it is the second build of a table that has
a merge. Stress pages with 40,000 strokes take the same time.

## Losses that come from the measurement

**The answer keys.** The ParseBench README says that the v1 pages (the `tables` subset) are
human-verified. For the ParseBench-2 subsets, the dataset gives review tags only in
`dense_tables`: 17 of 373 pages have `gt_reviewed`, and 281 have only `gt_draft`. The other
subsets have no review tag. Of the table pages that score lower with this PR, 8 are
`gt_draft` pages in `dense_tables`.

We compared each page that scores lower with an image of the page. On most of these pages, this
PR reads the page better or as well as `main`, and the score drop comes from how the scorer
reads the output:

- **Side-by-side tables** (`dense_table_289`, `dense_table_290`: 0.93 to 0; `dense_table_041`).
  The page has three tax tables side by side. Both `main` and this PR give one wide table. The
  scorer splits the pipe table of `main` into three, but it does not split the HTML table. It
  then reports 178 rows as missing. These three pages give a total drop of 2.26 (the sum of the
  per-page drops), the largest part of the table losses.
- **Folded headers.** On `dense_table_313`, the reference has "Component" over columns A to M.
  This PR gives the folded header "Component A ... Component M", and `main` gives "Component"
  over two columns only. The score goes from 1.00 to 0.95. On `64a4c79de95f6871_p8`, the
  reference has "Coverage: Bodily Injury" over 3 columns, the same as this PR, and the score
  goes down.
- **Whole cells read as extra values (forms).** A caption that is now one cell can hold printed
  text that the scorer reads as an invented value (REFUND on `740-2025__p3`, "( 7 )" on a
  Mexico form).
- **Titles in the grid.** A table title inside the ruled grid becomes one spanning header row.
  The data rows are the same (`SERFF_TX page706`, `test_page1`).

## Known limitations

- **Boxes above a table** (a `main` bug, not changed here). Small boxes above a table can share
  the table's border x. Then the title and form text between them become table rows. 90 of
  1,753 table pages have this layout. A separate PR can fix it in
  `split_component_at_grid_gaps`.
- **Form grids that are wrong on `main` too.** On 8 form pages, `main` and this PR are both
  wrong at the same field, in different ways. For example, a span can cross a box edge that is
  not a grid boundary.
- **Two pages read worse:**
  - On `canada--37_ISP-1610--p1`, the ✓ moves away from "Yes".
  - `dense_table_443` (tracked changes) goes from 0.001 to 0.
- **The issue layout.** The body under the framed header in the issue PDF is now in reading
  order, but it is one spanning cell. It does not become headings and paragraphs.

## Tests

- 12 synthetic-PDF tests in `crates/liteparse/tests/ruled_colspan.rs`. They cover four drawing
  styles of section rows, the issue frame, partial merges, header-only dividers, text runs
  across drawn rules, aligned numbers, repeated group headers, a merged note row under data
  rows, and a header text run over narrow columns. Each new header test fails without its fix.
- `cargo fmt` is clean. `cargo clippy` gives no new warnings.
- All other tests pass, except 3 tests that convert a `.doc` file. They need LibreOffice,
  and the test machine does not have it.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
