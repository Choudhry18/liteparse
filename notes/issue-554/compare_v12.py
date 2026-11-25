"""Compare main, v10 and v12 on ParseBench-2, and list each page that changed from v10 to v12.

Usage: compare_v12.py <scratch dir> [run name, default v12]
"""
import json
import random
import sys
from pathlib import Path

S = Path(sys.argv[1])
NEW = sys.argv[2] if len(sys.argv) > 2 else "v12"
B = S / "bench"
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(S / "gen/overlap.json"))}
HELD = ["complex_table_headers", "financial_tables", "insurance_filing_tables", "dense_tables"]


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
    "v10": scores("v2_abl_noband"),
    NEW: scores(f"v2_{NEW}"),
}
held = sorted(e for e in runs[NEW] if e.split("/")[0] in HELD and overlap.get(e) == "new")
insample = sorted(e for e in runs[NEW] if e.startswith("tables/"))
print(f"held-out pages: {len(held)}   in-sample tables pages: {len(insample)}")


def mean(xs):
    return sum(xs) / len(xs) if xs else float("nan")


print(f"\n{'subset (held out)':26}" + "".join(f"{k:>10}" for k in runs))
for sub in HELD:
    ex = [e for e in held if e.startswith(sub + "/")]
    print(f"{sub:26}" + "".join(f"{mean([runs[k].get(e, 0) for e in ex]):10.4f}" for k in runs))
print(f"{'tables, equal weight':26}" + "".join(
    f"{mean([mean([runs[k].get(e, 0) for e in held if e.startswith(s + '/')]) for s in HELD]):10.4f}" for k in runs))
print(f"{'tables (in-sample subset)':26}" + "".join(
    f"{mean([runs[k].get(e, 0) for e in insample]):10.4f}" for k in runs))

random.seed(0)
print()
for k in ("v10", NEW):
    for name, pages in (("held-out", held), ("in-sample", insample)):
        d = [runs[k].get(e, 0) - runs["main"].get(e, 0) for e in pages]
        boots = sorted(mean([random.choice(d) for _ in d]) for _ in range(2000))
        up = sum(x > 1e-9 for x in d)
        dn = sum(x < -1e-9 for x in d)
        worst = min(d)
        print(f"{k:5} vs main, {name:9} per page: {mean(d):+.4f}  95% CI [{boots[50]:+.4f}, {boots[1950]:+.4f}]"
              f"  better {up} worse {dn}  largest loss {worst:+.3f}")

print(f"\npages where {NEW} differs from v10 (score main / v10 / {NEW}):")
for e in sorted(held + insample, key=lambda e: runs[NEW].get(e, 0) - runs["v10"].get(e, 0)):
    a, b, c = runs["main"].get(e, 0), runs["v10"].get(e, 0), runs[NEW].get(e, 0)
    if abs(c - b) > 1e-9:
        print(f"   {c - b:+.3f}   {a:.3f} / {b:.3f} / {c:.3f}   {e}")

print(f"\nlosses, {NEW} vs main:")
for d, e in sorted((runs[NEW].get(e, 0) - runs["main"].get(e, 0), e) for e in held + insample):
    if d >= -1e-9:
        break
    print(f"   {d:+.3f}  main {runs['main'].get(e, 0):.3f}  v10 {runs['v10'].get(e, 0):.3f}  {NEW} {runs[NEW].get(e, 0):.3f}  {e}")
