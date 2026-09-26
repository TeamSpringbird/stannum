#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Probe a TIN (or Stannum) server for the behaviors recorded in
docs/tin-behavior.md, so the comparison can be repeated.

    PGHOST=... PGDATABASE=... benchmarks/tin_behavior_probe.py [--engine tin] [--crash-probe]

The connection comes from the environment only, like the other benchmark
tools: the libpq variables (PGHOST, PGPORT, PGUSER, PGDATABASE, PGPASSWORD or
~/.pgpass, PGSERVICE), or a full connection string in TIN_DSN, which takes
precedence. Never pass it on the command line or commit it. Objects are
created in a scratch schema,
`stannum_probe`, which is dropped at the end. `--crash-probe` sends queries
that crashed the whole TIN server (every backend restarted) on 2026-09-26;
it stops at the first dropped connection and reports whether an idle
sentinel connection died too. Run it only against a server you may restart.
"""
import argparse
import json
import os
import time

import psycopg

DOCS = ["alpha x beta gamma", "alpha beta gamma", "beta gamma alpha", "alpha x y beta gamma"]
SPAN_QUERIES = ['alpha THEN/1 "beta gamma"', 'alpha THEN/2 "beta gamma"', '"alpha x" THEN/2 "beta gamma"',
                'alpha THEN/1 beta', '"beta gamma" THEN/1 alpha', 'alpha NEAR/1 "beta gamma"']


def connect(dsn):
    connection = psycopg.connect(dsn, autocommit=True, connect_timeout=20)
    connection.execute("SET search_path = stannum_probe, public")
    connection.execute("SET statement_timeout = '60s'")
    return connection


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--engine', default='tin', choices=['tin', 'stannum'])
    parser.add_argument('--crash-probe', action='store_true')
    args = parser.parse_args()
    # An empty connection string leaves every setting to the libpq environment.
    dsn = os.environ.get('TIN_DSN', '')
    engine = args.engine
    out = {}
    connection = connect(dsn)
    connection.execute(f"CREATE EXTENSION IF NOT EXISTS {engine}")
    connection.execute("DROP SCHEMA IF EXISTS stannum_probe CASCADE")
    connection.execute("CREATE SCHEMA stannum_probe")
    try:
        connection.execute("CREATE TABLE docs(id int PRIMARY KEY, body text)")
        for i, doc in enumerate(DOCS, 1):
            connection.execute("INSERT INTO docs VALUES (%s, %s)", (i, doc))
        connection.execute(f"CREATE INDEX docs_idx ON docs USING {engine}(body)")
        spans = {}
        for query in SPAN_QUERIES:
            ids = connection.execute("SELECT coalesce(array_agg(id ORDER BY id), '{}') FROM docs WHERE body ==> %s",
                                     (query,)).fetchone()[0]
            ranked = connection.execute(
                f"SELECT coalesce(array_agg(id ORDER BY s DESC, id), '{{}}') FROM (SELECT id, {engine}.full_score(ctid) s "
                "FROM docs WHERE body ==> %s ORDER BY s DESC LIMIT 10) x", (query,)).fetchone()[0]
            spans[query] = dict(ids=ids, ranked=ranked)
        out['spans'] = spans

        connection.execute("CREATE TABLE w(id int PRIMARY KEY, body text)")
        connection.execute("INSERT INTO w VALUES (1, 'w w w')")
        for i in range(2, 11):
            connection.execute("INSERT INTO w VALUES (%s, 'w')", (i,))
        connection.execute(f"CREATE INDEX w_idx ON w USING {engine}(body)")
        connection.execute("SET enable_seqscan = off")
        k1s = {}
        for k1 in (0.0, 0.001, 0.01, 1.2):
            rows = connection.execute(
                f"SELECT id, {engine}.full_score(ctid, %s::real, 0.75::real) FROM w WHERE body ==> 'w' ORDER BY id",
                (k1,)).fetchall()
            exhaustive = sorted(rows, key=lambda row: (-row[1], row[0]))[0][0]
            top = connection.execute(
                f"SELECT id FROM w WHERE body ==> 'w' ORDER BY {engine}.full_score(ctid, %s::real, 0.75::real) DESC LIMIT 1",
                (k1,)).fetchone()[0]
            k1s[str(k1)] = dict(exhaustive_top=exhaustive, pruned_top=top, row1=rows[0][1], row2=rows[1][1])
        out['k1'] = k1s

        if args.crash_probe:
            depth = {}
            for family, make, sizes in [('words', lambda n: 'a ' * n, [1000, 3000, 10000]),
                                        ('or_chain', lambda n: ' OR '.join(['a'] * n), [1000, 3000, 10000]),
                                        ('nested', lambda n: '(' * n + 'a' + ')' * n, [100, 1000, 5000])]:
                for n in sizes:
                    sentinel = connect(dsn)
                    try:
                        started = time.time()
                        connection.execute("SELECT count(*) FROM docs WHERE body ==> %s", (make(n),)).fetchone()
                        depth[f'{family}_{n}'] = dict(ok=True, seconds=round(time.time() - started, 2))
                        sentinel.close()
                        continue
                    except psycopg.Error as error:
                        result = dict(ok=False, error=str(error).splitlines()[0][:200])
                    try:
                        sentinel.execute("SELECT 1")
                        result['server_restarted'] = False
                    except psycopg.Error:
                        result['server_restarted'] = True
                    depth[f'{family}_{n}'] = result
                    time.sleep(10)
                    connection = connect(dsn)
                    break
            out['depth'] = depth
    finally:
        connect(dsn).execute("DROP SCHEMA IF EXISTS stannum_probe CASCADE")
    print(json.dumps(out, indent=1, default=str))


if __name__ == '__main__':
    main()
