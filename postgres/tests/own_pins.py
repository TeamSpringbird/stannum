#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Own pins (`stannum.own_pins`) against the buffer header lock and under load.

A run page found through its hint is pinned and released outside
PostgreSQL's per-pin bookkeeping, on the buffer header's state word. Whoever
holds the header's spinlock (the bgwriter's and a checkpoint's scans, a
clock sweep, `pg_buffercache`) reads the state under the lock and writes it
back at the unlock, so a release that lands under the lock as a plain atomic
decrement is undone by the write-back: a pin nobody holds, which keeps the
buffer from eviction and makes `DROP` or `TRUNCATE` of the relation spin in
`InvalidateBuffer` forever. PostgreSQL's `UnpinBuffer` waits for the lock.

Two checks, in a disposable private cluster running the pg_test build (the
hooks `tests.own_pin_held`, `tests.buffer_header_locked` and
`tests.buffer_refcount` exist only there):

- header lock: one session holds an own pin on a run page while another
  holds the page's buffer header lock across the release; the reference
  count must be zero once both return;
- load: walks with own pins beside sessions reading `pg_buffercache` (a
  header lock per buffer per read), VACUUMs and REINDEXes, with walkers
  cancelled and terminated; afterwards no buffer may stay pinned, the server
  log must show no crash, and the index must drop within a timeout.

Run right after `cargo pgrx test`, under the same hold of the pgrx lock
(`script/test-all pgrx` does), with the server's binaries on PATH and
`pg_buffercache` installed with it.
"""
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time

PORT = '28995'
CRASH = re.compile(r'terminated by signal|was terminated|reinitializing|TRAP:|PANIC:|'
                   r'all server processes terminated')
# Whatever the walkers' cancellation and termination report, and nothing else,
# may be logged above LOG.
EXPECTED = re.compile(r'canceling statement due to user request|'
                      r'terminating connection due to administrator command|'
                      r'unexpected EOF on client connection|'
                      r'could not send data to client|'
                      r'canceling statement due to statement timeout')
SEVERE = re.compile(r'\b(WARNING|ERROR|FATAL|PANIC):')


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-own-pins-'))
    data = root / 'data'
    log = root / 'server.log'
    env = dict(os.environ, PGHOST=str(root), PGPORT=PORT, PGUSER='pin_probe', PGDATABASE='postgres')
    for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD', 'PGOPTIONS'):
        env.pop(key, None)

    def command(argv, timeout=60, **kwargs):
        return subprocess.run(argv, env=env, text=True, capture_output=True,
                              check=True, timeout=timeout, **kwargs).stdout.strip()

    def sql(statement, timeout=60):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], timeout=timeout, input=statement)

    def session(statement, name):
        """A psql running `statement` in the background, named for pg_stat_activity."""
        return subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', statement],
                                env=dict(env, PGAPPNAME=name), text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def log_problems():
        lines = log.read_text().splitlines()
        return [line for line in lines
                if CRASH.search(line) or (SEVERE.search(line) and not EXPECTED.search(line))]

    processes = []
    success = False
    try:
        command(['initdb', '-D', str(data), '-U', 'pin_probe', '-A', 'trust', '--no-locale',
                 '--encoding=UTF8'])
        with (data / 'postgresql.conf').open('a') as config:
            config.write(f"\nlisten_addresses=''\nport={PORT}\nunix_socket_directories='{root}'\n"
                         "shared_buffers=64MB\nlog_min_messages=warning\n")
        command(['pg_ctl', '-D', str(data), '-l', str(log), '-w', 'start'])
        sql("""CREATE EXTENSION stannum;
CREATE EXTENSION pg_buffercache;
CREATE TABLE docs(id int PRIMARY KEY, body text) WITH (autovacuum_enabled=false);
INSERT INTO docs SELECT n, (SELECT string_agg('w' || ((random() * 400)::int), ' ')
                            FROM generate_series(1, 12))
 FROM generate_series(1, 40000) n;
CREATE INDEX docs_idx ON docs USING stannum(body);
VACUUM ANALYZE docs;""", timeout=300)
        # The library, loaded by the first call, defines the setting.
        setting = sql("SELECT stannum.version(); SELECT current_setting('stannum.own_pins')")
        assert setting.splitlines()[-1] == 'on', 'own pins are off: nothing to test'
        words = [f'w{n}' for n in range(0, 400, 7)]
        ranked = ("SELECT id FROM docs WHERE body ==> %s ORDER BY stannum.score(ctid) DESC LIMIT 10")
        plan = sql("EXPLAIN (ANALYZE, FORMAT JSON) " + ranked % "'w1 OR w2 OR w3'")
        pinned = re.search(r'"Pages Pinned": (\d+)', plan)
        assert pinned and int(pinned.group(1)) > 0, plan
        block = sql("SELECT block FROM stannum.index_page_kinds('docs_idx') WHERE kind = 'run' "
                    "ORDER BY block LIMIT 1")
        assert block, 'no run page'
        index = "'docs_idx'::regclass"

        # 1. A release under another session's header lock.
        assert sql(f'SELECT tests.buffer_refcount({index}, {block})') == '0'
        started = time.monotonic()
        holder = session(f'SELECT tests.own_pin_held({index}, {block}, 1500)', 'pin-holder')
        processes.append(holder)
        time.sleep(0.5)
        locker = session(f'SELECT tests.buffer_header_locked({index}, {block}, 2000)', 'header-locker')
        processes.append(locker)
        holder_out, holder_err = holder.communicate(timeout=30)
        held_for = time.monotonic() - started
        locker_out, locker_err = locker.communicate(timeout=30)
        assert holder.returncode == 0, holder_err
        assert locker.returncode == 0, locker_err
        assert locker_out.strip() == '1', f'the lock did not see the own pin: {locker_out!r}'
        after = sql(f'SELECT tests.buffer_refcount({index}, {block})')
        print(f'header lock: pin held {held_for:.2f}s, refcount after release {holder_out.strip()}, '
              f'now {after}')
        assert after == '0', f'a pin released under the header lock stayed: refcount {after}'

        # 2. Load: walks, header lockers, maintenance, cancellations, terminations.
        queries = []
        for i in range(0, len(words) - 4):
            queries.append(' OR '.join(words[i:i + 4]))
            queries.append(' AND '.join(words[i:i + 2]))
            queries.append('"' + ' '.join(words[i:i + 2]) + '"')
        array = 'ARRAY[' + ','.join("'" + q + "'" for q in queries) + ']'
        walk = f"""DO $$ DECLARE q text[] := {array}; n int; BEGIN
  FOR i IN 0..200000 LOOP
    EXECUTE format('SELECT count(*) FROM ({ranked.replace("%s", "%L")}) top',
                   q[1 + i % array_length(q, 1)]) INTO n;
  END LOOP; END $$"""
        hammer = """DO $$ DECLARE n bigint; BEGIN
  FOR i IN 1..100000 LOOP SELECT count(*) INTO n FROM pg_buffercache; END LOOP; END $$"""
        maintain = '\n'.join(['VACUUM docs;', 'REINDEX INDEX docs_idx;', 'VACUUM docs;'] * 200)

        def walkers():
            return [session(walk, f'walker-{i}') for i in range(3)]

        active = walkers()
        hammers = [session(hammer, f'hammer-{i}') for i in range(2)]
        maintainer = subprocess.Popen(['psql', '-XqAt'], env=dict(env, PGAPPNAME='maintainer'),
                                      text=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE)
        processes.extend(active + hammers + [maintainer])
        maintainer.stdin.write(maintain)
        maintainer.stdin.flush()
        cancelled = terminated = 0
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            time.sleep(0.7)
            pids = sql("SELECT string_agg(pid::text, ',') FROM pg_stat_activity "
                       "WHERE application_name LIKE 'walker-%' AND state = 'active'")
            if not pids:
                continue
            victims = pids.split(',')
            if cancelled <= terminated:
                sql(f'SELECT pg_cancel_backend({victims[0]})')
                cancelled += 1
            else:
                sql(f'SELECT pg_terminate_backend({victims[-1]})')
                terminated += 1
            for i, walker in enumerate(active):
                if walker.poll() is not None:
                    walker.communicate()
                    active[i] = session(walk, f'walker-{i}')
                    processes.append(active[i])
        # A backend in a DO loop notices its client's exit only at its end:
        # end every other session from the server's side.
        maintainer.stdin.close()
        sql("SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity "
            "WHERE backend_type = 'client backend' AND pid <> pg_backend_pid()")
        for process in active + hammers + [maintainer]:
            process.communicate(timeout=60)
        # Every session but this one has gone: nothing may stay pinned.
        for _ in range(50):
            others = sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend' "
                         "AND pid <> pg_backend_pid()")
            if others == '0':
                break
            time.sleep(0.1)
        assert others == '0', f'{others} sessions still connected'
        stuck = sql("SELECT count(*) FROM pg_buffercache WHERE pinning_backends > 0")
        print(f'load: {cancelled} cancelled, {terminated} terminated; buffers still pinned: {stuck}')
        assert stuck == '0', 'buffers stayed pinned after every session ended'
        problems = log_problems()
        assert not problems, 'server log:\n' + '\n'.join(problems)
        # A stuck pin would make this spin uncancellably in InvalidateBuffer.
        dropper = session('DROP INDEX docs_idx', 'dropper')
        processes.append(dropper)
        try:
            out, err = dropper.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'])
            raise AssertionError('DROP INDEX did not finish: a pin outlived its holder')
        assert dropper.returncode == 0, err
        success = True
        print('own pins: ok')
    finally:
        for process in processes:
            if process.poll() is None:
                process.kill()
                process.communicate()
        if (data / 'postmaster.pid').exists():
            subprocess.run(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'], env=env,
                           capture_output=True, timeout=120)
        if success:
            shutil.rmtree(root)
        else:
            print('Failure artifacts retained at ' + str(root), file=sys.stderr)


if __name__ == '__main__':
    main()
