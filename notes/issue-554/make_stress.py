"""Stress page: a 30x5 ruled grid whose rows hold text crossing the column rules,
plus N short vertical strokes elsewhere on the page (a chart's hatching)."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent))
from make_pdfs import Page

def stress(n_strokes, rows=30, cols=5):
    p = Page()
    x0, x1, y0, pitch = 40, 555, 40, 12
    xs = [x0 + (x1 - x0) * c / cols for c in range(cols + 1)]
    for r in range(rows + 1):
        p.line(x0, y0 + pitch * r, x1, y0 + pitch * r)
    for x in xs:
        p.line(x, y0, x, y0 + pitch * rows)
    for r in range(rows):
        for c in range(cols):
            p.text(xs[c] + 3, y0 + pitch * r + 9, f"r{r}c{c}", size=7)
    # Hatching under the table: many short vertical strokes at many x positions.
    for i in range(n_strokes):
        x = 40 + (i * 7919) % 515
        y = 450 + (i * 104729) % 350
        p.line(x, y, x, y + 4)
    return p

out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=True)
for n in (0, 10000, 40000):
    (out / f"stress_{n}.pdf").write_bytes(stress(n).pdf())

# Chart-like candidate: a 60x40 gridline lattice (few labels) plus hatching.
for n in (10000, 40000):
    (out / f"lattice_{n}.pdf").write_bytes(stress(n, rows=30, cols=40).pdf())

# Same lattice, but every row's text crosses the column rules (worst case for
# the rule lookup, since no early exit applies).
def crossing_lattice(n_strokes, rows=30, cols=40):
    p = stress(n_strokes, rows=0, cols=cols)  # hatching only
    x0, x1, y0, pitch = 40, 555, 40, 12
    xs = [x0 + (x1 - x0) * c / cols for c in range(cols + 1)]
    for r in range(rows + 1):
        p.line(x0, y0 + pitch * r, x1, y0 + pitch * r)
    for x in xs:
        p.line(x, y0, x, y0 + pitch * rows)
    for r in range(rows):
        p.text(x0 + 3, y0 + pitch * r + 9, " ".join(f"w{r}x{i}" for i in range(40)), size=7)
    return p

for n in (10000, 40000):
    (out / f"lattice_cross_{n}.pdf").write_bytes(crossing_lattice(n).pdf())
