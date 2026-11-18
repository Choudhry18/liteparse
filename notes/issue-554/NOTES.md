# Issue 554: ruled table rows that span columns

Work notes for https://github.com/run-llama/liteparse/issues/554.
Branch: `fix/ruled-colspan-missing-vrule`, based on upstream `main` at `a684a4a` (v2.20.0).

**Drop this `notes/` commit before you open a pull request.**

## 1. The problem

A ruled table can have a row where the interior vertical rules stop. That row is one cell
that spans the columns. The markdown output split each line of that row at the column
positions of the rows above. The text then came out in the wrong order.

The cause is in `crates/liteparse/src/markdown_layout/tables.rs`:

- `build_ruled_table_from` uses one set of column boundaries (`xs`) for all rows. It does not
  look at the y-extent of each vertical rule.
- `assign_cells` splits each span at every boundary that the span crosses.
- `rowspan_mask` finds cells that merge vertically. No code found cells that merge
  horizontally.
- `cluster_v_segments` unions the y-ranges of all rules at one x. This fills the gap that a
  spanning row leaves in a rule. Only the raw segments (`raw_vs`) show the gap.

## 2. What the branch changes

| File | Change |
|---|---|
| `markdown_layout/tables.rs` | New `colspan_mask` and `text_flows_across`. `assign_cells` puts the text of a merged group into the first cell of the group. `raw_vs` goes through `build_ruled_table` to `build_ruled_table_from`. |
| `markdown_layout/blocks.rs` | New field `Cell.covered`. Markdown renders a `Block::Table` with covered cells as an HTML `<table>` with `colspan` (`table_span_rows`). |
| `layout.rs` | The layout JSON keeps `kind: "table"` and one cell per column. The first cell of a group gets `colspan`. A covered cell is an empty cell with no `bbox`. |
| `markdown_layout/cross_region.rs` | Adds `..Cell::default()` to a `Cell` literal. |
| `tests/ruled_colspan.rs` | 6 tests with synthetic PDFs. |

### The merge rule

In row `r`, column `c` joins column `c - 1` only when both conditions are true:

1. **No rule in the row.** The raw vertical segments at `xs[c]` cover less than 20% of the row
   band (`COLSPAN_MIN_RULE_COVERAGE`). The test uses the row band, not the junction point.
2. **The text flows across the boundary.** A span crosses `xs[c]` by 3pt on each side. Also, a
   word box covers the boundary, or the gap at the boundary is less than half the font height.

In a row with a merge, an unruled boundary next to an empty cell also merges. Thus a spanning
row whose text ends early still spans the full width.

The density gate (`passes_density_gate`) counts covered cells as empty, the same as on `main`.
`repl` and `row_alpha_spanner` still see all the columns of a span, so `flatten_header_band`
works the same as on `main`.

## 3. Alternatives that were tested

The script `make_pdfs.py` makes 14 synthetic cases. A temporary switch ran each rule on the same
build. A tick means correct output.

| Case | Straddle only | Rules only | Junction point | Both (chosen) |
|---|---|---|---|---|
| 01-02 issue frame | yes | yes | no, misses | yes |
| 05-07, 09 section row, 4 drawing styles | yes | yes | no, misses | yes |
| 08 partial merge, columns 0-1 | yes | yes | no | yes |
| 10 dividers in header only, body in columns | yes | **no, merges each body row** | no | yes |
| 11 one text run across a drawn rule | **no, merges** | yes | yes | yes |
| 12-14 plain, rowspan, dashed | yes | yes | yes | yes |

- The junction-point test fails because a rule that ends on the row's top edge touches the
  junction. When rules are drawn one row at a time, both junctions exist, but the edge does not.
- Case 07 draws each rule as two long strokes with a gap at the section row. Only `raw_vs`
  shows that gap.

## 4. Performance

The first version scanned all page strokes for each (row, column). The fix does two things:

- It stops early when no text crosses a boundary.
- It puts each stroke in the bucket of its nearest boundary, in one pass.

| Stress page (`make_stress.py`) | main | before fix | final |
|---|---|---|---|
| Ruled table + 40k other strokes | 96-101ms | 99ms | 95-101ms |
| 30x40 lattice + 40k strokes | 97-104ms | 132ms | 97-105ms |
| Same lattice, text crosses each row | 97-103ms | 132ms | 103-106ms |

## 5. Benchmark: ParseBench v1

ParseBench v1 has 2,049 documents. The runs used no OCR. Only ParseBench gives a useful result
for this change:

- olmOCR-bench has no published LiteParse runner.
- The published opendataloader-bench engine scores `result.text` (plain text) from the
  `liteparse` 1.2.1 Python package. It does not see markdown tables.

| Category | main | v1 | v2 | v3 | v4 (branch) |
|---|---|---|---|---|---|
| Tables | 0.4581 | 0.4603 | 0.4667 | 0.4658 | **0.4662** |
| Charts | 0.0123 | 0.0194 | 0.0118 | 0.0123 | 0.0118 |
| Text content | 0.7060 | 0.7065 | 0.7063 | 0.7061 | 0.7063 |
| Text formatting | 0.4440 | 0.4430 | 0.4467 | 0.4426 | 0.4467 |
| Layout | 0.3203 | 0.3112 | 0.3327 | 0.3189 | **0.3327** |
| **Overall** | 0.3881 | 0.3881 | 0.3929 | 0.3891 | **0.3928** |
| Documents better / worse | | | 104 / 33 | 47 / 23 | 95 / 26 |

`lit` output is deterministic. Two runs of the `main` binary gave the same output.

### What each version changed

- **v1.** This was the first merge rule. The density gate excused covered cells, and the JSON used `kind: "merged_table"`.
  Three regressions occurred:
  - The merge path did not fill `repl` or set `row_alpha_spanner`. `flatten_header_band` then
    stopped folding headers such as `North America` over `Revenue | Units`. The table metric
    (TRM) keys on the folded header. Examples: `AZ LIC Rate Tables 2.0.v2_page78` (0.95 to 0.45)
    and `synthetic_colspan_table_no_borders_page1`.
  - The density gate excused covered cells. Decorative page grids then became one large table.
    Example: `AONR32314_page1` (0.59 to 0.07).
  - The ParseBench LiteParse provider maps only `table` and `grid_fallback` to the Table label.
    It dropped `merged_table` blocks. The layout score fell, and 2 pages failed.
- **v2.** This version fixed the three regressions above.
- **v3.** This was v2 plus a fallback: when a merged grid fails a gate, the build runs again without
  merges. The score was worse. Many v2 gains come from decorative grids that the gate rejects.
  The branch does not include the fallback.
- **v4.** This was v2 plus `text_flows_across`. Example: `USPS 10-k_page49` (1.00 in main, 0.71 in v2,
  1.00 in v4). PDFium joined two number cells into one run, and v2 merged them.

### Remaining losses in v4

`results/compare_v4.txt` has the full list. These are the largest:

| Document | main | v4 |
|---|---|---|
| table/SERFF_CA_random_pages 1_page1551 | 0.7566 | 0.0000 |
| text/text_simple__yell | 0.6697 | 0.0000 |
| table/203924_fb04e77929bf4bbc93dbd659653a4f01_page23 | 0.7942 | 0.2136 |
| layout/dupont_2024sustainabilityreport_p22 | 0.6364 | 0.2424 |
| table/SERFF_CA_random_pages 1_page1724 | 0.4814 | 0.1621 |
| text/text_simple__returns | 1.0000 | 0.6867 |
| layout/2024-Ford-Integrated-Sustainability-and-Financial-Report_Final_p46 | 0.4146 | 0.1220 |
| table/DS5795A-06_page13 | 0.2200 | 0.0000 |
| table/FBLB-134215544_page163 | 0.2017 | 0.0000 |
| table/FBLB-134215544_page99 | 0.1388 | 0.0000 |

A score of 0.0000 usually means that the table is gone. The merges leave empty covered cells,
and then the grid fails the density gate. Example: `FBLB-134215544_page163` goes from an
empty-cell fraction that passes to 0.67. Note that `main` also gives a bad table for that page.
Nobody looked at `text_simple__yell` or `SERFF_CA ... page1724` yet.

## 6. The issue PDFs on the branch

- `frame.pdf` (2 header rows) gives a 2-row table and one `colspan="3"` cell. The body is in
  reading order.
- `frame_one_row.pdf` (1 header row) fails the density gate: 2 of 6 cells are covered. The
  output is the header as one line, then `# 1. Section`, then the body text in order. The
  reporter said that this output is the best for them.

## 7. Open items

1. **A long body under a frame still rejects the whole table** (cases 03-04). The straddle
   census in `assign_cells` counts lines that cross missing rules. The header then becomes one
   line. A better fix splits the grid at a tall row that spans all columns. For example, the row
   is 3 or more times the median row height and holds several lines. `split_component_at_grid_gaps`
   does a similar split, but it looks at all verticals. It must look only at interior verticals.
   A ruled header and a ruled footer around a page body (`make_mid.py`, `frame_hdr_ftr_*`) need
   this split.
2. Find the cause of each loss in section 5.
3. A 1-row piece after a split cannot be a table, because a table needs 2 rows.
4. **Possible bug, not tested:** `cluster_h_segments` also unions the x-ranges of rules at one y.
   `rowspan_mask` can then miss a rowspan that a rule draws in two pieces with a gap.
5. Before a pull request, run `cargo clippy` in strict mode the same way as CI. The local clippy
   reports old warnings in `tables.rs`. The branch adds no new warnings.

## 8. How to continue

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
python3 notes/issue-554/make_pdfs.py /tmp/pdfs      # 14 cases
python3 notes/issue-554/make_mid.py /tmp/pdfs_mid   # mid-table and header+footer frames
python3 notes/issue-554/make_stress.py /tmp/stress  # performance pages
notes/issue-554/run_all.sh target/release/lit /tmp/pdfs > after.md
notes/issue-554/timeit.sh target/release/lit /tmp/stress/lattice_40000.pdf 9
```

The issue attachments are
https://github.com/user-attachments/files/34475842/frame.pdf and
https://github.com/user-attachments/files/34475858/frame_one_row.pdf.

### ParseBench

The dataset is 0.59 GB. Build one `lit` binary for `main` and one for the branch. Then run:

```sh
git clone --depth 1 https://github.com/run-llama/ParseBench.git && cd ParseBench && uv sync
LITEPARSE_BIN=/path/to/lit-main .venv/bin/parse-bench run liteparse_markdown \
  --open_report=False --output_dir ../out_main --force -m 6
LITEPARSE_BIN=/path/to/lit-branch .venv/bin/parse-bench run liteparse_markdown \
  --open_report=False --output_dir ../out_branch --force -m 6
python3 notes/issue-554/compare_parsebench.py ../out_main ../out_branch
```

Each run takes 3 to 4 minutes on an Apple Silicon laptop. Each document's raw `lit` output is in
`<out>/liteparse_markdown/<category>/<name>.raw.json`. The ground truth is in
`data/<category>.jsonl` (`expected_markdown`).

## 9. Fork note

A push to `main` of the fork `Choudhry18/liteparse` starts the upstream workflows. On
2026-11-18, GitHub did not start the jobs because of a billing lock on the account. A push of a
branch starts no workflow.
