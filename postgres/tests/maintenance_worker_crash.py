#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""A maintenance worker that crashes or is terminated in the middle of a merge.

A worker builds a merge without the index's metadata lock, writes the merged
run, and only then publishes it. Each case makes the worker stop at a race
point through `stannum.debug_maintenance_race` (a reload setting of pg_test
builds), and checks that the index the worker leaves behind is consistent:

- crash: the worker crashes the server (PANIC after flushing WAL) with the
  merged run written and not published (race point maintenance:built).
  After crash recovery the directory is as before, the run's pages are
  orphans that `verify_index` reports as warnings, a new worker merges once
  the setting is gone, and VACUUM reclaims the orphans;
- terminate: the worker is terminated, as by pg_terminate_backend, at its
  first merge checkpoint (maintenance:checkpoint). The server keeps
  running, the worker's job is abandoned and its slot freed, and the next
  fold's job merges.

The hook exists only in pg_test builds, which `cargo pgrx test` installs;
run this right after one, in the same hold of the pgrx lock, as
`script/test-all pgrx` does. It runs in a disposable private cluster.
Reinstall the release build afterwards (`script/test-all install`).

    python3 postgres/tests/maintenance_worker_crash.py [case...]
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

PORT = 28971
DEADLINE = 60
ORPHAN = re.compile(r'^warning: page \d+: (run|buffer) page referenced by nothing; VACUUM reclaims it$')


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-worker-crash-'))
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

    def sql(text):
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=env).strip()

    def try_sql(text):
        result = subprocess.run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=env,
                                text=True, capture_output=True)
        return result.stdout.strip() if result.returncode == 0 else None

    def wait_for(description, probe, deadline=DEADLINE):
        end = time.monotonic() + deadline
        while True:
            value = probe()
            if value:
                return value
            assert time.monotonic() < end, f'timed out waiting for {description}'
            time.sleep(.1)

    def status():
        return json.loads(sql('SELECT row_to_json(s) FROM stannum.maintenance_status() s;'))

    def idle():
        s = status()
        return s['queued_jobs'] == 0 and s['running_jobs'] == 0 and s['worker_pid'] is None

    def launcher_up():
        # pg_stat_activity, not maintenance_status(): the extension may not
        # exist yet.
        return try_sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'stannum maintenance launcher';") == '1'

    def segments(index):
        return int(sql(f"SELECT count(*) FROM stannum.segment_info('{index}') WHERE kind = 'immutable';"))

    def findings(index):
        rows = sql(f"SELECT severity || ': ' || location || ': ' || message "
                   f"FROM stannum.verify_index('{index}', true);")
        return [row for row in rows.splitlines() if row]

    def assert_only_orphans(index):
        rows = findings(index)
        assert all(ORPHAN.match(row) for row in rows), (index, rows)
        return len(rows)

    def assert_clean(index):
        rows = findings(index)
        assert rows == [], (index, rows)

    def assert_matches_heap(table):
        assert sql(f"SELECT count(*) FROM {table} WHERE body ==> 'needle';") == sql(f'SELECT count(*) FROM {table};')

    def race(setting):
        """Sets stannum.debug_maintenance_race for workers started from now on."""
        if setting:
            sql(f"ALTER SYSTEM SET stannum.debug_maintenance_race = '{setting}'; SELECT pg_reload_conf();")
        else:
            sql('ALTER SYSTEM RESET stannum.debug_maintenance_race; SELECT pg_reload_conf();')
        # A new session forks from a postmaster that has reloaded, as a worker does.
        wait_for('the reload', lambda: sql('SHOW stannum.debug_maintenance_race;') == setting)

    def prepare(table):
        """Three segments and four buffered rows, all written in manual mode,
        so no job is queued yet; the next background insert folds and queues
        a merge."""
        sql(f'CREATE TABLE {table}(id int, body text);'
            f'CREATE INDEX {table}_idx ON {table} USING stannum(body) WITH (target_segment_count = 2);')
        statements = '\n'.join(f"INSERT INTO {table} VALUES ({n}, 'needle w{n}');" for n in range(1, 17))
        sql(f'SET stannum.write_buffer_docs = 4; SET stannum.index_maintenance_mode = manual;\n{statements}')
        assert segments(f'{table}_idx') == 3

    def fold(table, first):
        """Four inserts in background mode; the last folds and queues a job."""
        statements = '\n'.join(f"INSERT INTO {table} VALUES ({n}, 'needle w{n}');" for n in range(first, first + 4))
        return subprocess.run(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], env=env, text=True,
                              capture_output=True, input=f'SET stannum.write_buffer_docs = 4;\n{statements}\n')

    def crash():
        marker = 'crash injected at race point maintenance:built in a maintenance worker'
        restarts = log.read_text().count('all server processes terminated')
        prepare('crashed')
        race('crash:maintenance:built')
        # The insert that queues the job may commit before the crash or be
        # lost with it; the index must agree with the heap either way.
        fold('crashed', 17)
        wait_for('the worker crash', lambda: marker in log.read_text())
        wait_for('the restart', lambda: log.read_text().count('all server processes terminated') > restarts)
        wait_for('crash recovery', launcher_up)
        race('')
        orphans = assert_only_orphans('crashed_idx')
        assert orphans > 0, 'the merged run was not written before the crash'
        # Three segments, or four if the insert that queued the job committed:
        # either way not the two the unpublished merge would have left.
        assert segments('crashed_idx') in (3, 4), segments('crashed_idx')
        assert_matches_heap('crashed')
        result = fold('crashed', 101)
        assert result.returncode == 0, result.stderr
        wait_for('the worker to merge crashed_idx', lambda: segments('crashed_idx') <= 2 and idle())
        assert_only_orphans('crashed_idx')
        sql('VACUUM (INDEX_CLEANUP ON) crashed;')
        assert_clean('crashed_idx')
        assert_matches_heap('crashed')
        for n in (1, 16, 101, 104):
            assert sql(f"SELECT string_agg(id::text, ',') FROM crashed WHERE body ==> 'w{n}';") == str(n), n
        return {'orphans_after_crash': orphans}

    def terminate():
        restarts = log.read_text().count('all server processes terminated')
        prepare('terminated')
        race('terminate:maintenance:checkpoint')
        before = status()
        result = fold('terminated', 17)
        assert result.returncode == 0, result.stderr
        wait_for('the terminated job to be abandoned',
                 lambda: status()['abandoned'] > before['abandoned'] and idle())
        assert 'maintenance worker terminating itself at race point maintenance:checkpoint' in log.read_text()
        assert log.read_text().count('all server processes terminated') == restarts, 'the server restarted'
        race('')
        orphans = assert_only_orphans('terminated_idx')
        assert segments('terminated_idx') == 4
        assert_matches_heap('terminated')
        result = fold('terminated', 101)
        assert result.returncode == 0, result.stderr
        wait_for('the worker to merge terminated_idx', lambda: segments('terminated_idx') <= 2 and idle())
        sql('VACUUM (INDEX_CLEANUP ON) terminated;')
        assert_clean('terminated_idx')
        assert_matches_heap('terminated')
        return {'orphans_after_termination': orphans}

    cases = {'crash': crash, 'terminate': terminate}
    selected = sys.argv[1:] or list(cases)
    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale',
                 '--encoding=UTF8', '--data-checksums'])
        with (data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport={PORT}\nunix_socket_directories='{root}'\n"
                    "shared_buffers='64MB'\nshared_preload_libraries='stannum'\n"
                    "restart_after_crash=on\nautovacuum=off\n")
        command(['pg_ctl', '-D', str(data), '-l', str(log), '-w', 'start'])
        wait_for('the launcher', launcher_up)
        sql('CREATE EXTENSION stannum;')
        results = {name: cases[name]() for name in selected}
        print(json.dumps({'status': 'passed', 'cases': results}))
        print('Artifacts:', root)
    finally:
        if (data / 'postmaster.pid').exists():
            command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'])


if __name__ == '__main__':
    main()
