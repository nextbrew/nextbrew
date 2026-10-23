"""Summarise bench/clients.sh runs into the published tables.

    bench/report.py --machine TEXT --out results/DATE --site site/data/benchmarks.json DIR...

--version TOOL=VERSION replaces the version a client reported, for a build
whose version string lags its code.

Each time is the median over the rounds (DIRs) where that install succeeded;
failed runs are listed under the tables.
Totals only count formulae that every client installed at least once.
"""
import argparse
import json
import statistics
from collections import defaultdict
from pathlib import Path

TOOLS = {"nb": "nextbrew", "brew": "brew", "zb": "zerobrew", "nanobrew": "nanobrew", "wax": "wax"}
PHASES = ("cold", "warm")

ap = argparse.ArgumentParser()
ap.add_argument("--machine", required=True)
ap.add_argument("--out", required=True, type=Path)
ap.add_argument("--site", required=True, type=Path)
ap.add_argument("--version", action="append", default=[], metavar="TOOL=VERSION")
ap.add_argument("rounds", nargs="+", type=Path)
args = ap.parse_args()

runs = defaultdict(list)  # (formula, tool, phase) -> [ms]
kegs = {}  # (formula, tool) -> kegs in the Cellar after the cold install
failed = defaultdict(list)
order = []
for d in args.rounds:
    for line in (d / "results.tsv").read_text().splitlines():
        formula, tool, phase, ms, rc, n = line.split("\t")
        if formula not in order:
            order.append(formula)
        if rc == "0":
            runs[formula, tool, phase].append(int(ms))
            kegs.setdefault((formula, tool), int(n))
        else:
            failed[formula, tool, phase].append(rc)
env = dict(line.split("\t", 1) for line in (args.rounds[0] / "env.tsv").read_text().splitlines())

med = {k: statistics.median(v) / 1000 for k, v in runs.items()}
complete = [f for f in order if all((f, t, p) in med for t in TOOLS for p in PHASES)]
total = {p: {TOOLS[t]: round(sum(med[f, t, p] for f in complete), 3) for t in TOOLS} for p in PHASES}
versions = {t: env[t].split()[-1] for t in TOOLS} | dict(v.split("=", 1) for v in args.version)
measured = env["date"][:10]


def cell(s):
    if s is None:
        return "failed"
    return f"{s:.2f}s" if s >= 1 else f"{s * 1000:.0f}ms"


md = [
    f"# Install benchmark, {measured}",
    "",
    f"{args.machine}. {', '.join(f'{TOOLS[t]} {versions[t]}' for t in TOOLS)}. "
    f"Median of {len(args.rounds)} round(s); see bench/clients.sh for the method.",
    "",
]
for p in PHASES:
    md += [f"## {p.capitalize()}", "", "| Formula | Kegs | " + " | ".join(TOOLS.values()) + " |",
           "|---|---:|" + "---:|" * len(TOOLS)]
    md.append(f"| **Total ({len(complete)} formulae)** | | " + " | ".join(
        f"**{cell(total[p][n])}**" + ("" if n == "brew" or not total[p][n] else f" ({total[p]['brew'] / total[p][n]:.1f}x)")
        for n in TOOLS.values()) + " |")
    for f in order:
        md.append(f"| {f} | {kegs.get((f, 'brew'), '')} | " + " | ".join(cell(med.get((f, t, p))) for t in TOOLS) + " |")
    md.append("")
odd = [f"{f}: {TOOLS[t]} {kegs[f, t]}" for f in order for t in TOOLS
       if (f, t) in kegs and (f, "brew") in kegs and kegs[f, t] != kegs[f, "brew"]]
if odd:
    md += ["Kegs installed where a client differs from brew: " + ", ".join(odd) + ".", ""]
if failed:
    md += ["Failed installs: " + ", ".join(f"{f} {TOOLS[t]} {p} ({'/'.join(s)})" for (f, t, p), s in failed.items()) + ".", ""]

args.out.mkdir(parents=True, exist_ok=True)
(args.out / "benchmark.md").write_text("\n".join(md))
(args.out / "benchmark.json").write_text(json.dumps({
    "environment": env | {"machine": args.machine, "rounds": len(args.rounds)},
    "results": [{"formula": f, "kegs": {TOOLS[t]: kegs.get((f, t)) for t in TOOLS},
                 **{p: {TOOLS[t]: runs.get((f, t, p), []) for t in TOOLS} for p in PHASES}} for f in order],
}, indent=2) + "\n")
args.site.write_text(json.dumps({
    "measured": measured,
    "machine": args.machine,
    "rounds": len(args.rounds),
    "tools": [{"id": TOOLS[t], "version": versions[t]} for t in TOOLS],
    "total": {"formulae": len(complete), **total},
    "results": [{"formula": f, "deps": kegs[f, "brew"] - 1,
                 **{p: {TOOLS[t]: med.get((f, t, p)) for t in TOOLS} for p in PHASES}}
                for f in order if (f, "brew") in kegs],
}, indent=2) + "\n")
print("\n".join(md))
