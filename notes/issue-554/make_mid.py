"""Mid-table rows without interior rules: how they move the straddle fraction."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
from make_pdfs import Page, XS, GRID_TOP, PITCH, rows_of

DATA = [["Item", "Description", "Amount"], ["A-100", "Steel bracket", "12.50"],
        ["A-200", "Copper fitting", "8.75"]]
TAIL = [["B-100", "Brass valve", "31.00"], ["B-200", "Rubber seal", "2.10"]]

def table_with_mid_row(n_lines):
    """Rows: 3 data rows, one open row of n_lines lines, 2 data rows. Rules per row."""
    p = Page()
    y = GRID_TOP
    bounds = [y]
    def data_row(cells, bold=False):
        nonlocal y
        for x, s in zip(XS, cells):
            p.text(x + 6, y + 15, s, bold=bold)
        for x in XS[1:-1]:
            p.line(x, y, x, y + PITCH)
        y += PITCH; bounds.append(y)
    for i, r in enumerate(DATA):
        data_row(r, bold=(i == 0))
    h = 13 * n_lines + 9
    for i in range(n_lines):
        p.text(XS[0] + 6, y + 15 + 13 * i, rows_of(f"note_{i + 1}"))
    y += h; bounds.append(y)
    for r in TAIL:
        data_row(r)
    for b in bounds:
        p.line(XS[0], b, XS[-1], b)
    p.line(XS[0], GRID_TOP, XS[0], y); p.line(XS[-1], GRID_TOP, XS[-1], y)
    return p

def frame_header_and_footer(n_lines):
    """Page frame: ruled 1x3 header at top, open body, ruled 1x3 footer at bottom."""
    p = Page()
    top, bottom = 60, 780
    p.rect(XS[0], top, XS[-1], bottom)
    p.line(XS[0], top + 25, XS[-1], top + 25)
    p.line(XS[0], bottom - 25, XS[-1], bottom - 25)
    for x in XS[1:-1]:
        p.line(x, top, x, top + 25)
        p.line(x, bottom - 25, x, bottom)
    for x, s in zip(XS, ["Lorem ipsum", "Dolor sit amet", "Page 1"]):
        p.text(x + 6, top + 16, s)
    for x, s in zip(XS, ["Doc 42-A", "Revision 3", "Approved"]):
        p.text(x + 6, bottom - 9, s)
    y = top + 43
    for i in range(n_lines):
        p.text(63, y, rows_of(f"row_{i + 1}")); y += 13
    return p

out = Path(sys.argv[1])
for n in (1, 2, 3, 6):
    (out / f"mid_row_{n}_lines.pdf").write_bytes(table_with_mid_row(n).pdf())
for n in (1, 3, 6):
    (out / f"frame_hdr_ftr_{n}_lines.pdf").write_bytes(frame_header_and_footer(n).pdf())
