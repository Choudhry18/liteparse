"""Print the table_next events of one page for several runs."""
import json, sys
from pathlib import Path
S = Path(sys.argv[1]); page = sys.argv[2]
for run in sys.argv[3:]:
    for rep in (S / "bench" / run).glob(f"parsebench-2/{page.split('/')[0]}/liteparse_markdown/_evaluation_report.json"):
        for r in json.loads(rep.read_text())["per_example_results"]:
            if r["example_id"] == page:
                for m in r.get("metrics") or []:
                    if m["metric_name"] == "table_next":
                        d = m.get("details") or m.get("metadata") or {}
                        print(f"{run}: {m['value']:.3f}")
                        for p in d.get("policy_inputs", []):
                            print(f"     {p.get('severity'):8} x{p.get('count')}  {p.get('label')}")
