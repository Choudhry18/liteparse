"""Compare main and v17 on the non-table ParseBench-2 subsets, metric by metric."""
import json, random, sys
from collections import defaultdict
from pathlib import Path
S = Path(sys.argv[1]); B = S / "bench"
def load(run):
    res = defaultdict(dict)  # (subset, example) -> metric -> value
    for rep in (B / run).glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
        sub = rep.parent.parent.name
        for r in json.loads(rep.read_text())["per_example_results"]:
            for m in r.get("metrics") or []:
                if isinstance(m.get("value"), (int, float)):
                    res[(sub, r["example_id"])][m["metric_name"]] = m["value"]
    return res
a, b = load("v2_other_main"), load("v2_other_v17")
by_sub = defaultdict(lambda: defaultdict(list))
for k in a:
    if k not in b:
        continue
    for name, v in a[k].items():
        if name in b[k]:
            by_sub[k[0]][name].append((b[k][name] - v, k[1], v, b[k][name]))
random.seed(0)
for sub in sorted(by_sub):
    print(f"\n== {sub}")
    for name, d in sorted(by_sub[sub].items()):
        diffs = [x[0] for x in d]
        n_up = sum(x > 1e-9 for x in diffs); n_dn = sum(x < -1e-9 for x in diffs)
        if not (n_up or n_dn):
            print(f"   {name:28} no change on {len(d)} pages"); continue
        mean = sum(diffs) / len(diffs)
        boots = sorted(sum(random.choice(diffs) for _ in diffs) / len(diffs) for _ in range(2000))
        print(f"   {name:28} {mean:+.4f} [{boots[50]:+.4f}, {boots[1950]:+.4f}]  better {n_up} worse {n_dn} of {len(d)}")
        for x in sorted(d)[:4]:
            if x[0] < -1e-9:
                print(f"        {x[0]:+.3f}  {x[2]:.3f} -> {x[3]:.3f}  {x[1]}")
