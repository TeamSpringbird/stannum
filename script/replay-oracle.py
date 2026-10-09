#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Turn a published query trace into the offline replay's trace, and answer it
from PostgreSQL in the replay's result format, so the two can be diffed.

    script/replay-oracle.py trace --queries datasets/stackexchange/queries.json --out trace.tsv
    $STANNUM_PYTHON script/replay-oracle.py postgres --dbname db --trace trace.tsv --out pg.tsv
    cargo run -p bench --release --bin replay -- --dump DIR --trace trace.tsv --expect pg.tsv

`trace` writes `name<TAB>style<TAB>query` per query and style (conjunction,
disjunction, phrase), named `<source_id>:<style>` as the benchmark names
them, from each record's TIN form, which is the one the benchmark sends to
Stannum. `postgres` runs the benchmark's two statements per query,

    SELECT id, stannum.score(ctid) AS score FROM <table> WHERE body ==> $1
        ORDER BY score DESC LIMIT <k>
    SELECT count(*) FROM <table> WHERE body ==> $1

and writes each ranked answer as `id:<score bits>` in order and each count,
as the replay does (`--ranked-only` leaves the counts out). It needs psycopg 3 (STANNUM_PYTHON) and libpq settings
naming the database; the extension's release build should be installed and
the table's index built, with no write buffer or VACUUM since the dump.
"""
import argparse
import json
import struct
import sys
from pathlib import Path

STYLES = ("conjunction", "disjunction", "phrase")


def write_trace(queries, out):
    records = json.loads(Path(queries).read_text())["queries"]
    lines = []
    for record in records:
        for style in STYLES:
            text = record["engines"]["tin"][style]
            if "\t" in text or "\n" in text:
                raise SystemExit(f"query {record['source_id']}: a tab or newline in {text!r}")
            lines.append(f"{record['source_id']}:{style}\t{style}\t{text}")
    Path(out).write_text("\n".join(lines) + "\n")
    print(f"{len(lines)} queries from {len(records)} records")


def bits(score):
    return struct.unpack("<I", struct.pack("<f", score))[0]


def answer(args):
    import psycopg

    trace = [line.split("\t", 2) for line in Path(args.trace).read_text().splitlines() if line]
    ranked_sql = (f"SELECT {args.id_column}, stannum.score(ctid) AS score FROM {args.table} "
                  f"WHERE {args.column} ==> %s ORDER BY score DESC LIMIT {args.k}")
    count_sql = f"SELECT count(*) FROM {args.table} WHERE {args.column} ==> %s"
    out = []
    # No server-side prepared statements: each query is planned with its
    # text, as the benchmark's driver plans it.
    with psycopg.connect(args.dbname or "", prepare_threshold=None, autocommit=True) as connection:
        connection.execute("SET extra_float_digits = 3")
        for n, (name, style, text) in enumerate(trace):
            rows = connection.execute(ranked_sql, (text,)).fetchall()
            out.append(f"{name}\t{style}\tranked\t" + ",".join(f"{row[0]}:{bits(row[1]):08x}" for row in rows))
            if not args.ranked_only:
                count, = connection.execute(count_sql, (text,)).fetchone()
                out.append(f"{name}\t{style}\tcount\t{count}")
            if (n + 1) % 500 == 0:
                print(f"{n + 1} of {len(trace)}", file=sys.stderr)
    Path(args.out).write_text("\n".join(out) + "\n")
    print(f"{len(trace)} queries answered")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    commands = parser.add_subparsers(dest="command", required=True)
    trace = commands.add_parser("trace", help="queries.json to trace.tsv")
    trace.add_argument("--queries", required=True)
    trace.add_argument("--out", required=True)
    pg = commands.add_parser("postgres", help="answer a trace from PostgreSQL")
    pg.add_argument("--dbname", help="a libpq connection string or database name")
    pg.add_argument("--trace", required=True)
    pg.add_argument("--out", required=True)
    pg.add_argument("--table", default="documents")
    pg.add_argument("--column", default="body")
    pg.add_argument("--id-column", default="id")
    pg.add_argument("--k", type=int, default=10)
    pg.add_argument("--ranked-only", action="store_true",
                    help="skip the counts (slow on a large table)")
    args = parser.parse_args()
    if args.command == "trace":
        write_trace(args.queries, args.out)
    else:
        answer(args)


if __name__ == "__main__":
    main()
