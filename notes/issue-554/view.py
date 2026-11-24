"""Condensed view of tables: view.py <markdown file or ->. Prints each table row as cells joined by ' | ', colspan as [n]."""
import re, sys, html
md = open(sys.argv[1]).read() if sys.argv[1] != "-" else sys.stdin.read()
md = re.sub(r"\s+", " ", md)
for i, t in enumerate(re.findall(r"<table>(.*?)</table>", md)):
    print(f"  <table {i+1}>")
    for tr in re.findall(r"<tr>(.*?)</tr>", t):
        cells = re.findall(r'<t([hd])(?: [^>]*?colspan="(\d+)")?[^>]*>(.*?)</t[hd]>', tr)
        print("   " + " | ".join(f"{'[' + cs + ']' if cs else ''}{'*' if tag == 'h' else ''}{html.unescape(re.sub('<[^>]+>', ' ', txt)).strip()[:28]}" for tag, cs, txt in cells))
