"""Classify pages where main keeps a grid whole because of a 2-line sliver."""
import json, re, sys
from pathlib import Path
S = Path(sys.argv[1]); B = S / "bench"
def scores(*outs):
    res = {}
    for out in outs:
        for rep in (B / out).glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
            for r in json.loads(rep.read_text())["per_example_results"]:
                m = {x["metric_name"]: x["value"] for x in (r.get("metrics") or [])}
                res[r["example_id"]] = m.get("table_next", m.get("grits_trm_composite")) or 0.0
    return res
main = scores("v2_main", "v2_main_tables"); v10 = scores("v2_abl_noband"); v11 = scores("v2_v11")
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(S / "gen/overlap.json"))}
rows = []
for line in open(S / "vgap_all.tsv"):
    path, msg = line.rstrip("\n").split("\t")
    e = path[:-4]
    pos = set()
    for m in re.finditer(r"bands=\[(.*?)\]", msg):
        bands = re.findall(r'"(\d+)h y(\d+)-(\d+)"', m.group(1))
        big = [i for i, b in enumerate(bands) if int(b[0]) >= 3]
        for i, b in enumerate(bands):
            if int(b[0]) < 3:
                if not big: pos.add("all-slivers")
                elif i < big[0]: pos.add("above")
                elif i > big[-1]: pos.add("below")
                else: pos.add("between")
    rows.append((e, ",".join(sorted(pos)), main.get(e), v10.get(e), v11.get(e), overlap.get(e, "in-sample" if e.startswith("tables/") else "?")))
from collections import Counter
print(Counter(r[1] for r in rows))
print(f"{'page':58} {'sliver':14} {'main':>6} {'v10':>6} {'v11':>6}")
for r in sorted(rows, key=lambda r: ((r[3] or 0) - (r[2] or 0))):
    f = lambda x: f"{x:6.3f}" if x is not None else "     -"
    print(f"{r[0][:58]:58} {r[1]:14} {f(r[2])} {f(r[3])} {f(r[4])}  {r[5]}")
