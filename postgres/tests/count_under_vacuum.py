#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""A count whose index view lists a row VACUUM removes meanwhile.

A count captures its index view, then reads the visibility map. If VACUUM
removes a row the view still lists (publishing a segment's dead list or a
rewritten write buffer, freeing the line pointer and marking the page
all-visible) between the two, the count must not take the page's
all-visible bit to cover that row. Each case deletes one row, pauses a
count at its `count:view` race point with its view captured, runs VACUUM
until the row's page is all-visible, releases the count, and compares its
answer with the same predicate counted from the heap.

VACUUM may remove the row only if no snapshot older than the delete
exists anywhere on the server, since a snapshot's xmin covers every
database. The test therefore runs in a private cluster where nothing else
runs, so the removal happens exactly when the test runs VACUUM, with no
waiting on other sessions.

The pause hook exists only in pg_test builds, which `cargo pgrx test`
installs; run this right after one, in the same hold of the pgrx lock:

    script/pgrx-lock.py -- sh -c \\
        'cargo pgrx test pg18 -p stannum && python3 postgres/tests/count_under_vacuum.py'

`script/test-all pgrx` runs both. Reinstall the release build afterwards
(`script/test-all install`).
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

# The advisory lock a paused count waits on (tests::COUNT_RACE_LOCK).
COUNT_RACE_LOCK = 0x5354_4E43

# (deleted row, ==> query, equivalent SQL predicate, setting). Rows 1-400
# are in a segment, 1001-1060 in the write buffer; every deleted row
# matches its query.
CASES = [
    (2, 'alpha', 'true', 'stannum.count_fold = off'),
    (4, '"alpha beta"', "body = 'alpha beta'", ''),
    (6, '"alpha beta"', "body = 'alpha beta'", 'stannum.force_count_pages = on'),
    (8, 'alpha AND NOT gamma', "body = 'alpha beta'", ''),
    (10, 'alph*', 'true', ''),
    (1002, '"alpha beta"', "body = 'alpha beta'", ''),
    (1004, 'alpha', 'true', 'stannum.count_fold = off'),
    (1006, 'alpha AND NOT gamma', "body = 'alpha beta'", 'stannum.force_count_pages = on'),
    # The fold already checks its view after reading the map.
    (12, 'alpha', 'true', 'stannum.count_fold = on'),
]


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-count-vacuum-'))
    data = root / 'data'
    env = dict(os.environ, PGHOST=str(root), PGPORT='28942', PGUSER='postgres', PGDATABASE='postgres',
               PGOPTIONS='-c statement_timeout=60000')
    for name in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(name, None)
    log = root / 'server.log'

    def command(args, **kw):
        try:
            return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kw)
        except subprocess.CalledProcessError as error:
            raise AssertionError(error.output) from error

    def sql(text):
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=env).strip()

    def session():
        """An interactive psql whose output can be read line by line."""
        return subprocess.Popen(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], stdin=subprocess.PIPE,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)

    def ask(process, text):
        """Runs `text` in the session and returns its non-empty output lines,
        read up to a sentinel (void results print empty lines)."""
        process.stdin.write(f"{text}\nSELECT 'stannum-done';\n")
        process.stdin.flush()
        lines = []
        while (line := process.stdout.readline()) != 'stannum-done\n':
            assert line, (text, process.stderr.read() if process.poll() is not None else 'no answer')
            if line.strip():
                lines.append(line.strip())
        return lines

    def settings_for(setting):
        return f'SET {setting};' if setting else ''

    def run_case(row, query, predicate, setting):
        block = sql(f'SELECT (ctid::text::point)[0]::bigint FROM count_race WHERE id = {row};')
        deleter = int(sql(f'BEGIN; DELETE FROM count_race WHERE id = {row}; SELECT txid_current(); COMMIT;'))
        # Nothing else runs in this cluster, so no snapshot can still see the
        # row: VACUUM may remove it under the count's snapshot.
        assert sql(f'SELECT pg_snapshot_xmin(pg_current_snapshot())::text::bigint > {deleter};') == 't', \
            (row, 'a transaction older than the delete is still running')
        count = f"SELECT count(*) FROM count_race WHERE body ==> '{query}'"
        plan = sql(f'SET enable_seqscan = off; {settings_for(setting)} EXPLAIN {count};')
        assert 'Stannum Count' in plan, (query, setting, plan)

        locker = session()
        counter = session()
        try:
            ask(locker, f"SELECT pg_advisory_lock({COUNT_RACE_LOCK});")
            [pid] = ask(counter, 'SELECT pg_backend_pid();')
            ask(counter, f"SELECT tests.count_race_pause(); SET enable_seqscan = off; {settings_for(setting)}")
            # The first statement fixes the snapshot, after the delete; the
            # count then stops at count:view with its view captured.
            counter.stdin.write('BEGIN ISOLATION LEVEL REPEATABLE READ;\n'
                                f'SELECT count(*) FROM count_race WHERE {predicate};\n'
                                f'{count};\nCOMMIT;\n')
            counter.stdin.flush()
            expected = counter.stdout.readline().strip()
            assert expected, (row, 'the reference count failed', counter.stderr.read())
            waiting = (f"SELECT count(*) FROM pg_locks WHERE pid = {pid} "
                       "AND locktype = 'advisory' AND NOT granted;")
            deadline = time.monotonic() + 30
            while sql(waiting) != '1':
                assert counter.poll() is None, (row, 'the count finished without reaching count:view')
                assert time.monotonic() < deadline, (row, 'the count did not reach count:view')
                time.sleep(.01)
            # With no other session, VACUUM removes the row and marks its
            # page all-visible at once; a retry covers only a transient pin.
            for attempt in range(5):
                sql('VACUUM (INDEX_CLEANUP ON) count_race;')
                if sql(f"SELECT tests.count_race_all_visible('count_race'::regclass, {block});") == 't':
                    break
            else:
                raise AssertionError((row, f'VACUUM did not mark block {block} all-visible'))
            ask(locker, f"SELECT pg_advisory_unlock({COUNT_RACE_LOCK});")
            counted = counter.stdout.readline().strip()
            assert counted, (row, 'the count failed', counter.stderr.read())
        finally:
            for process in (locker, counter):
                if process.poll() is None:
                    process.stdin.close()
                    process.wait(timeout=30)
        return int(expected), int(counted)

    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale',
                 '--encoding=UTF8', '--data-checksums'])
        with (data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport=28942\nunix_socket_directories='{root}'\n"
                    "shared_buffers='64MB'\nautovacuum=off\n")
        command(['pg_ctl', '-D', str(data), '-l', str(log), '-w', 'start'])
        sql('CREATE EXTENSION stannum;')
        sql("""CREATE TABLE count_race(id int PRIMARY KEY, body text) WITH (autovacuum_enabled = off);
               INSERT INTO count_race SELECT n,
                   CASE WHEN n % 2 = 0 THEN 'alpha beta' ELSE 'alpha gamma' END
                   FROM generate_series(1, 400) n;
               CREATE INDEX count_race_idx ON count_race USING stannum(body);
               INSERT INTO count_race SELECT n,
                   CASE WHEN n % 2 = 0 THEN 'alpha beta' ELSE 'alpha gamma' END
                   FROM generate_series(1001, 1060) n;
               VACUUM count_race;""")
        assert sql("SELECT count(*) FROM stannum.segment_info('count_race_idx') "
                   "WHERE kind = 'mutable' AND docs = 60;") == '1', 'rows 1001-1060 are in the write buffer'
        wrong = []
        for row, query, predicate, setting in CASES:
            expected, counted = run_case(row, query, predicate, setting)
            if expected != counted:
                wrong.append(f'row {row}, {query} ({setting or "defaults"}): '
                             f'counted {counted}, expected {expected}')
        assert not wrong, '\n'.join(wrong)
        print(json.dumps({'status': 'passed', 'cases': len(CASES)}))
    finally:
        if (data / 'postmaster.pid').exists():
            command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'])


if __name__ == '__main__':
    main()
