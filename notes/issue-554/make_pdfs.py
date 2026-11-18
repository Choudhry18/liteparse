"""Generate synthetic ruled-table PDFs for issue 554 (colspan from missing vertical rules).

All coordinates in this script use a top-left origin (like liteparse's viewport space).
The writer flips y for the PDF content stream. Page size is A4 (595 x 842).
"""

import sys
from pathlib import Path

W, H = 595, 842


class Page:
    def __init__(self):
        self.ops = ["0.5 w"]

    def line(self, x1, y1, x2, y2, dash=None):
        if dash:
            self.ops.append(f"[{dash[0]} {dash[1]}] 0 d")
        self.ops.append(f"{x1} {H - y1} m {x2} {H - y2} l S")
        if dash:
            self.ops.append("[] 0 d")

    def rect(self, x0, y0, x1, y1):
        self.ops.append(f"{x0} {H - y1} {x1 - x0} {y1 - y0} re S")

    def text(self, x, baseline, s, size=10, bold=False):
        s = s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")
        font = "F2" if bold else "F1"
        self.ops.append(f"BT /{font} {size} Tf {x} {H - baseline} Td ({s}) Tj ET")

    def pdf(self):
        content = "\n".join(self.ops)
        objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] "
            "/Resources << /Font << /F1 4 0 R /F2 5 0 R >> >> /Contents 6 0 R >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>",
            f"<< /Length {len(content)} >>\nstream\n{content}\nendstream",
        ]
        out = bytearray(b"%PDF-1.7\n")
        offsets = []
        for i, obj in enumerate(objects):
            offsets.append(len(out))
            out += f"{i + 1} 0 obj\n{obj}\nendobj\n".encode()
        xref = len(out)
        out += f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode()
        for off in offsets:
            out += f"{off:010} 00000 n \n".encode()
        out += (
            f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
            f"startxref\n{xref}\n%%EOF\n"
        ).encode()
        return bytes(out)


XS = [57, 190, 400, 538]  # column boundaries, as in the issue PDFs


def framed_header(p, header_rows, body_bottom=780):
    """Issue layout: an outer frame to `body_bottom`, interior rules only in the header band."""
    top = 60
    p.rect(XS[0], top, XS[-1], body_bottom)
    ys = [top + 25 * (i + 1) for i in range(len(header_rows))]
    for y in ys:
        p.line(XS[0], y, XS[-1], y)
    for x in XS[1:-1]:
        p.line(x, top, x, ys[-1])
    for r, row in enumerate(header_rows):
        for x, s in zip(XS, row):
            p.text(x + 6, top + 25 * r + 16, s)
    return ys[-1]


def section(p, y, title, line_texts):
    p.text(63, y, title, bold=True)
    y += 13
    for s in line_texts:
        p.text(63, y, s)
        y += 13
    return y + 13


def rows_of(tag, n=10):
    return ", ".join([tag] * n)


def issue_one_row():
    p = Page()
    y = framed_header(p, [["Lorem ipsum", "Dolor sit amet", "Page 1"]]) + 18
    section(p, y, "1. Section", [rows_of(f"row_{i}") for i in (1, 2, 3)])
    return p


def issue_two_rows():
    p = Page()
    y = framed_header(
        p,
        [["Lorem ipsum", "Dolor sit amet", "Page 1"], ["Consectetur", "Adipiscing elit", "No. 42"]],
    ) + 18
    y = section(p, y, "1. Section", [rows_of(f"row_{i}") for i in (1, 2, 3)])
    section(p, y, "2. Section", [rows_of(f"row_{i}") for i in (4, 5, 6)])
    return p


def issue_long_body():
    p = Page()
    y = framed_header(p, [["Lorem ipsum", "Dolor sit amet", "Page 1"]]) + 18
    n = 1
    for s in range(1, 4):
        y = section(p, y, f"{s}. Section", [rows_of(f"row_{n + i}") for i in range(5)])
        n += 5
    return p


PROSE = [
    "The contractor shall deliver the goods to the site named in the order. Delivery",
    "takes place between eight and five on working days. The buyer inspects the goods",
    "on arrival and reports any damage in writing within three days of the delivery.",
]


def issue_prose_body():
    p = Page()
    y = framed_header(p, [["Lorem ipsum", "Dolor sit amet", "Page 1"]]) + 18
    y = section(p, y, "1. Delivery", PROSE)
    section(p, y, "2. Payment", PROSE)
    return p


GRID_ROWS = [
    ["Item", "Description", "Amount"],
    ["A-100", "Steel bracket", "12.50"],
    ["A-200", "Copper fitting", "8.75"],
    None,  # section row: one cell across all columns
    ["B-100", "Brass valve", "31.00"],
    ["B-200", "Rubber seal", "2.10"],
]
SECTION_TEXT = "Group B: parts that ship from the second warehouse, priced in euro"
GRID_TOP, PITCH = 100, 22


def grid_text(p, rows, section_text=SECTION_TEXT, bold_header=True):
    for r, row in enumerate(rows):
        base = GRID_TOP + PITCH * r + 15
        if row is None:
            p.text(XS[0] + 6, base, section_text)
            continue
        for x, s in zip(XS, row):
            p.text(x + 6, base, s, bold=(bold_header and r == 0))


def grid_hlines(p, n_rows):
    for r in range(n_rows + 1):
        y = GRID_TOP + PITCH * r
        p.line(XS[0], y, XS[-1], y)


def section_row_segments():
    """Mid-table section row; every interior rule is drawn per row, and the section row has none."""
    p = Page()
    n = len(GRID_ROWS)
    grid_hlines(p, n)
    p.line(XS[0], GRID_TOP, XS[0], GRID_TOP + PITCH * n)
    p.line(XS[-1], GRID_TOP, XS[-1], GRID_TOP + PITCH * n)
    for r, row in enumerate(GRID_ROWS):
        if row is None:
            continue
        y0, y1 = GRID_TOP + PITCH * r, GRID_TOP + PITCH * (r + 1)
        for x in XS[1:-1]:
            p.line(x, y0, x, y1)
    grid_text(p, GRID_ROWS)
    return p


def section_row_rects():
    """Same table, drawn as one stroked rect per cell; the section row is one wide rect."""
    p = Page()
    for r, row in enumerate(GRID_ROWS):
        y0, y1 = GRID_TOP + PITCH * r, GRID_TOP + PITCH * (r + 1)
        if row is None:
            p.rect(XS[0], y0, XS[-1], y1)
            continue
        for c in range(3):
            p.rect(XS[c], y0, XS[c + 1], y1)
    grid_text(p, GRID_ROWS)
    return p


def section_row_one_stroke():
    """Interior rules drawn as two long strokes each, broken only at the section row."""
    p = Page()
    n = len(GRID_ROWS)
    grid_hlines(p, n)
    p.line(XS[0], GRID_TOP, XS[0], GRID_TOP + PITCH * n)
    p.line(XS[-1], GRID_TOP, XS[-1], GRID_TOP + PITCH * n)
    sec = GRID_ROWS.index(None)
    for x in XS[1:-1]:
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * sec)
        p.line(x, GRID_TOP + PITCH * (sec + 1), x, GRID_TOP + PITCH * n)
    grid_text(p, GRID_ROWS)
    return p


def partial_merge():
    """Row 3 merges columns 0-1 only; the rule at x=400 continues through it."""
    p = Page()
    rows = [r for r in GRID_ROWS]
    n = len(rows)
    grid_hlines(p, n)
    for x in (XS[0], XS[2], XS[3]):
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * n)
    sec = rows.index(None)
    p.line(XS[1], GRID_TOP, XS[1], GRID_TOP + PITCH * sec)
    p.line(XS[1], GRID_TOP + PITCH * (sec + 1), XS[1], GRID_TOP + PITCH * n)
    for r, row in enumerate(rows):
        base = GRID_TOP + PITCH * r + 15
        if row is None:
            p.text(XS[0] + 6, base, "Note: the B parts ship from warehouse two")
            p.text(XS[2] + 6, base, "40.00")
            continue
        for x, s in zip(XS, row):
            p.text(x + 6, base, s, bold=(r == 0))
    return p


def inset_vrules():
    """Per-row interior rules inset 1.5pt from each horizontal rule (gaps at every junction)."""
    p = Page()
    n = len(GRID_ROWS)
    grid_hlines(p, n)
    p.line(XS[0], GRID_TOP, XS[0], GRID_TOP + PITCH * n)
    p.line(XS[-1], GRID_TOP, XS[-1], GRID_TOP + PITCH * n)
    for r, row in enumerate(GRID_ROWS):
        if row is None:
            continue
        y0, y1 = GRID_TOP + PITCH * r + 1.5, GRID_TOP + PITCH * (r + 1) - 1.5
        for x in XS[1:-1]:
            p.line(x, y0, x, y1)
    grid_text(p, GRID_ROWS)
    return p


HEADER_ONLY_ROWS = [
    ["Item", "Description", "Amount"],
    ["A-100", "Steel bracket", "12.50"],
    ["A-200", "Copper fitting", "8.75"],
    ["A-300", "Brass valve", "31.00"],
    ["A-400", "Rubber seal", "2.10"],
]


def header_only_vrules():
    """Control: interior rules rule only the header band; body text stays inside its columns."""
    p = Page()
    n = len(HEADER_ONLY_ROWS)
    grid_hlines(p, n)
    p.line(XS[0], GRID_TOP, XS[0], GRID_TOP + PITCH * n)
    p.line(XS[-1], GRID_TOP, XS[-1], GRID_TOP + PITCH * n)
    for x in XS[1:-1]:
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH)
    grid_text(p, HEADER_ONLY_ROWS)
    return p


def merged_run_with_rule():
    """Control: all rules drawn; one row writes two cells with a single Tj that crosses x=400."""
    p = Page()
    rows = [
        ["Item", "Description", "Amount"],
        ["A-100", "Steel bracket", "12.50"],
        ["A-200", "Copper fitting", "8.75"],
        ["A-300", "Brass valve", "31.00"],
    ]
    n = len(rows) + 1
    grid_hlines(p, n)
    for x in XS:
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * n)
    grid_text(p, rows)
    base = GRID_TOP + PITCH * len(rows) + 15
    # One text object for col 1 and col 2: "Rubber seal" ends at x=397.4 and
    # "2.10" starts at x=403.0 (Helvetica widths: "Rubber seal" 53.91pt, space 2.78pt).
    p.text(XS[0] + 6, base, "A-400")
    p.text(round(XS[2] + 3 - 53.91 - 2 * 2.78, 2), base, "Rubber seal  2.10")
    return p


def plain_grid():
    p = Page()
    rows = HEADER_ONLY_ROWS
    n = len(rows)
    grid_hlines(p, n)
    for x in XS:
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * n)
    grid_text(p, rows)
    return p


def rowspan_control():
    """Control: column 0 merges rows 1-2; the rule between them is drawn from x=190 only."""
    p = Page()
    rows = [
        ["Group", "Description", "Amount"],
        ["Fasteners", "Steel bracket", "12.50"],
        ["", "Copper fitting", "8.75"],
        ["Seals", "Brass valve", "31.00"],
        ["Misc", "Rubber seal", "2.10"],
    ]
    n = len(rows)
    for r in range(n + 1):
        y = GRID_TOP + PITCH * r
        x0 = XS[1] if r == 2 else XS[0]
        p.line(x0, y, XS[-1], y)
    for x in XS:
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * n)
    grid_text(p, rows)
    return p


def dashed_vrules():
    """Control: full grid, interior rules dashed with a dash pattern."""
    p = Page()
    rows = HEADER_ONLY_ROWS
    n = len(rows)
    grid_hlines(p, n)
    for x in XS:
        dash = (2, 2) if x not in (XS[0], XS[-1]) else None
        p.line(x, GRID_TOP, x, GRID_TOP + PITCH * n, dash=dash)
    grid_text(p, rows)
    return p


CASES = [
    issue_one_row,
    issue_two_rows,
    issue_long_body,
    issue_prose_body,
    section_row_segments,
    section_row_rects,
    section_row_one_stroke,
    partial_merge,
    inset_vrules,
    header_only_vrules,
    merged_run_with_rule,
    plain_grid,
    rowspan_control,
    dashed_vrules,
]

if __name__ == "__main__":
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    for i, case in enumerate(CASES, 1):
        path = out / f"{i:02}_{case.__name__}.pdf"
        path.write_bytes(case().pdf())
        print(path.name)
