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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--docs', type=positive, default=32768)
    parser.add_argument('--repeat', type=positive, default=200)
    parser.add_argument('--scenario', choices=['merge', 'rewrite', 'mixed'], default='merge')
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
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
    delete = '' if args.scenario == 'merge' else 'DELETE FROM docs WHERE id % 4 <> 0;'
    setup = f"""CREATE EXTENSION stannum;
CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);
INSERT INTO docs SELECT n, 'w' || (n % 97) || ' ' || repeat('common filler ', {args.repeat}) || md5(n::text)
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
        before = json.loads(sql("SELECT json_agg(row_to_json(s)) FROM stannum.segment_info('docs_idx') s"))
        assert len(before) == segments, before
        save(output / 'before.json', before)
        save(output / 'server.json', json.loads(sql("SELECT json_build_object('version',version(),'shared_buffers',current_setting('shared_buffers'),'vacuum_cost_delay',current_setting('vacuum_cost_delay'))")))
        expected = int(sql('SELECT count(*) FROM docs WHERE id % 97=7'))
        # No logical mutations during traffic: the arithmetic fixture oracle
        # remains constant and every reader transaction checks membership count.
        query = f"SELECT 1 / CASE WHEN (SELECT count(*) FROM docs WHERE body ==> 'common AND w7') = {expected} THEN 1 ELSE 0 END;\n"
        (output / 'reader.sql').write_text(query)
        plan = json.loads(sql('EXPLAIN (FORMAT JSON) ' + query))
        assert 'docs_idx' in json.dumps(plan), plan
        save(output / 'reader-plan.json', plan)
        sql('CHECKPOINT')
        log = (output / 'reader.txt').open('w')
        reader = subprocess.Popen(['pgbench', '-n', '-c', '2', '-j', '2', '-T', '4',
                                   '-f', str(output / 'reader.sql'), '-l', '--log-prefix', str(output / 'reader-log'), database],
                                  env=dict(env, PGAPPNAME='vacuum-probe-reader'), stdout=log, stderr=subprocess.STDOUT)
        reader_start = time.monotonic()
        deadline = time.monotonic() + 10
        while int(sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='vacuum-probe-reader'")) < 2:
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
        summary = summarize_logs(list(output.glob('reader-log.*')), ['reader'], elapsed)
        assert not summary['failures'], summary
        overlap = []
        for path in output.glob('reader-log.*'):
            for line in path.read_text().splitlines():
                fields = line.split()
                end = int(fields[4]) + int(fields[5]) / 1e6
                start = end - int(fields[2]) / 1e6
                if started_epoch <= start <= end <= ended_epoch:
                    overlap.append(line)
        assert overlap, 'no complete reader transactions overlapped VACUUM'
        (output / 'overlap.log').write_text('\n'.join(overlap) + '\n')
        during = summarize_logs([output / 'overlap.log'], ['reader'], vacuum_seconds)
        after = json.loads(sql("SELECT coalesce(json_agg(row_to_json(s)), '[]'::json) FROM stannum.segment_info('docs_idx') s"))
        assert len(after) == 1, after
        remaining = args.docs if args.scenario == 'merge' else args.docs // 4
        assert sum(s['docs'] for s in after) == remaining, after
        assert sum(s['dead_docs'] for s in after) == 0, after
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx',true)") == '0'
        assert sql("SELECT count(*) FROM docs WHERE body ==> 'common AND w7'") == str(expected)
        assert sql("""WITH actual AS MATERIALIZED (SELECT id FROM docs WHERE body ==> 'common AND w7'),
            expected AS (SELECT id FROM docs WHERE id % 97=7),
            delta AS ((SELECT * FROM actual EXCEPT ALL SELECT * FROM expected)
              UNION ALL (SELECT * FROM expected EXCEPT ALL SELECT * FROM actual))
            SELECT count(*) FROM delta""") == '0'
        assert hashlib.sha256(args.artifact.read_bytes()).hexdigest() == digest
        save(output / 'after.json', after)
        save(output / 'rss.json', samples)
        save(output / 'results.json', dict(status='passed', artifact_sha256=digest, vacuum_seconds=vacuum_seconds,
            vacuum_epoch=[started_epoch, ended_epoch], wal_bytes=wal_bytes, rss_samples=len(samples),
            sampled_backend_rss_max=max((s['rss_bytes'] for s in samples), default=None),
            reader=summary, during_vacuum=during, expected_matches=expected, live_docs=remaining))
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
