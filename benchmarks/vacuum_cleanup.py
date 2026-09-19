#!/usr/bin/env python3
"""Measure one VACUUM with concurrent checked readers on a disposable database.

Uses libpq environment settings and an already installed release library. Hold
the installation lock across the campaign. RSS is sampled backend residency,
including shared mappings; it is neither allocation accounting nor true peak RSS.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import uuid

from foreground_writes import positive
from run import summarize_logs


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def percentage(value):
    number = int(value)
    if not 0 <= number < 100:
        raise argparse.ArgumentTypeError('percentage must be between 0 and 99; all-dead cleanup is covered by correctness tests')
    return number


def validate_workload(args):
    if args.scenario == 'rewrite' and args.delete_percent < 50:
        raise ValueError('rewrite requires at least 50% deleted documents to trigger maintenance')
    if args.scenario == 'merge' and args.delete_percent != 75:
        raise ValueError('delete-percent applies only to rewrite/mixed scenarios')
    if args.vocabulary < 8:
        raise ValueError('vocabulary must be at least eight (selective term is w7)')


def add_workload_arguments(parser):
    parser.add_argument('--delete-percent', type=percentage, default=75)
    parser.add_argument('--vocabulary', type=positive, default=97)
    parser.add_argument('--distribution', choices=['uniform', 'hot'], default='uniform',
                        help='hot assigns 80%% of documents to w0')
    parser.add_argument('--query-shapes', choices=['selective', 'mixed'], default='selective')
    parser.add_argument('--reader-rate', type=positive,
                        help='total offered transactions/sec, seeded Poisson schedule')
    parser.add_argument('--reader-seconds', type=positive, default=4)
    parser.add_argument('--readers', type=positive, default=2)


def term_expression(vocabulary, distribution, column='n'):
    term = f'({column} % {vocabulary})'
    return term if distribution == 'uniform' else f'(CASE WHEN {column} % 5 <> 0 THEN 0 ELSE {term} END)'


def membership_query(shape, term):
    if shape == 'count':
        return 'common', 'true'
    if shape == 'phrase':
        return '"common filler"', 'true'
    return 'common AND w7', f'{term}=7'


def checked_query(query, predicate, aggregate='count(*)'):
    # Both branches use the same SQL snapshot. Expected membership comes from
    # fixture columns, without invoking the text index or tokenizer.
    return f"SELECT 1 / CASE WHEN (SELECT {aggregate} FROM docs WHERE body ==> '{query}') IS NOT DISTINCT FROM (SELECT {aggregate} FROM docs WHERE {predicate}) THEN 1 ELSE 0 END;\n"


def ranked_script():
    # Force exhaustive scoring for the reference. Materializing IDs alone would
    # lose the CTID-bound scoring state; retain scores while scanning instead.
    return r"""BEGIN ISOLATION LEVEL REPEATABLE READ;
SET LOCAL stannum.enable_custom_scan=off;
WITH all_matches AS MATERIALIZED (
 SELECT id, stannum.full_score(ctid) AS score FROM docs WHERE body ==> 'common OR w7'),
 top AS (SELECT id, score FROM all_matches ORDER BY score DESC, id LIMIT 10)
SELECT quote_literal(coalesce(json_agg(score ORDER BY score DESC)::text, '[]')) AS reference FROM top
\gset
SET LOCAL stannum.enable_custom_scan=on;
WITH top AS MATERIALIZED (SELECT id, stannum.full_score(ctid) AS score FROM docs
 WHERE body ==> 'common OR w7' ORDER BY stannum.full_score(ctid) DESC LIMIT 10)
SELECT 1 / CASE WHEN (SELECT coalesce(json_agg(score ORDER BY score DESC)::text, '[]') FROM top) = :reference AND (SELECT count(*) = count(DISTINCT id) FROM top) THEN 1 ELSE 0 END;
COMMIT;
"""


def require_selective_matches(expected):
    if expected <= 0:
        raise ValueError('selective fixture has no live w7 matches; adjust vocabulary, distribution, deletion density, or document count')


def phase_lines(paths, start_epoch, end_epoch):
    result = []
    for path in paths:
        for line in path.read_text().splitlines():
            fields = line.split()
            if len(fields) < 6:
                raise ValueError('malformed pgbench record')
            if not fields[2].isdigit():
                raise ValueError('failed or skipped reader transaction: ' + line)
            end = int(fields[4]) + int(fields[5]) / 1e6
            # Rate-limited pgbench latency includes schedule lag. Execution
            # starts after that lag, not at the scheduled arrival time.
            lag = int(fields[6]) if len(fields) >= 7 else 0
            start = end - (int(fields[2]) - lag) / 1e6
            if start_epoch <= start <= end <= end_epoch:
                result.append(line)
    return result


def load_metrics(paths, rate, seconds):
    lags, starts, ends = [], [], []
    failed = skipped = 0
    for path in paths:
        for line in path.read_text().splitlines():
            fields = line.split()
            if fields[2] == 'skipped':
                skipped += 1
                continue
            if not fields[2].isdigit():
                failed += 1
                continue
            lag = int(fields[6]) if len(fields) >= 7 else 0
            value = int(fields[2])
            end = int(fields[4]) + int(fields[5]) / 1e6
            lags.append(lag / 1000)
            starts.append(end - value / 1e6)
            ends.append(end)
    ordered = sorted(zip(starts, ends))
    tail = max(1, len(ordered) // 10)
    # Delays in the first/last scheduled deciles reveal queue accumulation.
    delays = [(end - start) * 1000 for start, end in ordered]
    from run import percentile
    return dict(offered_rate=rate, nominal_offered=rate * seconds if rate else None,
                scheduled_logged=len(lags) + failed + skipped, completed=len(lags),
                failed=failed, skipped=skipped,
                schedule_lag_p95_ms=percentile(lags, .95),
                schedule_lag_max_ms=max(lags, default=None),
                late_over_1ms=sum(lag > 1 for lag in lags),
                scheduled_latency_first_decile_p95_ms=percentile(delays[:tail], .95),
                scheduled_latency_last_decile_p95_ms=percentile(delays[-tail:], .95),
                interpretation='Seeded rate is Poisson, not an exact arrival count. Logged schedules exclude any arrivals never emitted at shutdown. Schedule lag measures client queuing, not server queue depth; latency includes lag with --rate.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--docs', type=positive, default=32768)
    parser.add_argument('--repeat', type=positive, default=200)
    parser.add_argument('--scenario', choices=['merge', 'rewrite', 'mixed'], default='merge')
    parser.add_argument('--vacuum-strategy', choices=['auto', 'direct', 'reconstruct'])
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    add_workload_arguments(parser)
    args = parser.parse_args()
    try:
        validate_workload(args)
    except ValueError as error:
        parser.error(str(error))
    if args.docs % 8 or args.repeat > 10000:
        parser.error('docs must be divisible by eight; repeat must be <= 10000')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    database = 'stannum_vacuum_' + uuid.uuid4().hex
    digest = hashlib.sha256(args.artifact.read_bytes()).hexdigest()
    env = dict(os.environ, PGDATABASE=database, PGOPTIONS=(
        '-c jit=off -c enable_seqscan=off -c statement_timeout=180000 '
        '-c stannum.merge_tier_factor=8 -c stannum.max_segments=128 '
        '-c vacuum_cost_delay=0'))
    if args.vacuum_strategy:
        env['PGOPTIONS'] += ' -c stannum.experimental_vacuum_merge_strategy=' + args.vacuum_strategy

    def command(argv, **kwargs):
        result = subprocess.run(argv, env=env, text=True, capture_output=True,
                                timeout=240, **kwargs)
        if result.returncode:
            save(output / 'command-error.json', dict(argv=argv, stdout=result.stdout, stderr=result.stderr))
            result.check_returncode()
        return result.stdout

    def sql(statement):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=statement).strip()

    segments = 1 if args.scenario == 'rewrite' else 8
    predicate = 'id % 4 <> 0' if args.delete_percent == 75 else f'id % 100 < {args.delete_percent}'
    delete = '' if args.scenario == 'merge' else f'DELETE FROM docs WHERE {predicate};'
    setup = f"""CREATE EXTENSION stannum;
CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);
INSERT INTO docs SELECT n, 'w' || {term_expression(args.vocabulary, args.distribution)} || ' ' || repeat('common filler ', {args.repeat}) || md5(n::text)
 FROM generate_series(1,{args.docs}) n;
SET stannum.build_segment_docs={args.docs // segments};
SET stannum.merge_tier_factor=64;
CREATE INDEX docs_idx ON docs USING stannum(body);
{delete}
ANALYZE docs;"""
    (output / 'setup.sql').write_text(setup)
    save(output / 'manifest.json', dict(config={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                                       artifact_sha256=digest,
                                       harness_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()))
    created = False
    reader = vacuum = None
    log = None
    stop = threading.Event()
    samples = []
    sample_errors = []
    sampler = None
    try:
        command(['createdb', '--maintenance-db=postgres', database])
        created = True
        sql(setup)
        if args.vacuum_strategy:
            # Unknown custom-GUC placeholders also pass current_setting();
            # require a real registered setting before labelling the strategy.
            assert sql("SELECT setting FROM pg_settings WHERE name='stannum.experimental_vacuum_merge_strategy'") == args.vacuum_strategy
        before = json.loads(sql("SELECT json_agg(row_to_json(s)) FROM stannum.segment_info('docs_idx') s"))
        assert len(before) == segments, before
        save(output / 'before.json', before)
        save(output / 'server.json', json.loads(sql("SELECT json_build_object('version',version(),'shared_buffers',current_setting('shared_buffers'),'vacuum_cost_delay',current_setting('vacuum_cost_delay'))")))
        remaining = int(sql('SELECT count(*) FROM docs'))
        save(output / 'dataset.json', json.loads(sql("SELECT json_build_object('live_docs',count(*),'body_bytes',sum(octet_length(body)),'min_body_bytes',min(octet_length(body)),'max_body_bytes',max(octet_length(body))) FROM docs")))
        term = term_expression(args.vocabulary, args.distribution, 'id')
        expected = int(sql(f'SELECT count(*) FROM docs WHERE {term}=7'))
        require_selective_matches(expected)
        names = ['selective'] if args.query_shapes == 'selective' else ['count', 'selective', 'phrase', 'ranked']
        scripts = []
        for name in names:
            if name == 'ranked':
                query = ranked_script()
                explain = "SELECT id FROM docs WHERE body ==> 'common OR w7' ORDER BY stannum.full_score(ctid) DESC LIMIT 10"
            else:
                text_query, predicate = membership_query(name, term)
                aggregate = 'array_agg(id ORDER BY id)' if name == 'selective' and args.query_shapes == 'mixed' else 'count(*)'
                query = checked_query(text_query, predicate, aggregate)
                if args.query_shapes == 'selective':
                    # Preserve the original lightweight count probe. No logical
                    # writes occur during VACUUM, so this heap oracle is stable.
                    query = f"SELECT 1 / CASE WHEN (SELECT count(*) FROM docs WHERE body ==> '{text_query}') = {expected} THEN 1 ELSE 0 END;\n"
                explain = query
            script = output / f'reader-{name}.sql'
            script.write_text(query)
            scripts.extend(['-f', str(script)])
            plan = json.loads(sql('EXPLAIN (FORMAT JSON) ' + explain))
            assert 'docs_idx' in json.dumps(plan), plan
            if name == 'ranked':
                assert '"Top K": 10' in json.dumps(plan) and '"Order": "score DESC"' in json.dumps(plan), plan
            save(output / f'reader-{name}-plan.json', plan)
        sql('CHECKPOINT')
        log = (output / 'reader.txt').open('w')
        rate = [] if args.reader_rate is None else ['--rate', str(args.reader_rate), '--random-seed=42']
        reader = subprocess.Popen(['pgbench', '-n', '-c', str(args.readers), '-j', str(min(args.readers, 4)),
                                   '-T', str(args.reader_seconds), *scripts, *rate,
                                   '-l', '--log-prefix', str(output / 'reader-log'), database],
                                  env=dict(env, PGAPPNAME='vacuum-probe-reader'), stdout=log, stderr=subprocess.STDOUT)
        reader_start = time.monotonic()
        deadline = time.monotonic() + 10
        while int(sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='vacuum-probe-reader'")) < args.readers:
            if reader.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('readers did not start')
            time.sleep(.01)
        # Dedicated backend lets RSS sampling follow the process doing VACUUM.
        vacuum = subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        vacuum.stdin.write('SELECT pg_backend_pid();\n')
        vacuum.stdin.flush()
        pid = int(vacuum.stdout.readline())
        def sample():
            try:
                while not stop.is_set():
                    result = subprocess.run(['ps', '-o', 'rss=', '-p', str(pid)], text=True, capture_output=True, timeout=5)
                    if result.returncode == 0 and result.stdout.strip():
                        samples.append(dict(epoch=time.time(), rss_bytes=int(result.stdout.strip()) * 1024))
                    stop.wait(.02)
            except Exception as error:
                sample_errors.append(str(error))
        sampler = threading.Thread(target=sample)
        sampler.start()
        wal_before = sql('SELECT pg_current_wal_insert_lsn()')
        started_epoch = time.time()
        started = time.monotonic()
        stdout, stderr = vacuum.communicate('VACUUM (INDEX_CLEANUP ON) docs;\n', timeout=180)
        vacuum_seconds = time.monotonic() - started
        ended_epoch = time.time()
        stop.set()
        sampler.join(timeout=6)
        assert not sampler.is_alive() and not sample_errors, sample_errors
        assert samples, 'no backend RSS samples captured'
        (output / 'vacuum.stdout').write_text(stdout)
        (output / 'vacuum.stderr').write_text(stderr)
        assert vacuum.returncode == 0, stderr
        assert reader.poll() is None, 'VACUUM outlasted the reader window'
        wal_bytes = int(sql(f"SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(), '{wal_before}')::bigint"))
        reader.wait(timeout=180)
        elapsed = time.monotonic() - reader_start
        assert reader.returncode == 0, 'reader failed; see reader.txt'
        paths = list(output.glob('reader-log.*'))
        summary = summarize_logs(paths, names, elapsed)
        offered_load = load_metrics(paths, args.reader_rate, args.reader_seconds)
        assert not summary['failures'], summary
        overlap = phase_lines(paths, started_epoch, ended_epoch)
        assert overlap, 'no complete reader transactions overlapped VACUUM'
        (output / 'overlap.log').write_text('\n'.join(overlap) + '\n')
        during = summarize_logs([output / 'overlap.log'], names, vacuum_seconds)
        after = json.loads(sql("SELECT coalesce(json_agg(row_to_json(s)), '[]'::json) FROM stannum.segment_info('docs_idx') s"))
        assert len(after) == (1 if remaining else 0), after
        assert sum(s['docs'] for s in after) == remaining, after
        assert sum(s['dead_docs'] for s in after) == 0, after
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx',true)") == '0'
        assert sql("SELECT count(*) FROM docs WHERE body ==> 'common AND w7'") == str(expected)
        assert sql(f"""WITH actual AS MATERIALIZED (SELECT id FROM docs WHERE body ==> 'common AND w7'),
            expected AS (SELECT id FROM docs WHERE {term}=7),
            delta AS ((SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
              UNION ALL (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual))
            SELECT count(*) FROM delta""") == '0'
        assert hashlib.sha256(args.artifact.read_bytes()).hexdigest() == digest
        save(output / 'after.json', after)
        save(output / 'rss.json', samples)
        save(output / 'results.json', dict(status='passed', artifact_sha256=digest, vacuum_seconds=vacuum_seconds,
            vacuum_epoch=[started_epoch, ended_epoch], wal_bytes=wal_bytes, rss_samples=len(samples),
            sampled_backend_rss_max=max((s['rss_bytes'] for s in samples), default=None),
            reader=summary, offered_load=offered_load, during_vacuum=during,
            during_vacuum_load=load_metrics([output / 'overlap.log'], args.reader_rate, vacuum_seconds), expected_matches=expected, live_docs=remaining))
    finally:
        stop.set()
        if sampler:
            sampler.join(timeout=6)
        for process in (reader, vacuum):
            if process is not None and process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        if log:
            log.close()
        if created:
            command(['dropdb', '--maintenance-db=postgres', database])


if __name__ == '__main__':
    main()
