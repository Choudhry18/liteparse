"""Adversarial synthetic ruled tables for the colspan change (review only)."""
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[2])
from make_pdfs import Page  # noqa: E402

TOP, PITCH = 100, 22

def grid(p, xs, n_rows, vrule_rows):
    """Full-width h rules; interior v rules only in rows listed in vrule_rows (None = all)."""
    for i in range(n_rows + 1):
        p.line(xs[0], TOP + PITCH * i, xs[-1], TOP + PITCH * i)
    p.line(xs[0], TOP, xs[0], TOP + PITCH * n_rows)
    p.line(xs[-1], TOP, xs[-1], TOP + PITCH * n_rows)
    for r in range(n_rows):
        if vrule_rows is not None and r not in vrule_rows:
            continue
        for x in xs[1:-1]:
            p.line(x, TOP + PITCH * r, x, TOP + PITCH * (r + 1))

def put(p, xs, rows, right_cols=()):
    for r, row in enumerate(rows):
        for c, s in enumerate(row):
            if not s:
                continue
            x = xs[c] + 4
            if c in right_cols:
                x = xs[c + 1] - 4 - 5.0 * len(s)  # approx right align (Helvetica digits ~5.56)
            p.text(x, TOP + PITCH * r + 15, s, bold=(r == 0))

XS5 = [57, 150, 250, 340, 430, 520]

def a_header_only_overflow_sparse():
    """Body has no interior v rules. Row 2 label overflows into col 1; cols 2 and 4 hold values, col 3 empty."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "100", "90", "80"],
        ["Net income attributable to holders", "", "55", "", "44"],
        ["Costs", "2", "40", "35", "30"],
        ["Tax", "3", "10", "9", "8"],
    ]
    grid(p, XS5, len(rows), {0})
    put(p, XS5, rows)
    return p

def b_full_rules_section_row_with_sparse():
    """All rows ruled except row 2, which has no interior rules. Row 2: a section label spans cols 0-1, plus values in col 2 and col 4."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "100", "90", "80"],
        ["Continuing operations total", "", "55", "", "44"],
        ["Costs", "2", "40", "35", "30"],
        ["Tax", "3", "10", "9", "8"],
    ]
    grid(p, XS5, len(rows), {0, 1, 3, 4})
    put(p, XS5, rows)
    return p

def c_header_only_overflow_dense():
    """Body has no interior v rules. Long label overflows into an empty col 1; other cols full."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "100", "90", "80"],
        ["Net income attributable to holders", "", "55", "50", "44"],
        ["Costs", "2", "40", "35", "30"],
        ["Tax", "3", "10", "9", "8"],
    ]
    grid(p, XS5, len(rows), {0})
    put(p, XS5, rows)
    return p

def d_fully_ruled_overflow_text():
    """All rules drawn. Row 2's col-0 text runs past the rule into col 1 (Excel overflow); col 1 empty."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "100", "90", "80"],
        ["Net income attributable to holders", "", "55", "50", "44"],
        ["Costs", "2", "40", "35", "30"],
        ["Tax", "3", "10", "9", "8"],
    ]
    grid(p, XS5, len(rows), None)
    put(p, XS5, rows)
    return p

def e_header_only_wide_numbers():
    """Body has no interior v rules; numbers in col 3 are right-aligned and wide enough to cross into col 2 by a few pt."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "", "1,234,567,890.00", "80"],
        ["Costs", "2", "", "987,654,321.00", "30"],
        ["Tax", "3", "10", "9", "8"],
    ]
    grid(p, XS5, len(rows), {0})
    put(p, XS5, rows, right_cols=(3,))
    return p

def f_section_row_then_values_far():
    """Fully ruled except the section row; label in col 0 spans 0-2; a footnote ref in col 4 of that row."""
    p = Page()
    rows = [
        ["Item", "Note", "2023", "2022", "2021"],
        ["Revenue", "1", "100", "90", "80"],
        ["Discontinued operations (see note 7 below)", "", "", "", "(a)"],
        ["Costs", "2", "40", "35", "30"],
    ]
    grid(p, XS5, len(rows), {0, 1, 3})
    put(p, XS5, rows)
    return p

CASES = [a_header_only_overflow_sparse, b_full_rules_section_row_with_sparse, c_header_only_overflow_dense,
         d_fully_ruled_overflow_text, e_header_only_wide_numbers, f_section_row_then_values_far]
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
for c in CASES:
    (out / f"{c.__name__}.pdf").write_bytes(c().pdf())
