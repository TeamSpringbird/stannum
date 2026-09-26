#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Per-query pages and disk reads on the local mock.

Starts a container on the saved mock database with the AWS proportions (5g
memory, 2GB shared buffers), drops the VM page cache before each query, and
reports for each query the pages the scan touched (EXPLAIN BUFFERS), the bytes
the container read from disk (cgroup io.stat) and the time. Needs STANNUM_MOCK
(the directory holding db/) and STANNUM_DATASET (the published StackExchange
dataset directory).
"""
import argparse
import time

import mockdb

FIXED = [("OR 5 common", 'the OR is OR to OR and OR you'), ("AND 3 common", 'to AND create AND the'),
         ("phrase 'to create the'", '"to create the"'), ("OR rare-ish", 'ojil OR sure OR that OR is OR possible')]
SQL = ('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id, body, stannum.score(ctid) AS score '
       'FROM documents WHERE body ==> %s ORDER BY score DESC LIMIT 10')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('image', help='benchmark image that wrote the saved database')
    parser.add_argument('label', help='prefix for every output line')
    parser.add_argument('published', type=int, nargs='?', default=8,
                        help='published queries to add, three styles each (default 8)')
    parser.add_argument('--port', type=int, default=29580)
    args = parser.parse_args()
    published = mockdb.published_queries()
    queries = FIXED + [(f'{s} #{x["source_id"]}', x['engines']['tin'][s])
                       for x in published[:args.published] for s in ('conjunction', 'disjunction', 'phrase')]
    total = dict(mb=0.0, ms=0.0, pages=0, scored=0)
    with mockdb.Server(args.image, 'mock15m-probe', args.port) as server, server.connect() as c:
        c.execute('SET enable_seqscan=off')
        c.execute('SET statement_timeout=300000')
        for label, text in queries:
            mockdb.drop_caches()
            r0 = server.read_bytes()
            t = time.perf_counter()
            try:
                plan = c.execute(SQL, (text,)).fetchone()[0][0]
                top = plan['Plan']
                node = mockdb.scan_node(top) or {}
                pages = top.get('Shared Hit Blocks', 0) + top.get('Shared Read Blocks', 0)
                scored = node.get('Scored Candidates') or node.get('Exhaustive Score Calls') or 0
            except Exception as error:
                pages = scored = 0
                print('  error', repr(error)[:80])
            ms = (time.perf_counter() - t) * 1000
            mb = (server.read_bytes() - r0) / 2**20
            total['mb'] += mb
            total['ms'] += ms
            total['pages'] += pages
            total['scored'] += scored
            print(f'{args.label} {label:26} pages {pages:7d} scored {scored:7d} read {mb:8.1f} MB {ms:8.0f} ms', flush=True)
    print(f"{args.label} TOTAL {len(queries)} queries: pages {total['pages']} scored {total['scored']} "
          f"read {total['mb']:.0f} MB {total['ms']:.0f} ms")


if __name__ == '__main__':
    main()
