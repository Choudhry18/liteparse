"""Score summary for several ParseBench-2 runs against main. Usage: ablate.py <scratch> name=dir1,dir2 ..."""
import json, random, sys
from pathlib import Path
S = Path(sys.argv[1]); B = S / "bench"
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(S / "gen/overlap.json"))}
HELD = ["complex_table_headers", "financial_tables", "insurance_filing_tables", "dense_tables"]
def scores(dirs):
    res = {}
    for d in dirs.split(","):
        for rep in (B / d).glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
            for r in json.loads(rep.read_text())["per_example_results"]:
                m = {x["metric_name"]: x["value"] for x in (r.get("metrics") or [])}
                res[r["example_id"]] = m.get("table_next", m.get("grits_trm_composite")) or 0.0
    return res
mean = lambda xs: sum(xs) / len(xs)
main = scores("v2_main,v2_main_tables")
held = sorted(e for e in main if e.split("/")[0] in HELD and overlap.get(e) == "new")
ins = sorted(e for e in main if e.startswith("tables/"))
print(f"{'run':14} {'held eq-wt':>10} {'per-page':>9} {'95% CI':>18} {'up':>4} {'down':>5} {'in-sample':>10}")
random.seed(0)
for spec in sys.argv[2:]:
    name, dirs = spec.split("=")
    s = main if name == "main" else scores(dirs)
    eq = mean([mean([s.get(e, 0) for e in held if e.startswith(x + "/")]) for x in HELD])
    d = [s.get(e, 0) - main.get(e, 0) for e in held]
    b = sorted(mean([random.choice(d) for _ in d]) for _ in range(1000))
    print(f"{name:14} {eq:10.4f} {mean(d):+9.4f} [{b[25]:+.4f},{b[975]:+.4f}] {sum(x>1e-9 for x in d):4} {sum(x<-1e-9 for x in d):5} {mean([s.get(e,0) for e in ins]):10.4f}")
