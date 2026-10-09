#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Background maintenance workers in a private cluster that preloads Stannum.

With `shared_preload_libraries = 'stannum'` a launcher starts with the
server, and a fold queues the merges it leaves behind for a maintenance
worker instead of running them in the inserting session. Each case waits
for a state (a directory shape, a counter), polling with a deadline; none
sleeps for a duration and assumes:

- background: inserts into an index with `target_segment_count = 2` fold
  without merging; the worker merges the directory down to two segments;
- manual: a session in manual mode queues nothing and its directory grows;
  back in background mode, one more fold has the worker merge it;
- settings: `stannum.maintenance_jobs_per_db` cannot be SET in a session
  (55P02) and takes effect on reload;
- databases: jobs of two databases are both served, one worker at a time;
- exhausted: with no background worker slot free the launcher cannot start
  a worker, and inserting sessions merge inline instead;
- standby: a hot standby, preloading too, runs no launcher.

Install the build to test first (`script/test-all install`); this runs in a
disposable cluster of its own and needs no lock beyond the installed build:

    python3 postgres/tests/maintenance_workers.py [case...]
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

PORT = 28961
STANDBY_PORT = 28962
DEADLINE = 60


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-maintenance-'))
    data = root / 'data'
    log = root / 'server.log'
    env = dict(os.environ, PGHOST=str(root), PGPORT=str(PORT), PGUSER='postgres', PGDATABASE='postgres',
               PGOPTIONS='-c statement_timeout=60000')
    for name in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(name, None)

    def command(args, **kw):
        try:
            return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kw)
        except subprocess.CalledProcessError as error:
            raise AssertionError(error.output) from error

    def sql(text, database='postgres', port=PORT):
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1', '-d', database, '-p', str(port)],
                       input=text, env=env).strip()

    def wait_for(description, probe, deadline=DEADLINE):
        """Polls `probe` until it returns a true value; fails after `deadline` seconds."""
        end = time.monotonic() + deadline
        while True:
            value = probe()
            if value:
                return value
            assert time.monotonic() < end, f'timed out waiting for {description}'
            time.sleep(.1)

    def status(database='postgres'):
        row = sql('SELECT row_to_json(s) FROM stannum.maintenance_status() s;', database)
        return json.loads(row)

    def segments(index, database='postgres'):
        return int(sql(f"SELECT count(*) FROM stannum.segment_info('{index}') WHERE kind = 'immutable';", database))

    def verify(index, database='postgres'):
        rows = sql(f"SELECT severity || ': ' || location || ': ' || message "
                   f"FROM stannum.verify_index('{index}', true);", database)
        assert rows == '', (index, rows)

    def idle():
        s = status()
        return s['queued_jobs'] == 0 and s['running_jobs'] == 0 and s['worker_pid'] is None

    def fill(table, rows, database='postgres', first=1, settings=''):
        """One row per statement, so every fourth row folds the buffer."""
        statements = '\n'.join(f"INSERT INTO {table} VALUES ({n}, 'needle w{n}');"
                               for n in range(first, first + rows))
        sql(f'SET stannum.write_buffer_docs = 4; {settings}\n{statements}', database)

    def create(table, database='postgres'):
        sql(f'CREATE TABLE {table}(id int, body text);'
            f'CREATE INDEX {table}_idx ON {table} USING stannum(body) WITH (target_segment_count = 2);',
            database)

    def start():
        command(['pg_ctl', '-D', str(data), '-l', str(log), '-w', 'start'])
        # pg_stat_activity, not maintenance_status(): the extension may not
        # exist yet.
        wait_for('the launcher', lambda: sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'stannum maintenance launcher';") == '1')

    def stop():
        command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'])

    def background():
        """Folds queue merges; the worker, not the session, merges them."""
        create('bg')
        before = status()
        assert before['preloaded'] is True and before['launcher_pid'], before
        assert sql("SELECT count(*) FROM pg_stat_activity "
                   "WHERE backend_type = 'stannum maintenance launcher';") == '1'
        fill('bg', 41)
        # Ten folds, each queuing the index (deduplicated while queued);
        # the worker merges down to target_segment_count.
        wait_for('the worker to merge bg_idx down to two segments', lambda: segments('bg_idx') <= 2 and idle())
        after = status()
        assert after['completed'] > before['completed'], (before, after)
        assert after['launches'] > before['launches'], (before, after)
        assert after['inline_fallbacks'] == before['inline_fallbacks'], (before, after)
        assert sql("SELECT count(*) FROM bg WHERE body ==> 'needle';") == '41'
        for n in (1, 20, 40, 41):
            assert sql(f"SELECT string_agg(id::text, ',') FROM bg WHERE body ==> 'w{n}';") == str(n), n
        verify('bg_idx')
        return {'segments': segments('bg_idx'), 'jobs': after['completed'] - before['completed']}

    def manual():
        """Manual mode queues nothing; background mode resumes."""
        create('man')
        wait_for('an idle queue', idle)
        before = status()
        fill('man', 41, settings='SET stannum.index_maintenance_mode = manual;')
        # Queuing is synchronous in the inserting session: had any fold
        # queued a job, the counter would already show it.
        after = status()
        assert after['requested'] == before['requested'], (before, after)
        assert sql('SELECT count(*) FROM stannum.maintenance_jobs();') == '0'
        unmerged = segments('man_idx')
        assert unmerged == 10, unmerged
        fill('man', 4, first=42)
        wait_for('the worker to merge man_idx down to two segments', lambda: segments('man_idx') <= 2 and idle())
        assert status()['requested'] > after['requested']
        assert sql("SELECT count(*) FROM man WHERE body ==> 'needle';") == '45'
        verify('man_idx')
        return {'unmerged': unmerged, 'merged': segments('man_idx')}

    def settings():
        """maintenance_jobs_per_db is a reload setting."""
        refused = subprocess.run(['psql', '-X', '-qAt', '-v', 'VERBOSITY=verbose',
                                  '-c', 'SET stannum.maintenance_jobs_per_db = 1'],
                                 env=env, text=True, capture_output=True)
        assert refused.returncode != 0, refused.stdout
        assert '55P02' in refused.stderr and 'cannot be changed now' in refused.stderr, refused.stderr
        sql('ALTER SYSTEM SET stannum.maintenance_jobs_per_db = 1; SELECT pg_reload_conf();')
        wait_for('the reload', lambda: sql('SHOW stannum.maintenance_jobs_per_db;') == '1')
        return {'refused': '55P02'}

    def databases():
        """Two databases with queued jobs are both served, one worker at a time."""
        sql('CREATE DATABASE other;')
        sql('CREATE EXTENSION stannum;', 'other')
        create('mine')
        create('theirs', 'other')
        wait_for('an idle queue', idle)
        fill('mine', 41)
        fill('theirs', 41, 'other')
        wait_for('both indexes merged', lambda: segments('mine_idx') <= 2
                 and segments('theirs_idx', 'other') <= 2 and idle())
        verify('mine_idx')
        verify('theirs_idx', 'other')
        return {'mine': segments('mine_idx'), 'theirs': segments('theirs_idx', 'other')}

    def exhausted():
        """No worker slot free: inserting sessions merge inline."""
        # The launcher and the logical replication launcher take both slots.
        sql('ALTER SYSTEM SET max_worker_processes = 2;')
        stop()
        start()
        create('starved')
        fill('starved', 9)  # Two folds: the first queues a job the launcher cannot start.
        wait_for('a refused registration', lambda: status()['launch_failures'] > 0)
        before = status()
        fill('starved', 32, first=10)
        after = status()
        assert after['inline_fallbacks'] > before['inline_fallbacks'], (before, after)
        assert after['launches'] == before['launches'], (before, after)
        # Inline deferred merges keep the directory tiered without a worker.
        assert segments('starved_idx') < 10, segments('starved_idx')
        verify('starved_idx')
        sql('ALTER SYSTEM RESET max_worker_processes;')
        stop()
        start()
        return {'inline_fallbacks': after['inline_fallbacks'], 'segments': segments('starved_idx')}

    def standby():
        """A hot standby preloads Stannum but starts no launcher."""
        replica = root / 'standby'
        command(['pg_basebackup', '-D', str(replica), '-R', '-X', 'stream', '-c', 'fast'], env=env)
        with (replica / 'postgresql.conf').open('a') as f:
            f.write(f"\nport={STANDBY_PORT}\nhot_standby=on\n")
        command(['pg_ctl', '-D', str(replica), '-l', str(root / 'standby.log'), '-w', 'start'])
        try:
            assert sql('SELECT pg_is_in_recovery();', port=STANDBY_PORT) == 't'
            row = json.loads(sql('SELECT row_to_json(s) FROM stannum.maintenance_status() s;', port=STANDBY_PORT))
            assert row['preloaded'] is True and row['launcher_pid'] is None, row
            assert sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type LIKE 'stannum%';",
                       port=STANDBY_PORT) == '0'
        finally:
            command(['pg_ctl', '-D', str(replica), '-m', 'fast', '-w', 'stop'])
        return {'launcher': None}

    cases = {'background': background, 'manual': manual, 'settings': settings,
             'databases': databases, 'exhausted': exhausted, 'standby': standby}
    selected = sys.argv[1:] or list(cases)
    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale',
                 '--encoding=UTF8', '--data-checksums'])
        with (data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport={PORT}\nunix_socket_directories='{root}'\n"
                    "shared_buffers='64MB'\nshared_preload_libraries='stannum'\n"
                    "wal_level=replica\nmax_wal_senders=4\nautovacuum=off\n")
        with (data / 'pg_hba.conf').open('a') as f:
            f.write('local replication all trust\n')
        start()
        sql('CREATE EXTENSION stannum;')
        results = {name: cases[name]() for name in selected}
        print(json.dumps({'status': 'passed', 'cases': results}))
        print('Artifacts:', root)
    finally:
        if (data / 'postmaster.pid').exists():
            stop()


if __name__ == '__main__':
    main()
