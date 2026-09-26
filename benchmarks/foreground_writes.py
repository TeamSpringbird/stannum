#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Attribute single-row INSERT execution latency to folds and merges.

Uses a fresh disposable database on the server selected by libpq environment
variables. Run against an installed release build, holding the installation
lock on shared development machines. EXPLAIN measures server execution, not
commit/fsync or client latency. Between inserts, directory inspection warms
index buffers; this is an attribution probe, not a throughput benchmark.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import uuid


def decode_stream(text):
    decoder = json.JSONDecoder()
    result = []
    while text.strip():
        text = text.lstrip()
        value, end = decoder.raw_decode(text)
        result.append(value)
        text = text[end:]
    return result


def classify(before, after):
    """Immutable generation changes with one writer and no maintenance backend."""
    old = {row['generation']: row for row in before}
    new = {row['generation']: row for row in after}
    removed = old.keys() - new.keys()
    added = new.keys() - old.keys()
    if removed and not added:
        raise ValueError('retired generations without a replacement')
    kind = 'fold_and_merge' if removed else 'fold' if added else 'buffered'
    return {
        'kind': kind,
        'retired_generations': sorted(removed),
        'new_generations': sorted(added),
        # Cascaded merges may rewrite newly built runs as well. Do not call
        # this total merge work; only pre-existing retired inputs are visible.
        'retired_existing_docs': sum(old[g]['docs'] for g in removed),
        'segments_after': len(new),
    }


def summarize(samples):
    result = {}
    for kind in ('buffered', 'fold', 'fold_and_merge'):
        selected = [s for s in samples if s['kind'] == kind]
        if not selected:
            continue
        times = sorted(s['execution_ms'] for s in selected)
        result[kind] = {
            'samples': len(times), 'p50_ms': statistics.median(times),
            'p99_ms': times[min(len(times) - 1, int(len(times) * .99))] if len(times) >= 100 else None,
            'max_ms': max(times),
            'wal_bytes': sum(s['wal_bytes'] for s in selected),
            'slowest_ids': [s['id'] for s in sorted(selected, key=lambda s: -s['execution_ms'])[:5]],
        }
    return result


SNAPSHOT = """SELECT coalesce(json_agg(json_build_object(
    'generation', generation, 'docs', docs) ORDER BY generation), '[]'::json)
    FROM stannum.segment_info('docs_idx') WHERE kind = 'immutable';"""


def script(docs, repeat, buffer_docs, merge_docs):
    statements = [
        'CREATE EXTENSION stannum;',
        'CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);',
        'CREATE INDEX docs_idx ON docs USING stannum(body);',
        f'SET stannum.write_buffer_docs={buffer_docs};',
        'SET stannum.write_buffer_bytes=67108864;',
        f'SET stannum.max_merge_docs={merge_docs};',
        'SET stannum.merge_tier_factor=8;',
        'SET stannum.max_segments=128;',
        'SET statement_timeout=60000;',
        SNAPSHOT,
    ]
    for n in range(1, docs + 1):
        statements += [
            'EXPLAIN (ANALYZE, BUFFERS, WAL, TIMING OFF, FORMAT JSON) '
            f"INSERT INTO docs VALUES ({n}, 'w' || ({n} % 97) || ' ' || "
            f"repeat('common filler ', {repeat}) || md5('{n}'));",
            SNAPSHOT,
        ]
    # Membership oracle uses arithmetic on fixture IDs, never the text operator.
    statements += [
        'SET enable_seqscan=off;',
        """WITH actual AS MATERIALIZED (SELECT id FROM docs WHERE body ==> 'common AND w7'),
        expected AS MATERIALIZED (SELECT id FROM docs WHERE id % 97=7),
        delta AS ((SELECT * FROM actual EXCEPT SELECT * FROM expected)
        UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM actual))
        SELECT json_build_object('differences', (SELECT count(*) FROM delta),
        'heap_docs', (SELECT count(*) FROM docs),
        'index_docs', (SELECT sum(docs) FROM stannum.segment_info('docs_idx')),
        'verify_errors', (SELECT count(*) FROM stannum.verify_index('docs_idx', true)
                         WHERE severity='error'));""",
    ]
    return '\n'.join(statements)


def analyze(values, docs):
    if len(values) != docs * 2 + 2:
        raise ValueError(f'expected {docs * 2 + 2} JSON values, got {len(values)}')
    samples = []
    before = values[0]
    for n in range(1, docs + 1):
        plan, after = values[2 * n - 1:2 * n + 1]
        root = plan[0]
        if root['Plan']['Node Type'] != 'ModifyTable':
            raise ValueError('expected an executed INSERT plan')
        row = dict(id=n, execution_ms=root['Execution Time'],
                   wal_bytes=root['Plan']['WAL Bytes'],
                   shared_hit_blocks=root['Plan']['Shared Hit Blocks'],
                   shared_read_blocks=root['Plan']['Shared Read Blocks'],
                   **classify(before, after))
        samples.append(row)
        before = after
    correct = values[-1]
    if correct != dict(differences=0, heap_docs=docs, index_docs=docs, verify_errors=0):
        raise ValueError(f'correctness failed: {correct}')
    return dict(summary=summarize(samples), samples=samples, correctness=correct)


def positive(value):
    value = int(value)
    if value <= 0:
        raise argparse.ArgumentTypeError('must be positive')
    return value


def nonnegative(value):
    value = int(value)
    if not 0 <= value <= 2147483647:
        raise argparse.ArgumentTypeError('must be between 0 and 2147483647')
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--docs', type=positive, default=4097)
    parser.add_argument('--repeat', type=positive, default=20)
    parser.add_argument('--write-buffer-docs', type=positive, default=32)
    parser.add_argument('--max-merge-docs', type=nonnegative, default=1024,
                        metavar='N')
    parser.add_argument('--artifact', type=Path, required=True, help='installed extension library')
    parser.add_argument('--output', type=Path, required=True, help='new artifact directory')
    args = parser.parse_args()
    if args.write_buffer_docs > 1_000_000:
        parser.error('--write-buffer-docs exceeds the server GUC maximum')
    if args.repeat > 10_000:
        parser.error('--repeat must be <= 10000 to keep each fixture document below the byte cap')
    args.output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, PGAPPNAME='stannum-foreground-probe', PGOPTIONS='')
    database = 'stannum_foreground_' + uuid.uuid4().hex

    def command(argv, **kwargs):
        return subprocess.run(argv, env=env, text=True, capture_output=True, check=True,
                              timeout=600, **kwargs).stdout

    def sql(statement):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-d', database], input=statement)

    binary_hash = hashlib.sha256(args.artifact.read_bytes()).hexdigest()
    query = script(args.docs, args.repeat, args.write_buffer_docs, args.max_merge_docs)
    (args.output / 'workload.sql').write_text(query)
    created = False
    try:
        command(['createdb', '--maintenance-db=postgres', database])
        created = True
        settings = sql("SELECT json_build_object('version', version(), 'shared_buffers', "
                       "current_setting('shared_buffers'), 'synchronous_commit', "
                       "current_setting('synchronous_commit'));")
        output = sql(query)
        (args.output / 'raw.json-stream').write_text(output)
        result = analyze(decode_stream(output), args.docs)
        if hashlib.sha256(args.artifact.read_bytes()).hexdigest() != binary_hash:
            raise RuntimeError('installed extension changed during measurement')
        result['config'] = {key: str(value) if isinstance(value, Path) else value
                            for key, value in vars(args).items()}
        result['server'] = json.loads(settings)
        result['artifact_sha256'] = binary_hash
        result['status'] = 'passed'
        (args.output / 'results.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result['summary'], indent=2))
    except subprocess.CalledProcessError as error:
        (args.output / 'failure.txt').write_text(error.stdout + error.stderr)
        raise
    finally:
        if created:
            command(['dropdb', '--maintenance-db=postgres', database])


if __name__ == '__main__':
    main()
