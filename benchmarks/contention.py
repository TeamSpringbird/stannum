#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Measure concurrent insert/search tails and sampled waits on a disposable DB.

Requires psql, pgbench, an installed Stannum, and libpq connection settings.
Hold the installation lock when sharing a development server. Samples report
observed backend wait events, not a measured lock duration or attribution to a
particular index page. Generation changes describe sampling intervals; they do
not identify which concurrent INSERT folded or merged.
"""
import argparse
import collections
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import uuid

from foreground_writes import nonnegative, positive
from run import summarize_logs


ORACLE = """WITH actual AS MATERIALIZED (
 SELECT id FROM docs WHERE body ==> 'common AND w7'),
 expected AS MATERIALIZED (SELECT id FROM docs WHERE id % 97=7),
 delta AS ((SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
 UNION ALL (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual)),
 broad AS MATERIALIZED (SELECT id FROM docs WHERE body ==> 'common'),
 broad_delta AS ((SELECT id FROM docs EXCEPT ALL SELECT * FROM broad)
 UNION ALL (SELECT * FROM broad EXCEPT ALL SELECT id FROM docs))
 SELECT json_build_object('differences', (SELECT count(*) FROM delta),
 'broad_differences', (SELECT count(*) FROM broad_delta),
 'heap_docs', (SELECT count(*) FROM docs));"""

SAMPLE = """SELECT json_build_object('epoch', extract(epoch FROM clock_timestamp()),
 'backends', (SELECT coalesce(json_agg(json_build_object(
 'pid',pid,'application',application_name,'state',state,
 'wait_type',wait_event_type,'wait',wait_event)), '[]'::json)
 FROM pg_stat_activity WHERE datname=current_database()
 AND application_name IN ('stannum-contention-reader','stannum-contention-writer')),
 'segments', (SELECT coalesce(json_agg(json_build_object('generation', generation,
 'docs', docs) ORDER BY generation), '[]'::json)
 FROM stannum.segment_info('docs_idx') WHERE kind='immutable'));"""


def assert_oracle(result, minimum_docs=1):
    if (result.get('differences') != 0 or result.get('broad_differences') != 0
            or not isinstance(result.get('heap_docs'), int)
            or result['heap_docs'] < minimum_docs):
        raise ValueError(f'membership oracle failed: {result}')


def contained_checks(checks, start, end):
    """Only whole oracle executions inside the shared traffic window count."""
    if end <= start:
        return []
    return [check for check in checks
            if start <= check['query_start_monotonic']
            <= check['query_end_monotonic'] <= end]


def interval(before, after):
    """Concurrent changes are interval-level evidence, never per-insert labels."""
    old = {s['generation']: s for s in before['segments']}
    new = {s['generation']: s for s in after['segments']}
    retired, added = old.keys() - new.keys(), new.keys() - old.keys()
    return dict(start_epoch=before['epoch'], end_epoch=after['epoch'],
                new_generations=sorted(added), retired_generations=sorted(retired),
                observation=('retirement_observed' if retired else
                             'publication_observed' if added else 'no_change_observed'))


def summarize_samples(samples):
    waits = collections.Counter()
    active = collections.Counter()
    for sample in samples:
        for backend in sample['backends']:
            role = backend['application'].removeprefix('stannum-contention-')
            if backend['state'] == 'active':
                active[role] += 1
                if backend['wait']:
                    waits[f"{role}:{backend['wait_type']}:{backend['wait']}"] += 1
    return dict(samples=len(samples), active_backend_observations=dict(active),
                active_wait_observations=dict(waits),
                intervals=[interval(a, b) for a, b in zip(samples, samples[1:])])


def validate_traffic(summary, expected_inserts=None):
    if summary['failures']:
        raise ValueError(f"pgbench failures: {summary['failures']}")
    completed = sum(q['completed'] for q in summary['queries'].values())
    if completed <= 0:
        raise ValueError('no successful transactions recorded')
    if expected_inserts is not None and completed != expected_inserts:
        raise ValueError(f'insert accounting mismatch: logged={completed}, heap={expected_inserts}')


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--seconds', type=positive, default=30)
    parser.add_argument('--readers', type=positive, default=2)
    parser.add_argument('--writers', type=positive, default=2)
    parser.add_argument('--writer-rate', type=positive, help='target INSERT transactions/s; uses a fixed scheduling seed')
    parser.add_argument('--initial-docs', type=positive, default=512)
    parser.add_argument('--repeat', type=positive, default=20, help='body repetition count')
    parser.add_argument('--write-buffer-docs', type=positive, default=32)
    parser.add_argument('--max-merge-docs', type=nonnegative, default=1024)
    parser.add_argument('--sample-ms', type=positive, default=100)
    parser.add_argument('--check-ms', type=positive, default=1000)
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.write_buffer_docs > 1_000_000 or args.repeat > 10_000:
        parser.error('buffer docs must be <= 1000000 and repeat <= 10000')
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    database = 'stannum_contention_' + uuid.uuid4().hex
    binary_hash = hashlib.sha256(args.artifact.read_bytes()).hexdigest()
    options = (f'-c stannum.write_buffer_docs={args.write_buffer_docs} '
               f'-c stannum.max_merge_docs={args.max_merge_docs} '
               '-c stannum.write_buffer_bytes=67108864 -c stannum.merge_tier_factor=8 '
               '-c stannum.max_segments=128 -c statement_timeout=60000 '
               '-c enable_seqscan=off -c jit=off')
    env = dict(os.environ, PGOPTIONS=options, PGAPPNAME='stannum-contention-monitor')
    command_timeout = args.seconds + 120

    def command(argv, **kwargs):
        return subprocess.run(argv, text=True, capture_output=True, check=True,
                              timeout=command_timeout, env=env, **kwargs).stdout

    def sql(statement):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-d', database], input=statement)

    body = f"'w' || (n % 97) || ' ' || repeat('common filler ', {args.repeat}) || md5(n::text)"
    setup = f"""CREATE EXTENSION stannum;
CREATE SEQUENCE doc_ids START {args.initial_docs + 1};
CREATE TABLE docs(id bigint PRIMARY KEY, body text NOT NULL) WITH (autovacuum_enabled=false);
INSERT INTO docs SELECT n, {body} FROM generate_series(1,{args.initial_docs}) n;
CREATE INDEX docs_idx ON docs USING stannum(body);
ANALYZE docs;"""
    reader = "SELECT sum(id) FROM docs WHERE body ==> 'common AND w7';\n"
    writer = f"INSERT INTO docs SELECT n, {body} FROM (SELECT nextval('doc_ids') n) fixture;\n"
    for name, query in [('setup', setup), ('reader', reader), ('writer', writer), ('oracle', ORACLE), ('sample', SAMPLE)]:
        (args.output / f'{name}.sql').write_text(query)
    save(args.output / 'manifest.json', dict(
        config={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        database=database, artifact_sha256=binary_hash,
        harness_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        interpretation='Client transaction latency includes commit and network. Wait samples are observations, not lock durations or metadata-page attribution. Generation transitions are interval observations, not per-INSERT attribution. Monitor and oracle overhead is included.'))
    created = False
    processes, handles, threads, errors = [], [], [], []
    stop = threading.Event()
    samples, checks = [], []

    def monitor(name, period_ms, query, target, validate=None):
        try:
            with (args.output / f'{name}.jsonl').open('w') as output:
                while not stop.is_set():
                    start = time.monotonic()
                    value = json.loads(sql(query))
                    value['query_start_monotonic'] = start
                    value['query_end_monotonic'] = time.monotonic()
                    value['client_epoch'] = time.time()
                    value['probe_ms'] = (time.monotonic() - start) * 1000
                    target.append(value)
                    output.write(json.dumps(value) + '\n')
                    output.flush()
                    if validate:
                        validate(value, args.initial_docs)
                    stop.wait(max(0, period_ms / 1000 - (time.monotonic() - start)))
        except Exception as error:
            errors.append(f'{name}: {error}')
            stop.set()

    try:
        command(['createdb', '--maintenance-db=postgres', database])
        created = True
        sql(setup)
        settings = json.loads(sql("""SELECT json_build_object('version',version(),
 'settings', (SELECT json_object_agg(name, current_setting(name)) FROM unnest(ARRAY[
 'shared_buffers','synchronous_commit','max_parallel_workers_per_gather',
 'statement_timeout','enable_seqscan','jit','stannum.write_buffer_docs',
 'stannum.write_buffer_bytes','stannum.max_merge_docs','stannum.merge_tier_factor',
 'stannum.max_segments']) name));"""))
        settings['pgbench_version'] = command(['pgbench', '--version']).strip()
        # Checkout identity documents the harness source, not the installed library.
        identity = subprocess.run(['git', '-C', str(Path(__file__).resolve().parent),
                                   'rev-parse', 'HEAD'], text=True, capture_output=True)
        settings['harness_checkout_commit'] = identity.stdout.strip() if identity.returncode == 0 else None
        save(args.output / 'server.json', settings)
        before = json.loads(sql(ORACLE))
        assert_oracle(before, args.initial_docs)
        save(args.output / 'before.json', before)
        plan = json.loads(sql('EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) ' + reader))
        save(args.output / 'reader-plan.json', plan)
        if 'docs_idx' not in json.dumps(plan):
            raise ValueError('reader plan did not use docs_idx')
        save(args.output / 'oracle-plan.json', json.loads(sql(
            'EXPLAIN (FORMAT JSON) ' + ORACLE)))
        samples.append(json.loads(sql(SAMPLE)))
        for name, period, query, target, validate in [
                ('samples', args.sample_ms, SAMPLE, samples, None),
                ('checks', args.check_ms, ORACLE, checks, assert_oracle)]:
            thread = threading.Thread(target=monitor, args=(name, period, query, target, validate))
            thread.start()
            threads.append(thread)
        started = {}
        for role, clients in [('writer', args.writers), ('reader', args.readers)]:
            output = (args.output / f'{role}.txt').open('w')
            handles.append(output)
            argv = ['pgbench', '-n', '-M', 'simple', '-c', str(clients), '-j', str(min(clients, 4)),
                    '-T', str(args.seconds), '-f', str(args.output / f'{role}.sql'), '-l',
                    '--log-prefix', str(args.output / f'{role}-log'), database]
            if role == 'writer' and args.writer_rate is not None:
                argv.extend(['--rate', str(args.writer_rate), '--random-seed=42'])
            started[role] = time.monotonic()
            proc = subprocess.Popen(argv, env=dict(env, PGAPPNAME=f'stannum-contention-{role}'),
                                    stdout=output, stderr=subprocess.STDOUT)
            processes.append((role, proc))
        elapsed = {}
        finished = {}
        pending = dict(processes)
        deadline = time.monotonic() + command_timeout
        while pending:
            if errors:
                raise RuntimeError('; '.join(errors))
            if time.monotonic() > deadline:
                raise TimeoutError('traffic exceeded deadline')
            for role, proc in list(pending.items()):
                code = proc.poll()
                if code is not None:
                    finished[role] = time.monotonic()
                    elapsed[role] = finished[role] - started[role]
                    if code:
                        raise RuntimeError(f'{role} pgbench exited {code}; see {role}.txt')
                    del pending[role]
            time.sleep(.02)
        stop.set()
        for thread in threads:
            thread.join()
        if errors:
            raise RuntimeError('; '.join(errors))
        samples.append(json.loads(sql(SAMPLE)))
        save(args.output / 'samples.json', samples)
        after = json.loads(sql(ORACLE))
        assert_oracle(after, args.initial_docs)
        after['verify_errors'] = int(sql("SELECT count(*) FROM stannum.verify_index('docs_idx',true) WHERE severity='error';"))
        after['index_docs'] = int(sql("SELECT coalesce(sum(docs),0) FROM stannum.segment_info('docs_idx');"))
        save(args.output / 'after.json', after)
        if after['verify_errors'] or after['index_docs'] != after['heap_docs']:
            raise ValueError(f'index verification/accounting failed: {after}')
        common_start, common_end = max(started.values()), min(finished.values())
        concurrent_checks = contained_checks(checks, common_start, common_end)
        if not concurrent_checks:
            raise ValueError('no correctness checks completed wholly within concurrent traffic')
        summaries = {}
        for role in ('reader', 'writer'):
            summary = summarize_logs(sorted(args.output.glob(f'{role}-log.*')), [role], elapsed[role])
            validate_traffic(summary, after['heap_docs'] - args.initial_docs if role == 'writer' else None)
            summaries[role] = summary
        if hashlib.sha256(args.artifact.read_bytes()).hexdigest() != binary_hash:
            raise RuntimeError('installed extension changed during measurement')
        result = dict(status='passed', traffic=summaries, observations=summarize_samples(samples),
                      concurrent_checks=len(concurrent_checks), total_checks=len(checks),
                      common_traffic_monotonic=[common_start, common_end],
                      correctness=after, artifact_sha256=binary_hash)
        save(args.output / 'results.json', result)
        print(json.dumps(dict(traffic=summaries, concurrent_checks=len(concurrent_checks)), indent=2))
    except Exception as error:
        details = str(error)
        if isinstance(error, subprocess.CalledProcessError):
            details += '\n' + (error.stdout or '') + (error.stderr or '')
        (args.output / 'failure.txt').write_text(details + '\n')
        raise
    finally:
        stop.set()
        for _, proc in processes:
            if proc.poll() is None:
                proc.terminate()
        for _, proc in processes:
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        for thread in threads:
            thread.join()
        for handle in handles:
            handle.close()
        if created:
            command(['dropdb', '--force', '--maintenance-db=postgres', database])


if __name__ == '__main__':
    main()
