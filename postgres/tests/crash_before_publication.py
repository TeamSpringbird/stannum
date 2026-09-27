#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Crashes between an operation's page writes and its meta page.

A structural change writes pages, then publishes them on the meta page. A
crash after the page records reached disk and before the meta page's did
must leave an index whose meta page reads as before, with at most pages
nothing references, which VACUUM reclaims. Each case crashes the server at
a race point, after flushing WAL, and checks the recovered index:

- drain: a merge that drained a full pending list of removable runs and
  joined the runs it retired into the list (race point merge:released);
- fold: an insert that folded the write buffer and started it over with
  its own document (insert:buffered);
- vacuum: VACUUM rewriting the write buffer without its dead documents
  (bulk_delete:buffered);
- bound: an insert into a full directory (96 entries) merging its two
  smallest entries without the meta lock before it folds
  (maintenance:built).

One case terminates instead of crashing:

- terminate: a CREATE INDEX terminated (as by pg_terminate_backend) at its
  first merge checkpoint must exit there, with no merge checkpoint after it,
  and leave no index behind.

The crash hook exists only in pg_test builds, which `cargo pgrx test`
installs; run this right after one, in the same hold of the pgrx lock, in a
disposable private cluster:

    script/pgrx-lock.py -- sh -c \\
        'cargo pgrx test pg18 -p stannum && python3 postgres/tests/crash_before_publication.py'

`script/test-all pgrx` runs both. Name cases to run only those (default:
all). Reinstall the release build afterwards (`script/test-all install`).
"""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

ORPHAN = re.compile(r'^warning: page \d+: (run|buffer) page referenced by nothing; VACUUM reclaims it$')


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-crash-'))
    data = root / 'data'
    env = dict(os.environ, PGHOST=str(root), PGPORT='28941', PGUSER='postgres', PGDATABASE='postgres',
               PGOPTIONS='-c enable_seqscan=off -c statement_timeout=60000')
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

    def crash(setup, statement, at):
        """Runs `statement` in a session whose race point `at` crashes the
        server, then waits for crash recovery to finish."""
        marker = f'PANIC:  crash injected at race point {at}'
        crashes = log.read_text().count(marker)
        result = subprocess.run(['psql', '-X', '-qAt'], env=env, text=True, capture_output=True,
                                input=f"{setup}\nSELECT tests.crash_at_race_point('{at}');\n{statement}\n")
        assert result.returncode != 0, ('the crash was not injected', at, result.stdout)
        deadline = time.monotonic() + 60
        while True:
            probe = subprocess.run(['psql', '-X', '-qAt', '-c', 'SELECT 1'], env=env,
                                   text=True, capture_output=True)
            if probe.returncode == 0:
                break
            assert time.monotonic() < deadline, ('recovery did not finish', probe.stderr)
            time.sleep(.2)
        assert log.read_text().count(marker) > crashes, (at, 'no PANIC in the server log')

    def drain():
        """A merge drains a full list of removable pending runs, joins the
        runs it retires into the list, and crashes before the meta page."""
        tuned = ('SET stannum.write_buffer_docs=1; SET stannum.merge_tier_factor=2; '
                 'SET stannum.max_merge_docs=1000000; SET stannum.deferred_merge_docs=0;')
        sql('CREATE TABLE drained(id int, body text); CREATE INDEX drained_idx ON drained USING stannum(body);')
        # A held snapshot keeps every retired run pending until the list is full.
        holder = subprocess.Popen(['psql', '-X', '-qAt'], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL, text=True,
                                  env=dict(env, PGAPPNAME='stannum-crash-holder'))
        holder.stdin.write('BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM drained; '
                           'SELECT pg_sleep(600);\n')
        holder.stdin.flush()
        deadline = time.monotonic() + 10
        while sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='stannum-crash-holder' "
                  "AND wait_event='PgSleep';") != '1':
            assert time.monotonic() < deadline, 'snapshot holder did not start'
            time.sleep(.05)
        sql(tuned + '\n' + '\n'.join(f"INSERT INTO drained VALUES ({n}, 'needle w{n}');" for n in range(1, 121)))
        assert sql("SELECT tests.pending_entries('drained_idx'::regclass::oid);") == '48'
        sql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name='stannum-crash-holder';")
        holder.wait(timeout=10)
        assert_clean('drained_idx')
        crash(tuned, "INSERT INTO drained VALUES (121, 'needle w121');", 'merge:released')
        assert sql("SELECT tests.pending_entries('drained_idx'::regclass::oid);") == '48'
        orphans = assert_only_orphans('drained_idx')
        sql(tuned + "INSERT INTO drained VALUES (121, 'needle w121');")
        assert int(sql("SELECT tests.pending_entries('drained_idx'::regclass::oid);")) < 48
        sql('VACUUM (INDEX_CLEANUP ON) drained;')
        assert_clean('drained_idx')
        assert sql("SELECT count(*) FROM drained WHERE body ==> 'needle';") == '121'
        for n in (1, 60, 120, 121):
            assert sql(f"SELECT string_agg(id::text, ',') FROM drained WHERE body ==> 'w{n}';") == str(n), n
        return orphans

    def fold():
        """An insert folds the buffer, starts it over with its own document
        and crashes before the meta page."""
        tuned = 'SET stannum.write_buffer_docs=2; SET stannum.max_merge_docs=0; SET stannum.deferred_merge_docs=0;'
        sql('CREATE TABLE folded(id int, body text); CREATE INDEX folded_idx ON folded USING stannum(body);')
        sql(tuned + "INSERT INTO folded VALUES (1, 'needle one'); INSERT INTO folded VALUES (2, 'needle two');")
        crash(tuned, "INSERT INTO folded VALUES (3, 'needle three');", 'insert:buffered')
        orphans = assert_only_orphans('folded_idx')
        assert sql("SELECT string_agg(id::text, ',' ORDER BY id) FROM folded WHERE body ==> 'needle';") == '1,2'
        sql(tuned + "INSERT INTO folded VALUES (3, 'needle three'), (4, 'needle four');")
        sql('VACUUM (INDEX_CLEANUP ON) folded;')
        assert_clean('folded_idx')
        assert sql("SELECT string_agg(id::text, ',' ORDER BY id) FROM folded WHERE body ==> 'needle';") == '1,2,3,4'
        return orphans

    def vacuum():
        """VACUUM rewrites a buffer of several pages without its dead half
        and crashes before the meta page."""
        sql("CREATE TABLE vacuumed(id int, body text); CREATE INDEX vacuumed_idx ON vacuumed USING stannum(body);"
            "SET stannum.write_buffer_docs=1000;"
            "INSERT INTO vacuumed SELECT n, 'needle w' || n || repeat(' filler', 20) FROM generate_series(1, 400) n;")
        dead = sql("WITH gone AS (DELETE FROM vacuumed WHERE id % 2 = 0 RETURNING ctid) "
                   "SELECT array_agg(ctid::text) FROM gone;")
        crash('', f"SELECT tests.direct_bulk_delete('vacuumed_idx'::regclass::oid, '{dead}');",
              'bulk_delete:buffered')
        orphans = assert_only_orphans('vacuumed_idx')
        assert sql("SELECT sum(docs) FROM stannum.segment_info('vacuumed_idx');") == '400'
        assert sql("SELECT count(*) FROM vacuumed WHERE body ==> 'needle';") == '200'
        sql('VACUUM (INDEX_CLEANUP ON) vacuumed;')
        assert_clean('vacuumed_idx')
        assert sql("SELECT sum(docs) FROM stannum.segment_info('vacuumed_idx');") == '200'
        assert sql("SELECT count(*) FROM vacuumed WHERE body ==> 'needle';") == '200'
        return orphans

    def bound():
        """An insert into a full directory merges its two smallest entries
        without the meta lock and crashes before publishing the merge."""
        tuned = 'SET stannum.write_buffer_docs=1; SET stannum.max_merge_docs=0; SET stannum.deferred_merge_docs=0;'
        entries = "SELECT count(*) FROM stannum.segment_info('bounded_idx') WHERE kind = 'immutable';"
        sql('CREATE TABLE bounded(id int, body text); CREATE INDEX bounded_idx ON bounded USING stannum(body);')
        # One segment per insert: 96 entries, the on-disk bound, and one
        # document buffered, so the next fold must make room first.
        sql(tuned + "INSERT INTO bounded SELECT n, 'needle w' || n FROM generate_series(1, 97) n;")
        assert sql(entries) == '96'
        crash(tuned, "INSERT INTO bounded VALUES (98, 'needle w98');", 'maintenance:built')
        orphans = assert_only_orphans('bounded_idx')
        assert orphans > 0, 'the merged run was not written before the crash'
        assert sql(entries) == '96'
        assert sql("SELECT count(*) FROM bounded WHERE body ==> 'needle';") == '97'
        sql(tuned + "INSERT INTO bounded VALUES (98, 'needle w98');")
        assert sql(entries) == '96'
        sql('VACUUM (INDEX_CLEANUP ON) bounded;')
        assert_clean('bounded_idx')
        assert sql("SELECT count(*) FROM bounded WHERE body ==> 'needle';") == '98'
        for n in (1, 2, 97, 98):
            assert sql(f"SELECT string_agg(id::text, ',') FROM bounded WHERE body ==> 'w{n}';") == str(n), n
        return orphans

    def terminate():
        """A CREATE INDEX terminated at its first merge checkpoint exits
        there instead of finishing the merges of its build."""
        sql("CREATE TABLE terminated(id int, body text);"
            "INSERT INTO terminated SELECT n, (SELECT string_agg('w' || (n * k % 997), ' ') "
            "FROM generate_series(1, 20) k) FROM generate_series(1, 400) n;")
        marker = 'passed after termination'
        restarts = 'all server processes terminated'
        before = log.read_text()
        # Twenty build segments, no tier that fills: the only merge is the
        # compaction that ends the build.
        result = subprocess.run(
            ['psql', '-X', '-qAt'], env=env, text=True, capture_output=True,
            input="SET stannum.build_segment_docs=20; SET stannum.merge_tier_factor=64;\n"
                  "SELECT tests.terminate_at_race_point(ARRAY['merge:checkpoint', 'maintenance:checkpoint']);\n"
                  "CREATE INDEX terminated_idx ON terminated USING stannum(body);\n")
        assert result.returncode != 0, ('the build was not terminated', result.stdout)
        assert 'terminating connection due to administrator command' in result.stderr, result.stderr
        after = log.read_text()
        assert after.count(marker) == before.count(marker), 'merge checkpoints ran after the termination'
        assert after.count(restarts) == before.count(restarts), 'the server restarted'
        assert sql("SELECT to_regclass('terminated_idx') IS NULL;") == 't'
        sql('SET stannum.build_segment_docs=20; SET stannum.merge_tier_factor=64;'
            'CREATE INDEX terminated_idx ON terminated USING stannum(body);')
        assert_clean('terminated_idx')
        assert sql("SELECT count(*) FROM stannum.segment_info('terminated_idx');") == '1'
        return 0

    cases = {'drain': drain, 'fold': fold, 'vacuum': vacuum, 'bound': bound, 'terminate': terminate}
    selected = sys.argv[1:] or list(cases)
    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale',
                 '--encoding=UTF8', '--data-checksums'])
        with (data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport=28941\nunix_socket_directories='{root}'\n"
                    "shared_buffers='64MB'\nrestart_after_crash=on\n")
        command(['pg_ctl', '-D', str(data), '-l', str(log), '-w', 'start'])
        sql('CREATE EXTENSION stannum;')
        results = {name: {'orphans_after_crash': cases[name]()} for name in selected}
        print(json.dumps({'status': 'passed', 'crashes': len(results), 'cases': results}))
        print('Artifacts:', root)
    finally:
        if (data / 'postmaster.pid').exists():
            command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'])


if __name__ == '__main__':
    main()
