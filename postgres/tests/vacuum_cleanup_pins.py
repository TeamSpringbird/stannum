#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Reproduce deferred heap pruning and validate the VACUUM benchmark's drain contract."""
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import tempfile
import time

# The drain contract under test is the VACUUM benchmark's own validator.
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'benchmarks'))
from vacuum_cleanup import validate_cleanup  # noqa: E402


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-vacuum-pins-'))
    data = root / 'data'
    env = dict(os.environ, PGHOST=str(root), PGPORT='28994', PGUSER='pin_probe',
               PGDATABASE='postgres', PGOPTIONS='-c statement_timeout=15000 -c enable_seqscan=off')
    for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(key, None)

    def command(argv, **kwargs):
        return subprocess.run(argv, env=env, text=True, capture_output=True,
                              check=True, timeout=30, **kwargs).stdout.strip()

    def sql(statement):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=statement)

    def layout():
        return json.loads(sql("SELECT json_agg(s) FROM stannum.segment_info('docs_idx') s"))

    def check():
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx',true)") == '0'
        assert sql("SELECT count(*) FROM docs WHERE body ==> 'common'") == '8'
        assert sql("SELECT array_agg(id ORDER BY id) FROM docs WHERE body ==> 'common'") == '{4,8,12,16,20,24,28,32}'

    pin = vacuum = None
    success = False
    try:
        command(['initdb', '-D', str(data), '-U', 'pin_probe', '-A', 'trust', '--no-locale', '--encoding=UTF8'])
        with (data / 'postgresql.conf').open('a') as config:
            config.write(f"\nlisten_addresses=''\nport=28994\nunix_socket_directories='{root}'\n")
        command(['pg_ctl', '-D', str(data), '-l', str(root / 'server.log'), '-w', 'start'])
        sql("""CREATE EXTENSION stannum;
CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);
INSERT INTO docs SELECT n, 'w' || n%97 || ' ' || repeat('common filler ',20) || md5(n::text)
 FROM generate_series(1,32) n;
SET stannum.build_segment_docs=4;
SET stannum.merge_tier_factor=64;
CREATE INDEX docs_idx ON docs USING stannum(body);
DELETE FROM docs WHERE id%4<>0;
ANALYZE docs;""")
        # The cursor starts AFTER deletion: it pins a page, not an old snapshot.
        # The partially filled last page avoids opportunistic pruning on fetch.
        pin = subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], env=env,
                               stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        pin.stdin.write('BEGIN; DECLARE pinned CURSOR FOR SELECT id FROM docs WHERE id=32; FETCH 1 FROM pinned;\n')
        pin.stdin.flush()
        assert select.select([pin.stdout], [], [], 10)[0], 'cursor did not start'
        assert pin.stdout.readline().strip() == '32'
        vacuum = subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c',
                                   'VACUUM (INDEX_CLEANUP ON, VERBOSE) docs;'],
                                  env=dict(env, PGAPPNAME='pin-vacuum'), text=True,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 10
        while vacuum.poll() is None:
            # Once heap scanning has skipped pruning, VACUUM can later need the
            # pin released to remove already-dead line pointers. Do not sleep
            # for an assumed amount of time or terminate the cursor backend.
            wait = sql("SELECT coalesce((SELECT wait_event FROM pg_stat_activity WHERE application_name='pin-vacuum'),'')")
            if wait == 'BufferPin':
                break
            assert time.monotonic() < deadline, 'VACUUM did not reach the pin wait'
            time.sleep(.01)
        pin.communicate('ROLLBACK;\n', timeout=10)
        assert pin.returncode == 0
        pin = None
        stdout, stderr = vacuum.communicate(timeout=20)
        (root / 'vacuum.stderr').write_text(stderr)
        assert vacuum.returncode == 0, stderr
        assert 'not removed due to cleanup lock contention' in stderr, stderr
        after = layout()
        assert sum(s['docs'] for s in after) > 8, after
        assert sum(s['dead_docs'] for s in after) == 0, after
        check()
        sql('VACUUM (INDEX_CLEANUP ON) docs;')
        drained = layout()
        # Sparse tombstones need not trigger a physical segment rewrite.
        assert sum(s['dead_docs'] for s in drained) > 0, drained
        validate_cleanup(drained, 8)
        check()
        print(json.dumps(dict(after_concurrent=after, after_cleanup=drained)))
        success = True
    finally:
        for process in (pin, vacuum):
            if process is not None and process.poll() is None:
                process.terminate()
                process.communicate(timeout=10)
        if (data / 'postmaster.pid').exists():
            command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'])
        if success:
            shutil.rmtree(root)
        else:
            print('Failure artifacts retained at ' + str(root))


if __name__ == '__main__':
    main()
