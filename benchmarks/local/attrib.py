#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Attribute every page a ranked query reads from storage.

Per query, three independent views of the same reads: pg_statio deltas by
relation (index, heap, toast, toast index, everything else), the EXPLAIN node
counters (top node and scan node), and the extension's own phase and area
counters. Warm up on one slice of the published queries, then measure on a
disjoint slice, so the steady state is what is reported. Needs STANNUM_MOCK
(the directory holding the saved database, db/) and STANNUM_DATASET (the
published StackExchange dataset directory).
"""
import argparse
import sys
import time

import mockdb

SQL = ('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id, body, stannum.score(ctid) s '
       'FROM documents WHERE body ==> %s ORDER BY s DESC LIMIT 10')
STATIO = '''select coalesce(sum(heap_blks_read),0), coalesce(sum(idx_blks_read),0), coalesce(sum(toast_blks_read),0), coalesce(sum(tidx_blks_read),0)
          from pg_statio_all_tables where relname = 'documents' '''
STATIO_ALL = '''select coalesce(sum(heap_blks_read+idx_blks_read+coalesce(toast_blks_read,0)+coalesce(tidx_blks_read,0)),0) from pg_statio_all_tables'''


def statio(c):
    c.execute('select pg_stat_force_next_flush()')
    c.execute('select pg_stat_clear_snapshot()')
    h, i, t, ti = c.execute(STATIO).fetchone()
    a = c.execute(STATIO_ALL).fetchone()[0]
    return dict(heap=int(h), index=int(i), toast=int(t), tidx=int(ti), other=int(a) - int(h + i + t + ti))


def parse(field):
    out = {}
    for part in (field or '').split(', '):
        if ' ' in part:
            k, v = part.rsplit(' ', 1)
            out[k] = int(v)
    return out


def shape(p, d=0):
    print('  ' * d + p.get('Node Type', '?'), p.get('Custom Plan Provider', ''), 'read', p.get('Shared Read Blocks'),
          'hit', p.get('Shared Hit Blocks'), 'time', p.get('Actual Total Time'))
    for ch in p.get('Plans', []):
        shape(ch, d + 1)


def measure(c, phase, forms, show_plan):
    acc = {}
    n = 0
    per_style = {}

    def add(k, v):
        acc[k] = acc.get(k, 0) + v
    for style, text in forms:
        s0 = statio(c)
        t = time.perf_counter()
        whole = c.execute(SQL, (text,)).fetchone()[0][0]
        ms = (time.perf_counter() - t) * 1000
        s1 = statio(c)
        n += 1
        top = whole['Plan']
        node = mockdb.scan_node(top) or {}
        if show_plan:
            shape(top)
            print('  execution ms', whole.get('Execution Time'), 'planning read',
                  (whole.get('Planning') or {}).get('Shared Read Blocks'))
            show_plan = False
        for k in ('heap', 'index', 'toast', 'tidx', 'other'):
            add('statio ' + k, s1[k] - s0[k])
        add('planning read', (whole.get('Planning') or {}).get('Shared Read Blocks', 0))
        add('top node read', top.get('Shared Read Blocks', 0))
        add('scan node read', node.get('Shared Read Blocks', 0))
        add('scan node hit', node.get('Shared Hit Blocks', 0))
        add('exec ms', whole.get('Execution Time', 0))
        add('top node ms', top.get('Actual Total Time', 0))
        add('wall ms', ms)
        add('visibility checks', node.get('Visibility Checks', 0))
        add('heap fetches', node.get('Heap Fetches', 0))
        for k, v in parse(node.get('Disk Pages By Area')).items():
            add('area ' + k, v)
        for k, v in parse(node.get('Disk Pages By Phase')).items():
            add('phase ' + k, v)
        st = per_style.setdefault(style, {})

        def sadd(k, v):
            st[k] = st.get(k, 0) + v
        sadd('n', 1)
        sadd('statio index', s1['index'] - s0['index'])
        sadd('statio heap', s1['heap'] - s0['heap'])
        sadd('scan node read', node.get('Shared Read Blocks', 0))
        sadd('exec ms', whole.get('Execution Time', 0))
        sadd('visibility checks', node.get('Visibility Checks', 0))
        sadd('scan node hit', node.get('Shared Hit Blocks', 0))
        for k, v in parse(node.get('Disk Pages By Area')).items():
            sadd('area ' + k, v)
        for k, v in parse(node.get('Disk Pages By Phase')).items():
            sadd('phase ' + k, v)
    print(f'== {phase}: {n} queries')
    for k in sorted(acc):
        print(f'   {k:28} {acc[k]:12.0f}  {acc[k] / max(n, 1):9.1f}/query')
    for s, st in per_style.items():
        count = st.pop('n')
        print(f'   -- style {s} ({count} queries), per query:')
        for k in sorted(st):
            print(f'      {k:28} {st[k] / count:9.1f}')
    sys.stdout.flush()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('image', help='benchmark image that wrote the saved database')
    parser.add_argument('warm', type=int, nargs='?', default=240, help='warm-up query forms (default 240)')
    parser.add_argument('steady', type=int, nargs='?', default=360, help='measured query forms (default 360)')
    parser.add_argument('--port', type=int, default=29588)
    args = parser.parse_args()
    published = mockdb.published_queries()
    forms = [(s, x['engines']['tin'][s]) for x in published[:200] for s in ('conjunction', 'disjunction', 'phrase')]
    with mockdb.Server(args.image, 'attrib', args.port, settings=['autovacuum=off']) as server:
        mockdb.drop_caches()
        with server.connect() as c:
            c.execute('SET enable_seqscan=off')
            c.execute('SET statement_timeout=300000')
            measure(c, 'warm-up', forms[:args.warm], show_plan=True)
            measure(c, 'steady', forms[args.warm:args.warm + args.steady], show_plan=False)


if __name__ == '__main__':
    main()
