#!/usr/bin/env python3
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
        # Force an emergency merge that can invalidate VACUUM's captured inputs.
        sql("""SET stannum.write_buffer_docs=1; SET stannum.max_merge_docs=0;
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
    finally:
        if started:
            command(["pg_ctl", "-D", str(data), "-m", "fast", "-w", "stop"])


if __name__ == "__main__":
    main()
