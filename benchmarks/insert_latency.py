#!/usr/bin/env python3
"""Sustained-insert latency of a Stannum index, one batch statement at a time.

Inserts short documents in batches through one psql session and records how
long each INSERT statement took on the server, so the cost of write-buffer
folds and segment merges triggered by an unlucky insert is visible as a
per-batch spike. Connection details come from the usual libpq environment
variables (PGHOST, PGPORT, PGUSER); the database is created and, unless
`--keep` is given, dropped afterwards.

    PGPORT=28818 python3 benchmarks/insert_latency.py \\
        --docs 300000 --batch 1000 --write-buffer-docs 1024 --max-segments 32 \\
        --output benchmarks/results/insert-latency-after.json

Only GUCs passed on the command line are set, so the script works against
builds that predate a tunable.
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from datetime import datetime


def psql(args, sql, dbname):
    return subprocess.run(
        ["psql", "-X", "-qAt", "-v", "ON_ERROR_STOP=1", "-d", dbname, *args],
        input=sql,
        text=True,
        capture_output=True,
        check=True,
    ).stdout


def batches(docs, batch):
    start = 1
    while start <= docs:
        end = min(start + batch - 1, docs)
        yield start, end
        start = end + 1


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dbname", default="stannum_merge_bench")
    parser.add_argument("--docs", type=int, default=300_000)
    parser.add_argument("--batch", type=int, default=1_000)
    parser.add_argument("--write-buffer-docs", type=int)
    parser.add_argument("--max-segments", type=int)
    parser.add_argument("--merge-tier-factor", type=int)
    parser.add_argument("--label", default="")
    parser.add_argument("--output", help="JSON file for per-batch latencies and the summary")
    parser.add_argument("--keep", action="store_true", help="leave the database in place")
    args = parser.parse_args()

    maintenance = "postgres"
    subprocess.run(
        ["psql", "-X", "-q", "-v", "ON_ERROR_STOP=1", "-d", maintenance, "-c",
         f'DROP DATABASE IF EXISTS "{args.dbname}"', "-c", f'CREATE DATABASE "{args.dbname}"'],
        check=True, text=True, capture_output=True,
    )
    settings = []
    for name, value in (
        ("stannum.write_buffer_docs", args.write_buffer_docs),
        ("stannum.max_segments", args.max_segments),
        ("stannum.merge_tier_factor", args.merge_tier_factor),
    ):
        if value is not None:
            settings.append(f"SET {name} = {value};")
    script = [
        "CREATE EXTENSION stannum;",
        "CREATE TABLE docs(id int PRIMARY KEY, body text);",
        "CREATE INDEX docs_idx ON docs USING stannum(body);",
        *settings,
    ]
    for start, end in batches(args.docs, args.batch):
        script.append("SELECT clock_timestamp();")
        script.append(
            "INSERT INTO docs SELECT n, 'w' || (n % 97) || ' common term' || (n % 1009) || "
            f"' filler ' || md5(n::text) FROM generate_series({start}, {end}) n;"
        )
        script.append("SELECT clock_timestamp();")
    script.append("SELECT 'segments', count(*) FROM stannum.segment_info('docs_idx') WHERE kind = 'immutable';")
    script.append("SELECT 'docs', sum(docs) FROM stannum.segment_info('docs_idx');")
    script.append("SET enable_seqscan = off; SET enable_bitmapscan = on;")
    script.append("SELECT 'indexed', count(*) FROM docs WHERE body ==> 'common AND w7';")
    script.append("SET enable_seqscan = on; SET enable_bitmapscan = off;")
    script.append("SELECT 'reference', count(*) FROM docs WHERE body ==> 'common AND w7';")

    wall_start = time.monotonic()
    try:
        output = psql([], "\n".join(script), args.dbname)
    finally:
        if not args.keep:
            subprocess.run(
                ["psql", "-X", "-q", "-d", maintenance, "-c", f'DROP DATABASE IF EXISTS "{args.dbname}"'],
                check=False, text=True, capture_output=True,
            )
    wall = time.monotonic() - wall_start

    lines = [line for line in output.splitlines() if line.strip()]
    stamps = []
    tail = {}
    for line in lines:
        if "|" in line:
            key, value = line.split("|", 1)
            tail[key] = int(value)
        else:
            stamps.append(datetime.fromisoformat(line))
    if len(stamps) % 2 or not stamps:
        sys.exit(f"unexpected psql output: {output[-500:]}")
    latencies = [(after - before).total_seconds() for before, after in zip(stamps[::2], stamps[1::2])]
    if tail.get("indexed") != tail.get("reference"):
        sys.exit(f"index and sequential scans disagree: {tail}")

    ordered = sorted(latencies)
    summary = {
        "label": args.label,
        "docs": args.docs,
        "batch": args.batch,
        "batches": len(latencies),
        "settings": {
            "write_buffer_docs": args.write_buffer_docs,
            "max_segments": args.max_segments,
            "merge_tier_factor": args.merge_tier_factor,
        },
        "segments_at_end": tail.get("segments"),
        "wall_seconds": round(wall, 3),
        "insert_seconds": round(sum(latencies), 3),
        "mean_ms": round(statistics.mean(latencies) * 1000, 2),
        "median_ms": round(statistics.median(latencies) * 1000, 2),
        "p99_ms": round(ordered[min(len(ordered) - 1, int(len(ordered) * 0.99))] * 1000, 2),
        "max_ms": round(ordered[-1] * 1000, 2),
        "slowest_batches": [
            {"batch": i + 1, "ms": round(ms * 1000, 1)}
            for i, ms in sorted(enumerate(latencies), key=lambda pair: -pair[1])[:8]
        ],
    }
    print(json.dumps(summary, indent=2))
    if args.output:
        os.makedirs(os.path.dirname(os.path.abspath(args.output)), exist_ok=True)
        with open(args.output, "w") as f:
            json.dump({"summary": summary, "latencies_ms": [round(ms * 1000, 3) for ms in latencies]}, f, indent=1)


if __name__ == "__main__":
    main()
