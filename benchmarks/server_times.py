#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Server-side execution time per query shape, from EXPLAIN ANALYZE.

The throughput harness (`run.py`) measures what a client sees, which on a
remote server is mostly the network. This probe asks the server how long each
statement took to execute, so a Stannum build on one machine can be set beside
TIN on PlanetScale shape by shape. Hardware and settings still differ between
the two sides; the numbers show structure, not a ranking.

Every query of the harness's mixed profile runs in one session, so the figures
are warm: the first `--discard` executions of each shape are dropped and the
median of the rest is reported. Connection details come from the usual libpq
environment variables; the `documents` table and its index must already exist,
as `run.py` leaves them.

    PGHOST=... PGDATABASE=... python3 benchmarks/server_times.py \\
        --engine stannum --dataset "$DATASETS/wikipedia-100000" \\
        --output benchmarks/results/server-times-stannum-01
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import CASES, ENGINES, workload  # noqa: E402


def load_cases(dataset):
    if not dataset:
        return CASES
    manifest = json.loads((Path(dataset) / "manifest.json").read_text())
    return [tuple(case) for case in manifest["cases"]]


def explain_all(engine, queries, repetitions, disable_seqscan, env, setup=(), interleave=False):
    """Runs every query `repetitions` times in one session; returns the plans."""
    load = {"stannum": "DO $$ BEGIN PERFORM stannum.tokenize('load'); END $$;",
            "tin": "DO $$ BEGIN PERFORM tin.tokenize('load'); END $$;"}.get(engine, "")
    script = [load]
    if disable_seqscan:
        script.append("SET enable_seqscan = off;")
    script.extend(statement.rstrip(';') + ';' for statement in setup)
    order = measurement_order(len(queries), repetitions, interleave)
    for i, _ in order:
        sql = queries[i][1]
        script.append(f"EXPLAIN (ANALYZE, FORMAT JSON) {sql.rstrip(';')};")
    result = subprocess.run(["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1"],
                            input="\n".join(script), env=env, text=True, capture_output=True)
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    plans = parse_plans(result.stdout)
    expected = len(queries) * repetitions
    if len(plans) != expected:
        raise RuntimeError(f"expected {expected} plans, parsed {len(plans)}")
    # Preserve the existing grouped report contract even when execution alternates.
    return [plan for _, plan in sorted(zip(order, plans))]


def measurement_order(count, repetitions, interleave):
    if interleave:
        return [(i, repeat) for repeat in range(repetitions)
                for i in (range(count) if repeat % 2 == 0 else reversed(range(count)))]
    return [(i, repeat) for i in range(count) for repeat in range(repetitions)]


def parse_plans(text):
    """psql -At prints each JSON plan over several lines; split at top level."""
    plans, current = [], []
    for line in text.splitlines():
        if line.startswith("[") and current:
            plans.append(json.loads("\n".join(current)))
            current = []
        current.append(line)
    if current:
        plans.append(json.loads("\n".join(current)))
    return plans


def top_node(plan):
    node = plan[0]["Plan"]
    through = ("Limit", "Aggregate", "Sort", "Gather", "Result", "Nested Loop")
    while node.get("Node Type") in through and node.get("Plans"):
        node = node["Plans"][0]
    provider = node.get("Custom Plan Provider")
    return provider or node.get("Node Type")


def summarize(queries, plans, repetitions, discard):
    rows = []
    for i, (name, sql) in enumerate(queries):
        mine = plans[i * repetitions:(i + 1) * repetitions]
        times = [p[0]["Execution Time"] for p in mine][discard:]
        rows.append({
            "query": name,
            "sql": sql,
            "median_ms": statistics.median(times),
            "min_ms": min(times),
            "max_ms": max(times),
            "samples": len(times),
            "node": top_node(mine[-1]),
        })
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--engine", choices=ENGINES, required=True)
    parser.add_argument("--dataset", help="Verified dataset directory; its query cases are used")
    parser.add_argument("--profile", choices=("count", "ranked", "mixed"), default="mixed")
    parser.add_argument("--repetitions", type=int, default=7)
    parser.add_argument("--discard", type=int, default=2, help="Leading executions to drop per shape")
    parser.add_argument("--disable-seqscan", action="store_true",
                        help="SET enable_seqscan = off, so a broad term cannot fall back to the heap")
    parser.add_argument("--sql-cases", help="JSON with setup SQL list and named [name, SQL] queries")
    parser.add_argument("--interleave", action="store_true",
                        help="Alternate all cases in forward/reverse order each repetition")
    parser.add_argument("--output", required=True)
    parser.add_argument("--label", default="")
    args = parser.parse_args()
    if args.discard >= args.repetitions:
        parser.error("--discard must leave at least one execution")
    out = Path(args.output)
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    setup = []
    if args.sql_cases:
        cases = json.loads(Path(args.sql_cases).read_text())
        queries, setup = cases["queries"], cases.get("setup", [])
        if not queries or not all(isinstance(q, list) and len(q) == 2
                                  and all(isinstance(v, str) for v in q) for q in queries):
            parser.error("SQL cases must contain nonempty [name, SQL] pairs")
        if not isinstance(setup, list) or not all(isinstance(s, str) for s in setup):
            parser.error("SQL setup must be a list of statements")
    else:
        queries = workload(args.engine, args.profile, load_cases(args.dataset))
    started = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    plans = explain_all(args.engine, queries, args.repetitions, args.disable_seqscan, env, setup, args.interleave)
    rows = summarize(queries, plans, args.repetitions, args.discard)
    report = {
        "engine": args.engine, "label": args.label, "profile": args.profile,
        "setup": setup, "interleave": args.interleave,
        "measurement_order": measurement_order(len(queries), args.repetitions, args.interleave),
        "dataset": args.dataset, "repetitions": args.repetitions, "discard": args.discard,
        "disable_seqscan": args.disable_seqscan, "host": env.get("PGHOST", ""),
        "database": env.get("PGDATABASE", ""), "started": started, "queries": rows,
    }
    (out / "server-times.json").write_text(json.dumps(report, indent=2) + "\n")
    (out / "plans.json").write_text(json.dumps(plans) + "\n")
    width = max(len(r["query"]) for r in rows)
    print(f"{'query':{width}}  median ms   min     max  node")
    for r in rows:
        print(f"{r['query']:{width}}  {r['median_ms']:9.3f} {r['min_ms']:7.3f} {r['max_ms']:7.3f}  {r['node']}")
    print(f"report at {out / 'server-times.json'}")


if __name__ == "__main__":
    main()
