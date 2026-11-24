"""List held-out pages where v7 output has glued number cells, with main and v7 scores.

Usage: glue_examples.py <out_main> <out_v7> <overlap.json> <data_dir> <lit_v7>
"""
import html, json, re, sys
from pathlib import Path

out_main, out_v7 = Path(sys.argv[1]), Path(sys.argv[2])
# overlap.json: [subset, page, status, path]; only "new" pages are held out.
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(sys.argv[3]))}
NUM = re.compile(r"^\(?[-$€£]?\d[\d,]*(\.\d+)?%?\)?$")


def glued_cells(md):
    cells = []
    for body in re.findall(r'<td[^>]*colspan="\d+"[^>]*>(.*?)</td>', md, re.S):
        words = html.unescape(re.sub(r"<[^>]+>", " ", body)).split()
        nums = [w for w in words if NUM.match(w)]
        if len(nums) >= 2 and len(nums) >= 0.6 * len(words):
            cells.append(" ".join(words))
    return cells


def load(out):
    res = {}
    for rep in out.glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
        d = json.loads(rep.read_text())
        for r in d["per_example_results"]:
            m = {x["metric_name"]: x["value"] for x in (r.get("metrics") or [])}
            score = m.get("table_next", m.get("grits_trm_composite"))
            res[r["example_id"]] = score
    return res


DATA = Path(sys.argv[4])  # data_parsebench-2
LIT = sys.argv[5]  # the v7 lit binary


def markdown(out, ex):
    # The harness no longer saves each page's output, so run the binary.
    import subprocess
    pdf = DATA / f"{ex}.pdf"
    return subprocess.run([LIT, "parse", str(pdf), "--format", "markdown", "--no-ocr",
                           "--quiet", "--no-links"], capture_output=True, text=True).stdout


sm, s7 = load(out_main), load(out_v7)
held = [ex for ex in s7 if overlap.get(ex) == "new"]
rows = []
for ex in held:
    cells = glued_cells(markdown(out_v7, ex))
    if cells:
        rows.append((ex, sm.get(ex), s7.get(ex), cells))
print(f"held-out pages: {len(held)}  with glued cells: {len(rows)}  cells: {sum(len(r[3]) for r in rows)}")
print(f"{'main':>6} {'v7':>6} {'delta':>7} cells  page")
for ex, a, b, cells in sorted(rows, key=lambda r: (r[2] or 0) - (r[1] or 0)):
    d = (b or 0) - (a or 0)
    print(f"{(a or 0):6.3f} {(b or 0):6.3f} {d:+7.3f} {len(cells):5}  {ex}  e.g. {cells[:2]}")
