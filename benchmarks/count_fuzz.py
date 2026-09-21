#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Seeded Boolean and snapshot correctness checks on an owned test schema.

Requires psycopg, libpq settings and the experimental force_count_pages GUC.
Writes fixtures, query ASTs and operations before execution for deterministic
replay. Python token-set evaluation is independent of the extension parser.
"""
import argparse
import collections
import json
import hashlib
from pathlib import Path
import random
import uuid

TERMS = ('alpha', 'beta', 'rare', 'common', 'absent')


def tree(rng, depth):
    if depth == 0 or rng.random() < .3:
        return rng.choice(TERMS)
    return (rng.choice(('AND', 'OR')), tree(rng, depth-1), tree(rng, depth-1))


def render(node):
    if isinstance(node, str):
        return node
    return f'({render(node[1])} {node[0]} {render(node[2])})'


def matches(node, tokens):
    if isinstance(node, str):
        return node in tokens
    a, b = matches(node[1], tokens), matches(node[2], tokens)
    return a and b if node[0] == 'AND' else a or b


def body(rng):
    return ' '.join(['filler'] + [term for term, probability in
                    zip(TERMS[:4], (.05, .2, .01, .8)) if rng.random() < probability])


def fixture(seed, rows, queries, rounds):
    rng = random.Random(seed)
    docs = [(n, body(rng)) for n in range(1, rows+1)]
    asts = list(TERMS) + [('OR', 'rare', 'rare'), ('AND', 'rare', 'absent')]
    asts += [tree(rng, rng.randint(1, 5)) for _ in range(queries)]
    operations = []
    for turn in range(rounds):
        batch = []
        for _ in range(20):
            operation = rng.choice(('delete', 'text', 'payload'))
            batch.append(dict(kind=operation, id=rng.randint(1, rows), body=body(rng)))
        batch.append(dict(kind='insert', id=rows+turn+1, body=body(rng)))
        operations.append(dict(rollback=turn % 3 == 2, operations=batch))
    return dict(seed=seed, documents=docs, queries=asts, mutations=operations,
                buffer_docs=rng.choice((32, 64, 128)))


def snapshot(conn):
    return {row[0]: set(row[1].split()) for row in conn.execute('SELECT id,body FROM docs')}


def check(conn, asts, reference, phase, receipt):
    for index, ast in enumerate(asts):
        query = render(ast)
        expected = sorted(i for i, tokens in reference.items() if matches(ast, tokens))
        for mode in ('off', 'on'):
            receipt['active'] = dict(phase=phase, query_index=index, query=query, mode=mode)
            conn.execute('SET stannum.force_count_pages=' + mode)
            actual = [r[0] for r in conn.execute('SELECT id FROM docs WHERE body ==> %s ORDER BY id', (query,))]
            count = conn.execute('SELECT count(*) FROM docs WHERE body ==> %s', (query,)).fetchone()[0]
            plan = conn.execute('EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM docs WHERE body ==> %s', (query,)).fetchone()[0][0]['Plan']
            assert plan['Custom Plan Provider'] == 'Stannum Count', plan
            strategy = plan['Count Strategy']
            if mode == 'on':
                assert strategy == 'page bitmaps', plan
            assert actual == expected and count == len(expected), dict(expected=expected, actual=actual, count=count)
            receipt['strategies'][strategy] += 1
            receipt['checks'] += 1


def exercise(spec, receipt):
    import psycopg
    schema = 'count_fuzz_' + uuid.uuid4().hex
    with psycopg.connect('', autocommit=True, prepare_threshold=None) as writer, psycopg.connect('', autocommit=True, prepare_threshold=None) as reader:
        receipt['server_version'] = writer.execute('SHOW server_version').fetchone()[0]
        writer.execute('CREATE SCHEMA ' + schema)
        try:
            for conn in (writer, reader):
                conn.execute('SET search_path=' + schema + ',public')
                conn.execute('SET enable_seqscan=off; SET stannum.enable_custom_scan=on; SET statement_timeout=30000')
            writer.execute('CREATE TABLE docs(id int PRIMARY KEY, body text, payload int DEFAULT 0) WITH(fillfactor=60)')
            middle = len(spec['documents'])//2
            with writer.cursor() as cur:
                cur.executemany('INSERT INTO docs(id,body) VALUES (%s,%s)', spec['documents'][:middle])
            writer.execute('CREATE INDEX docs_idx ON docs USING stannum(body)')
            writer.execute('SET stannum.write_buffer_docs=' + str(spec['buffer_docs']))
            writer.execute('SET stannum.merge_tier_factor=64')
            with writer.cursor() as cur:
                cur.executemany('INSERT INTO docs(id,body) VALUES (%s,%s)', spec['documents'][middle:])
            receipt['segments'] = writer.execute("SELECT count(*) FROM stannum.segment_info('docs_idx') WHERE kind='immutable'").fetchone()[0]
            assert receipt['segments'] > 1
            writer.execute('VACUUM ANALYZE docs')
            for turn, batch in enumerate(spec['mutations']):
                reader.execute('BEGIN ISOLATION LEVEL REPEATABLE READ')
                old = snapshot(reader)
                check(reader, spec['queries'], old, f'{turn}:before', receipt)
                writer.execute('BEGIN')
                for op in batch['operations']:
                    if op['kind'] == 'insert':
                        writer.execute('INSERT INTO docs(id,body) VALUES (%s,%s)', (op['id'],op['body']))
                    elif op['kind'] == 'text':
                        writer.execute('UPDATE docs SET body=%s WHERE id=%s', (op['body'],op['id']))
                    elif op['kind'] == 'payload':
                        writer.execute('UPDATE docs SET payload=payload+1 WHERE id=%s', (op['id'],))
                    else:
                        writer.execute('DELETE FROM docs WHERE id=%s', (op['id'],))
                writer.execute('ROLLBACK' if batch['rollback'] else 'COMMIT')
                # VACUUM must retain tuples still visible to the reader.
                writer.execute('VACUUM ANALYZE docs')
                check(reader, spec['queries'], old, f'{turn}:retained', receipt)
                reader.execute('COMMIT')
                fresh = snapshot(reader)
                if batch['rollback']:
                    assert fresh == old
                check(reader, spec['queries'], fresh, f'{turn}:fresh', receipt)
                writer.execute('VACUUM ANALYZE docs')
                check(reader, spec['queries'], fresh, f'{turn}:vacuumed', receipt)
        finally:
            reader.execute('ROLLBACK')
            writer.execute('ROLLBACK')
            writer.execute('DROP SCHEMA ' + schema + ' CASCADE')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seed', type=int, default=20260920)
    parser.add_argument('--seeds', type=int, default=3)
    parser.add_argument('--rows', type=int, default=1000)
    parser.add_argument('--queries', type=int, default=80)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--replay', type=Path, help='Replay a previously saved fixture instead of generating seeds')
    args = parser.parse_args()
    if args.rows < 512 or min(args.queries,args.rounds,args.seeds) < 1:
        parser.error('rows must be >=512 and query/round/seed counts positive')
    args.output.mkdir(parents=True, exist_ok=False)
    specifications = ([json.loads(args.replay.read_text())] if args.replay else
                      [fixture(seed,args.rows,args.queries,args.rounds) for seed in range(args.seed,args.seed+args.seeds)])
    for spec in specifications:
        seed = spec['seed']
        (args.output/f'{seed}-fixture.json').write_text(json.dumps(spec,indent=2)+'\n')
        receipt = dict(seed=seed, status='running', checks=0, strategies=collections.Counter(),
                       harness_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest())
        try:
            exercise(spec,receipt)
            receipt.update(status='passed')
        except BaseException as error:
            receipt.update(status='failed', error=repr(error))
            raise
        finally:
            (args.output/f'{seed}-result.json').write_text(json.dumps(receipt,indent=2)+'\n')
        print(json.dumps(receipt), flush=True)


if __name__ == '__main__':
    main()
