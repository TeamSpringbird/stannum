#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Deferred-merge checks in a private PG18 cluster; install a release build first.

Run under /tmp/stannum-pgrx-lock.py on a shared development machine.
No preload library or extra Python dependencies are needed.
"""
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    root = Path(tempfile.mkdtemp(prefix="stannum-merge-"))
    data = root / "data"
    env = dict(os.environ, PGHOST=str(root), PGPORT="28929", PGUSER="postgres",
               PGDATABASE="postgres", PGOPTIONS="-c statement_timeout=60000")
    for name in ("PGSERVICE", "PGSERVICEFILE", "PGPASSWORD"):
        env.pop(name, None)

    def command(args, **kwargs):
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kwargs)

    def sql(query):
        return command(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"], input=query, env=env).strip()

    def verify():
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx', true) WHERE severity='error'") == "0"
        assert sql("""SET enable_seqscan=off;
            WITH actual AS MATERIALIZED (SELECT id FROM docs WHERE body ==> 'needle'),
            expected AS MATERIALIZED (SELECT id FROM docs WHERE body ~ '\\mneedle\\M'),
            delta AS ((SELECT * FROM actual EXCEPT SELECT * FROM expected)
                   UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM actual))
            SELECT count(*) FROM delta""") == "0"

    def start():
        command(["pg_ctl", "-D", str(data), "-l", str(root / "server.log"), "-w", "start"])

    started = False
    try:
        command(["initdb", "-D", str(data), "-U", "postgres", "-A", "trust",
                 "--no-locale", "--encoding=UTF8", "--data-checksums"])
        with (data / "postgresql.conf").open("a") as handle:
            handle.write(f"\nlisten_addresses=''\nport=28929\nunix_socket_directories='{root}'\n"
                         "shared_buffers='64MB'\nautovacuum_naptime='1s'\n"
                         "log_autovacuum_min_duration=0\nshared_preload_libraries=''\n")
        start()
        started = True
        sql("""CREATE EXTENSION stannum;
            CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (
                vacuum_index_cleanup=auto,
                autovacuum_vacuum_insert_threshold=8,
                autovacuum_vacuum_insert_scale_factor=0);
            CREATE INDEX docs_idx ON docs USING stannum(body);
            SET stannum.write_buffer_docs=1;
            SET stannum.merge_tier_factor=8;
            SET stannum.max_merge_docs=0;
            INSERT INTO docs SELECT n, 'needle common' FROM generate_series(1,65) n;""")
        deadline = time.monotonic() + 30
        while True:
            count = int(sql("SELECT count(*) FROM stannum.segment_info('docs_idx') WHERE kind='immutable'"))
            vacuums = int(sql("SELECT autovacuum_count FROM pg_stat_user_tables WHERE relname='docs'"))
            if count < 64 and vacuums > 0:
                break
            assert time.monotonic() < deadline, ("insert-only autovacuum did not merge", count, vacuums, root)
            time.sleep(.2)
        verify()
        print(f"insert-only autovacuum: {vacuums} run(s), {count} segments; AUTO cleanup, no preload")
        sql("""ALTER TABLE docs SET (autovacuum_enabled=false);
            SET stannum.write_buffer_docs=32;
            SET stannum.max_merge_docs=0;
            INSERT INTO docs SELECT n, 'needle ' || repeat('common filler ', 50)
            FROM generate_series(66,2065) n;""")
        # Keep an old scan open across merge publication. Its reader retains
        # the old directory; retired runs cannot be reclaimed beneath it.
        reader = subprocess.Popen(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1"], env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, bufsize=1)
        reader.stdin.write("BEGIN ISOLATION LEVEL REPEATABLE READ; SET enable_seqscan=off; "
                           "SET stannum.enable_custom_scan=on; DECLARE old_scan CURSOR FOR "
                           "SELECT id FROM docs WHERE body ==> 'needle'; FETCH 1 FROM old_scan;\n")
        reader.stdin.flush()
        first = reader.stdout.readline().strip()
        assert first.isdigit(), first
        vacuum = subprocess.Popen(["psql", "-XqAt", "-v", "ON_ERROR_STOP=1", "-c",
                                   "VACUUM (INDEX_CLEANUP ON) docs"], env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        # Force overflow merges at a low soft bound, within a budget that
        # covers them, so an insert retires the inputs VACUUM captured.
        sql("""SET stannum.write_buffer_docs=1; SET stannum.max_merge_docs=1000000;
            SET stannum.max_segments=4;
            INSERT INTO docs SELECT n, 'needle fresh' FROM generate_series(2066,2100) n;""")
        _, error = vacuum.communicate(timeout=60)
        assert vacuum.returncode == 0, error
        sql("VACUUM (INDEX_CLEANUP ON) docs")
        reader.stdin.write("FETCH ALL FROM old_scan; COMMIT;\n")
        reader.stdin.close()
        rest = reader.stdout.read().splitlines()
        error = reader.stderr.read()
        assert reader.wait(timeout=60) == 0, error
        ids = [int(first)] + [int(line) for line in rest]
        assert len(ids) == len(set(ids)) == 2065, len(ids)
        verify()
        command(["pg_ctl", "-D", str(data), "-m", "immediate", "-w", "stop"])
        started = False
        start()
        started = True
        verify()
        print(f"concurrent maintenance, retained scan, WAL recovery and verification passed: {root}")
        orphans = crash_between_run_write_and_publication(sql, command, env, data, root, start)
        command(["pg_ctl", "-D", str(data), "-m", "fast", "-w", "stop"])
        started = False
        start()
        started = True
        verify()
        print(f"crash between run write and publication: {orphans} orphaned page(s) "
              "reported by verify_index, reclaimed by VACUUM and reused by the next fold")
    finally:
        if started or (data / "postmaster.pid").exists():
            command(["pg_ctl", "-D", str(data), "-m", "fast", "-w", "stop"])


def crash_between_run_write_and_publication(sql, command, env, data, root, start):
    """Buffers a large fold, stops the server immediately while the fold is
    writing its run, and checks that verify_index reports the run's pages as
    orphans, VACUUM reclaims them into the FSM and the next fold reuses them.
    The kill is timed on relation growth; a kill that lands before the run
    write starts leaves the buffer intact, so the attempt simply repeats.
    Returns the number of orphaned pages reported."""
    page_warnings = ("SELECT count(*) FROM stannum.verify_index('docs_idx') "
                     "WHERE severity = 'warning' AND location LIKE 'page %'")
    sql("ALTER TABLE docs SET (autovacuum_enabled=false); CREATE EXTENSION IF NOT EXISTS pg_freespacemap;")
    for attempt in range(5):
        buffered = int(sql("SELECT coalesce(sum(docs), 0) FROM stannum.segment_info('docs_idx') WHERE kind = 'mutable'"))
        if buffered < 40000:
            # The buffer holds at most a few documents from the steps above
            # (or nothing, after an attempt whose fold was published).
            sql("""SET stannum.write_buffer_docs=100000; SET stannum.write_buffer_bytes=67108864;
                INSERT INTO docs SELECT n, 'orphan ' || repeat(md5(n::text) || ' ', 30)
                FROM generate_series(100000 + 50000 * %d, 100000 + 50000 * %d + 39999) n;""" % (attempt, attempt))
        size = int(sql("SELECT pg_relation_size('docs_idx')"))
        # The next insert folds the buffer before appending. Watch the relation
        # grow as the run is written, then stop the server at once.
        folder = subprocess.Popen(["psql", "-XqAt", "-c",
                                   "SET stannum.write_buffer_docs=1; SET stannum.max_merge_docs=0; "
                                   "INSERT INTO docs VALUES (%d, 'orphan')" % (99000 + attempt)],
                                  env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        watcher = subprocess.Popen(["psql", "-XqAt"], env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1)
        deadline = time.monotonic() + 120
        grown = False
        while time.monotonic() < deadline and folder.poll() is None:
            watcher.stdin.write("SELECT pg_relation_size('docs_idx');\n")
            watcher.stdin.flush()
            line = watcher.stdout.readline().strip()
            if line.isdigit() and int(line) >= size + 32 * 8192:
                grown = True
                break
        watcher.kill()
        command(["pg_ctl", "-D", str(data), "-m", "immediate", "-w", "stop"])
        folder.wait()
        start()
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx') WHERE severity = 'error'") == "0"
        orphans = int(sql(page_warnings))
        if orphans == 0:
            # Killed before the run write began (or after publication): retry.
            continue
        assert grown, "orphans without observed growth"
        free_before = int(sql("SELECT count(*) FROM pg_freespace('docs_idx') WHERE avail > 0"))
        sql("VACUUM (INDEX_CLEANUP ON) docs")
        assert sql(page_warnings) == "0"
        assert sql("SELECT count(*) FROM stannum.verify_index('docs_idx', true) WHERE severity = 'error'") == "0"
        free_after = int(sql("SELECT count(*) FROM pg_freespace('docs_idx') WHERE avail > 0"))
        assert free_after >= free_before + orphans, (free_before, free_after, orphans)
        # The interrupted fold repeats and takes the reclaimed pages first.
        sql("SET stannum.write_buffer_docs=1; SET stannum.max_merge_docs=0; INSERT INTO docs VALUES (98000, 'orphan')")
        assert int(sql("SELECT count(*) FROM pg_freespace('docs_idx') WHERE avail > 0")) < free_after
        assert sql(page_warnings) == "0"
        return orphans
    raise AssertionError("no crash attempt interrupted a run write: %s" % root)


if __name__ == "__main__":
    main()
