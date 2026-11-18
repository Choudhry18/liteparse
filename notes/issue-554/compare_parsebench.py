"""Compare two ParseBench output dirs (liteparse_markdown pipeline).

Usage: compare_parsebench.py <out_before> <out_after>

Prints the per-category canonical metric (the same choice as run_benchmarks.sh),
then every example whose primary score changed, sorted by delta.
"""

import json
import sys
from pathlib import Path

PIPE = "liteparse_markdown"
CATS = [
    ("table", ["avg_grits_trm_composite"], "grits_trm_composite"),
    ("chart", ["avg_rule_pass_rate"], "rule_pass_rate"),
    ("text_content", ["avg_content_faithfulness", "avg_rule_pass_rate"], "content_faithfulness"),
    ("text_formatting", ["avg_semantic_formatting", "avg_rule_pass_rate"], "semantic_formatting"),
    ("layout", ["avg_layout_element_rule_pass_rate", "avg_rule_pass_rate"], "layout_element_rule_pass_rate"),
]


def report(out, grp):
    f = Path(out) / PIPE / grp / "_evaluation_report.json"
    return json.loads(f.read_text()) if f.exists() else None


def per_example(d, metric):
    res = {}
    for r in d["per_example_results"]:
        vals = {m["metric_name"]: m["value"] for m in (r.get("metrics") or [])}
        res[r["example_id"]] = vals.get(metric, vals.get("rule_pass_rate"))
    return res


before, after = sys.argv[1], sys.argv[2]
print(f"{'category':<16} {'before':>8} {'after':>8} {'delta':>8}   docs  failed(b/a)")
overall = {"b": [], "a": []}
changes = []
for grp, keys, metric in CATS:
    db, da = report(before, grp), report(after, grp)
    if not db or not da:
        print(f"{grp:<16} missing")
        continue
    mb = next(db["aggregate_metrics"][k] for k in keys if k in db["aggregate_metrics"])
    ma = next(da["aggregate_metrics"][k] for k in keys if k in da["aggregate_metrics"])
    overall["b"].append(mb)
    overall["a"].append(ma)
    print(
        f"{grp:<16} {mb:8.4f} {ma:8.4f} {ma - mb:+8.4f}   {db['total_examples']:4}  "
        f"{db['failed']}/{da['failed']}"
    )
    pb, pa = per_example(db, metric), per_example(da, metric)
    for ex in sorted(set(pb) | set(pa)):
        vb, va = pb.get(ex), pa.get(ex)
        if vb is None or va is None:
            if vb != va:
                changes.append((grp, ex, vb, va, None))
            continue
        if abs(va - vb) > 1e-9:
            changes.append((grp, ex, vb, va, va - vb))
mb = sum(overall["b"]) / len(overall["b"])
ma = sum(overall["a"]) / len(overall["a"])
print(f"{'overall (mean)':<16} {mb:8.4f} {ma:8.4f} {ma - mb:+8.4f}")
print(f"\n{len(changes)} examples changed:")
for grp, ex, vb, va, dlt in sorted(changes, key=lambda c: (c[4] is not None, c[4] or 0)):
    fmt = lambda v: "None" if v is None else f"{v:.4f}"
    print(f"  {grp:<15} {fmt(vb)} -> {fmt(va)}  {'' if dlt is None else f'{dlt:+.4f}'}  {ex}")
