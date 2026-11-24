"""Compare main, v7, v9 and v9 without alignment on ParseBench-2.

Usage: compare_v9.py <scratch dir>
"""
import html
import json
import random
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

S = Path(sys.argv[1])
B = S / "bench"
D = B / "ParseBench/data_parsebench-2"
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(S / "gen/overlap.json"))}
HELD = ["complex_table_headers", "financial_tables", "insurance_filing_tables", "dense_tables"]
NUM = re.compile(r"^\(?[-$€£]?\d[\d,]*(\.\d+)?%?\)?\*?$")


def scores(*outs):
    res = {}
    for out in outs:
        for rep in (B / out).glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
            for r in json.loads(rep.read_text())["per_example_results"]:
                m = {x["metric_name"]: x["value"] for x in (r.get("metrics") or [])}
                res[r["example_id"]] = m.get("table_next", m.get("grits_trm_composite")) or 0.0
    return res


runs = {
    "main": scores("v2_main", "v2_main_tables"),
    "v7": scores("v2_fixed7", "v2_fixed7_tables"),
    "v10": scores("v2_abl_noband"),
    "v11": scores("v2_v11"),
}
held = sorted(e for e in runs["v11"] if e.split("/")[0] in HELD and overlap.get(e) == "new")
insample = sorted(e for e in runs["v11"] if e.startswith("tables/"))
print(f"held-out pages: {len(held)}   in-sample tables pages: {len(insample)}")


def mean(xs):
    return sum(xs) / len(xs) if xs else float("nan")


print(f"\n{'subset (held out)':26}" + "".join(f"{k:>12}" for k in runs))
for sub in HELD:
    ex = [e for e in held if e.startswith(sub + "/")]
    print(f"{sub:26}" + "".join(f"{mean([runs[k].get(e, 0) for e in ex]):12.4f}" for k in runs))
print(f"{'tables, equal weight':26}" + "".join(
    f"{mean([mean([runs[k].get(e, 0) for e in held if e.startswith(s + '/')]) for s in HELD]):12.4f}" for k in runs))
print(f"{'tables (in-sample subset)':26}" + "".join(
    f"{mean([runs[k].get(e, 0) for e in insample]) if all(e in runs[k] for e in insample) else float('nan'):12.4f}"
    for k in runs))

random.seed(0)
print()
for k in ("v7", "v10", "v11"):
    d = [runs[k].get(e, 0) - runs["main"].get(e, 0) for e in held]
    boots = sorted(mean([random.choice(d) for _ in d]) for _ in range(2000))
    up = sum(x > 1e-9 for x in d)
    dn = sum(x < -1e-9 for x in d)
    print(f"{k:11} vs main, held-out per page: {mean(d):+.4f}  95% CI [{boots[50]:+.4f}, {boots[1950]:+.4f}]  up {up} down {dn}")


def run(binary, e, env=None):
    import os
    return subprocess.run([str(S / f"bin/lit-{binary}"), "parse", str(D / f"{e}.pdf"), "--format", "markdown",
                           "--no-ocr", "--quiet", "--no-links"], capture_output=True, text=True,
                          env={**os.environ, **(env or {})}).stdout


def glued(md):
    out = []
    for body in re.findall(r'<td[^>]*colspan="\d+"[^>]*>(.*?)</td>', md, re.S):
        w = html.unescape(re.sub(r"<[^>]+>", " ", body)).split()
        n = [x for x in w if NUM.match(x)]
        if len(n) >= 2 and len(n) >= 0.6 * len(w):
            out.append(" ".join(w))
    return out


def ntables(md):
    return md.count("<table>") + len(re.findall(r"(?m)^\|.*\|\n\|(?:\s*:?-+:?\s*\|)+\s*$", md))


def work(e):
    m, v7, v9 = run("main", e), run("fixed7", e), run("v11", e)
    v9n = ""
    return e, glued(v7), glued(v9), glued(v9n), ntables(m), ntables(v9), m != v9


with ThreadPoolExecutor(8) as pool:
    res = list(pool.map(work, held))
print()
for i, k in ((1, "v7"), (2, "v11")):
    pages = [r for r in res if r[i]]
    print(f"glued number cells {k:11}: {sum(len(r[i]) for r in pages):3} cells on {len(pages)} pages")
for r in res:
    if r[2]:
        print(f"   v11 still glued: {r[0]}  {r[2][:2]}")
print(f"\noutput differs from main on {sum(r[6] for r in res)} held-out pages")
seg = [(r[0], r[4], r[5]) for r in res if r[4] != r[5]]
print(f"table count changes main -> v11: {len(seg)}")
for e, a, b in seg:
    print(f"   {a:2} -> {b:2}  {e}  score {runs['main'].get(e, 0):.3f} -> {runs['v11'].get(e, 0):.3f}")

print("\nheld-out losses, v11 vs main (with v7 for the same page):")
losses = sorted((runs["v11"].get(e, 0) - runs["main"].get(e, 0), e) for e in held)
for d, e in losses:
    if d >= -1e-9:
        break
    print(f"   {d:+.3f}  main {runs['main'].get(e, 0):.3f}  v7 {runs['v7'].get(e, 0):.3f}  v10 {runs["v10"].get(e, 0):.3f}  v11 {runs['v11'].get(e, 0):.3f}  {e}")
print("\nin-sample tables subset, v11 vs main losses:")
for d, e in sorted((runs["v11"].get(e, 0) - runs["main"].get(e, 0), e) for e in insample):
    if d >= -1e-9:
        break
    print(f"   {d:+.3f}  main {runs['main'].get(e, 0):.3f}  v7 {runs['v7'].get(e, 0):.3f}  v10 {runs["v10"].get(e, 0):.3f}  v11 {runs['v11'].get(e, 0):.3f}  {e}")
