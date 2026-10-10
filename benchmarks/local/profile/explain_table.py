"""Per-query buffer accesses from the EXPLAIN-sampled runs beside pg_statio.

    explain_table.py RUN_PREFIX   (reads runs/RUN_PREFIX-{score,full_score}-{style}.json)

pg_statio index hits+reads per query = the v2 report's MiB/query basis (block counts over
all queries of the window, EXPLAINed ones included); EXPLAIN buffers = pgBufferUsage of the
whole statement (index + heap, ReadRecentBuffer pins included); Pages Pinned = Stannum's own
count of index pins; per-kind pinned pages and stitched bytes from "Native Reads By Kind".
"""
import collections
import json
import re
import sys

TIN_MB = {"conjunction": (21, 34), "disjunction": (31, 106), "phrase": (28, 37)}  # TIN, TIN_FULL MB/query
V2_MIB = {("score", "conjunction"): 1.3, ("full_score", "conjunction"): 1.8, ("score", "disjunction"): 2.4,
          ("full_score", "disjunction"): 7.0, ("score", "phrase"): 1.8, ("full_score", "phrase"): 2.4}
KIND = re.compile(r"(\w+): copied (\d+) B in (\d+) pages, pinned (\d+), stitched (\d+) B in (\d+) reads")


def main():
    prefix = sys.argv[1]
    print("| run | statio idx blocks/q | = MiB/q | v2 report MiB/q | EXPLAIN buffers/q | Pages Pinned/q | recent % "
          "| real MiB/q | TIN MB/q | footers decoded/q | footer bytes/q | planning buffers/q | held peak |")
    print("| --- |" + " ---: |" * 12)
    kinds_out = {}
    for fn in ("score", "full_score"):
        for style in ("conjunction", "disjunction", "phrase"):
            d = json.load(open(f"{prefix}-{fn}-{style}.json"))
            s = d["summary"]
            ex = [r for r in d["rows"] if r.get("explain")]
            n_all = len(d["rows"])
            statio = (s["idx_blks_hit"] + s["idx_blks_read"]) / n_all
            buf = sum(r["hit"] + r["read"] for r in ex) / len(ex)
            pins = sum(r["pins"] for r in ex) / len(ex)
            recent = sum(r["recent"] for r in ex) / max(1, sum(r["pins"] for r in ex))
            plan_buf = sum(r["planning"].get("Shared Hit Blocks", 0) + r["planning"].get("Shared Read Blocks", 0)
                           for r in ex) / len(ex)
            peak = sum(r["held_peak"] for r in ex) / len(ex)
            fd = sum(r["Footers Decoded"] for r in ex) / len(ex)
            kinds = collections.defaultdict(lambda: [0, 0, 0])
            for r in ex:
                for k, copied, cpages, pinned, stitched, sreads in KIND.findall(r["scan"].get("Native Reads By Kind", "")):
                    kinds[k][0] += int(pinned)
                    kinds[k][1] += int(stitched)
                    kinds[k][2] += int(copied)
            kinds = {k: (v[0] / len(ex), v[1] / len(ex)) for k, v in kinds.items()}
            kinds_out[f"{fn}-{style}"] = kinds
            tin = TIN_MB[style][0 if fn == "score" else 1]
            print(f"| {fn} {style} | {statio:.0f} | {statio * 8 / 1024:.1f} | {V2_MIB[(fn, style)]} | {buf:.0f} | "
                  f"{pins:.0f} | {100 * recent:.1f} | {buf * 8 / 1024:.1f} | {tin} | {fd:.0f} | "
                  f"{kinds.get('footer', (0, 0))[1] / 1e6:.2f} MB | {plan_buf:.0f} | {peak:.0f} |")
    print("\nWalk counters per query (EXPLAIN-sampled queries):\n")
    print("| run | scored candidates | positions checked | visibility checks | planning ms | planning buffers |")
    print("| --- |" + " ---: |" * 5)
    for fn in ("score", "full_score"):
        for style in ("conjunction", "disjunction", "phrase"):
            d = json.load(open(f"{prefix}-{fn}-{style}.json"))
            ex = [r for r in d["rows"] if r.get("explain")]

            def m(k):
                return sum(r["scan"].get(k, 0) or 0 for r in ex) / len(ex)
            pb = sum(r["planning"].get("Shared Hit Blocks", 0) + r["planning"].get("Shared Read Blocks", 0)
                     for r in ex) / len(ex)
            print(f"| {fn} {style} | {m('Scored Candidates'):.0f} | {m('Positions Checked'):.0f} | "
                  f"{m('Visibility Checks'):.0f} | {sum(r['planning_ms'] for r in ex) / len(ex):.2f} | {pb:.0f} |")
    print("\nPinned pages per query by kind (stitched bytes per query in parentheses):\n")
    names = sorted({k for v in kinds_out.values() for k in v})
    print("| run | " + " | ".join(names) + " |")
    print("| --- |" + " ---: |" * len(names))
    for run, kinds in kinds_out.items():
        print(f"| {run} | " + " | ".join(
            f"{kinds.get(k, (0, 0))[0]:.0f} ({kinds.get(k, (0, 0))[1] / 1e3:.0f} kB)" for k in names) + " |")


if __name__ == "__main__":
    main()
