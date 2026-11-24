"""For each page where v10 scores below main, summarise what the merges did."""
import html, json, re, subprocess, sys
from pathlib import Path
S = Path(sys.argv[1]); B = S / "bench"; D = B / "ParseBench/data_parsebench-2"
overlap = {f"{r[0]}/{r[1]}": r[2] for r in json.load(open(S / "gen/overlap.json"))}
def scores(*dirs):
    res = {}
    for d in dirs:
        for rep in (B / d).glob("parsebench-2/*/liteparse_markdown/_evaluation_report.json"):
            for r in json.loads(rep.read_text())["per_example_results"]:
                m = {x["metric_name"]: x["value"] for x in (r.get("metrics") or [])}
                res[r["example_id"]] = m.get("table_next", m.get("grits_trm_composite")) or 0.0
    return res
main, v10 = scores("v2_main", "v2_main_tables"), scores("v2_abl_noband")
def md(b, e):
    return subprocess.run([str(S / f"bin/lit-{b}"), "parse", str(D / f"{e}.pdf"), "--format", "markdown",
                           "--no-ocr", "--quiet", "--no-links"], capture_output=True, text=True).stdout
def ntables(m):
    return m.count("<table>") + len(re.findall(r"(?m)^\|.*\|\n\|(?:\s*:?-+:?\s*\|)+\s*$", m))
def spans(m):
    out = []
    for t in re.findall(r"<table>(.*?)</table>", m, re.S):
        trs = re.findall(r"<tr>(.*?)</tr>", t, re.S)
        ncols = max((sum(int(x or 1) for x in re.findall(r'<t[hd](?: colspan="(\d+)")?>', tr)) for tr in trs), default=0)
        for i, tr in enumerate(trs):
            for cs, txt in re.findall(r'<t[hd] colspan="(\d+)">(.*?)</t[hd]>', tr, re.S):
                txt = html.unescape(re.sub(r"<[^>]+>", " ", txt)).strip()
                pos = "top" if i <= 1 else "body"
                out.append((pos, "full" if int(cs) == ncols else "part", len(txt), txt[:34]))
    return out
for subset_filter, title in ((lambda e: overlap.get(e) == "new" and not e.startswith("tables/"), "HELD-OUT"),
                             (lambda e: e.startswith("tables/"), "IN-SAMPLE (tables subset)")):
    print(f"\n===== {title}")
    losses = sorted((v10[e] - main.get(e, 0), e) for e in v10 if subset_filter(e) and v10[e] < main.get(e, 0) - 1e-9)
    for d, e in losses:
        a, b = md("main", e), md("v10", e)
        sp = spans(b)
        top = sum(1 for s in sp if s[0] == "top"); body = len(sp) - top
        full_top = sum(1 for s in sp if s[0] == "top" and s[1] == "full")
        print(f"{d:+.3f} {main.get(e,0):.2f}->{v10[e]:.2f} tables {ntables(a)}->{ntables(b)} spans top={top}(full {full_top}) body={body}  {e}")
        for s in sp[:3]:
            print(f"        {s[0]:4} {s[1]:4} {s[2]:4}ch  {s[3]!r}")
