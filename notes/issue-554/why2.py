"""Print every metric and its details for one page, for several runs.

Usage: why2.py <scratch dir> <page id> <run> [<run> ...]
"""
import json, sys
from pathlib import Path
S = Path(sys.argv[1]); page = sys.argv[2]
for run in sys.argv[3:]:
    for rep in (S / "bench" / run).glob(f"parsebench-2/{page.split('/')[0]}/liteparse_markdown/_evaluation_report.json"):
        for r in json.loads(rep.read_text())["per_example_results"]:
            if r["example_id"] == page:
                print(f"===== {run}")
                for m in r.get("metrics") or []:
                    d = m.get("details") or m.get("metadata") or {}
                    print(f"{m['metric_name']}: {m.get('value')}")
                    if d:
                        print("    " + json.dumps(d)[:1500])
