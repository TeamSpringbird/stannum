#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""A backend that exits mid-walk must exit cleanly, never crash the server.

A FATAL error (pg_terminate_backend, a fast shutdown) exits the backend
through proc_exit without unwinding the Rust frames of the walk it
interrupts: PostgreSQL releases the walk's buffer pins and relation
references itself, and on Linux the C library then runs the backend's
thread-local destructors, which drop the cached segment readers with no
resource owner left. A reader that released those pins again there would
crash the backend, and the postmaster would restart every backend.

Runs against a server in a Docker container built from the tree under test
(see benchmarks/local/exit-test.sh), reading the server log with
`docker logs`:

    exit_during_walk.py --port 28963 --container stannum-exit

The container must be dedicated to the test: its log is checked for crashes
and the last check shuts the server down.
"""
import argparse
import random
import re
import subprocess
import threading
import time

import psycopg

# A walk checks for interrupts every 64 chunks of 65,536 rows, so a table of
# fewer than 64 chunks never takes a termination inside a walk: every one
# lands between walks, with no page held.
ROWS = 4_600_000

# Any of these in the server log means a backend died by a signal or an
# assertion and the postmaster reinitialized.
CRASH = re.compile(r'terminated by signal|server process .* was terminated|reinitializing|'
                   r'terminating any other active server processes|TRAP:|PANIC:|'
                   r'all server processes terminated|was interrupted while in recovery|'
                   r'database system was not properly shut down')


def vocabulary():
    syllables = ['ka', 'lo', 'mi', 'ne', 'pu', 'ra', 'si', 'to', 'va', 'ze', 'bo', 'du', 'fe', 'gi', 'ho']
    return [a + b + 'x' for a in syllables for b in syllables]


def queries(words, seed):
    """Disjunctions, conjunctions and phrases over common and rare words,
    so every statement walks many chunks with pages held."""
    rng = random.Random(seed)
    common, middle, rare = words[:20], words[20:120], words[120:]
    out = []
    for _ in range(60):
        out.append(' OR '.join(rng.sample(common, 2) + rng.sample(middle, 2) + [rng.choice(rare)]))
        out.append(' AND '.join(rng.sample(common, 2) + [rng.choice(middle)]))
        out.append(f'{rng.choice(common)} AND {rng.choice(common)}')
        out.append('"' + ' '.join(rng.sample(common, 2)) + '"')
    return out


def log_lines(container):
    return subprocess.run(['docker', 'logs', container], check=True, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.splitlines()


# A release of something exit processing already released need not crash:
# it can raise instead ("... is not owned by resource owner"), promoted to
# FATAL during exit, or warn of a leak. Only the termination itself may be
# reported at those levels.
EXPECTED = re.compile(r'FATAL:  terminating connection due to administrator command')
SEVERE = re.compile(r'\b(WARNING|ERROR|FATAL|PANIC):')


def crashes(lines):
    return [line for line in lines
            if CRASH.search(line) or SEVERE.search(line) and not EXPECTED.search(line)]


def walker_block(texts):
    array = 'ARRAY[' + ','.join("'" + t.replace("'", "''") + "'" for t in texts) + ']'
    # Thousands of ranked statements, each a top-10 walk; the loop outlives
    # every round, which ends by terminating the backend.
    return f"""DO $$
DECLARE q text[] := {array}; n bigint;
BEGIN
  FOR i IN 0..10000000 LOOP
    EXECUTE format('SELECT count(*) FROM (SELECT id FROM exit_docs WHERE body ==> %L
                    ORDER BY stannum.score(ctid) DESC LIMIT 10) top', q[1 + i % array_length(q, 1)])
      INTO n;
  END LOOP;
END $$"""


class Walker:
    """A session running ranked walks in a thread until its backend dies."""

    def __init__(self, dsn, block):
        self.connection = psycopg.connect(dsn, autocommit=True)
        self.pid = self.connection.info.backend_pid
        self.error = None
        self.thread = threading.Thread(target=self.run, args=(block,), daemon=True)
        self.thread.start()

    def run(self, block):
        try:
            self.connection.execute(block)
        except psycopg.Error as error:
            self.error = error

    def join(self):
        self.thread.join(60)
        assert not self.thread.is_alive(), f'backend {self.pid} still running after exit'
        self.connection.close()
        return self.error


def setup(control, rows):
    words = vocabulary()
    exists = control.execute("SELECT to_regclass('exit_docs_search') IS NOT NULL").fetchone()[0]
    if exists and control.execute('SELECT count(*) FROM exit_docs').fetchone()[0] == rows:
        return words
    control.execute('DROP TABLE IF EXISTS exit_docs')
    control.execute('CREATE EXTENSION IF NOT EXISTS stannum')
    control.execute('CREATE TABLE exit_docs(id int PRIMARY KEY, body text)')
    control.execute('SELECT setseed(0.42)')
    # Zipf-like: a cube of a uniform draw favours the first words, so a few
    # are in most documents and most are rare.
    control.execute("""INSERT INTO exit_docs
        SELECT n, (SELECT string_agg((%s::text[])[1 + floor(power(random(), 3) * %s)::int], ' ')
                   FROM generate_series(1, 8 + n %% 9))
        FROM generate_series(1, %s) n""", (words, len(words), rows))
    control.execute('CREATE INDEX exit_docs_search ON exit_docs USING stannum(body)')
    control.execute('VACUUM ANALYZE exit_docs')
    return words


def assert_walks_hold_pages(control, texts):
    for text in texts[:4]:
        plan = control.execute(
            """EXPLAIN (ANALYZE, FORMAT JSON) SELECT id FROM exit_docs WHERE body ==> %s
               ORDER BY stannum.score(ctid) DESC LIMIT 10""", (text,)).fetchone()[0]
        found = re.search(r'"Pages Pinned": (\d+)', str(plan).replace("'", '"'))
        assert found and int(found.group(1)) > 0, (text, plan)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--port', type=int, required=True)
    parser.add_argument('--container', required=True)
    parser.add_argument('--host', default='127.0.0.1')
    parser.add_argument('--password', default='postgres')
    parser.add_argument('--rounds', type=int, default=40)
    parser.add_argument('--seed', type=int, default=1)
    parser.add_argument('--rows', type=int, default=ROWS,
                        help='documents; a table already holding this many is reused')
    parser.add_argument('--no-shutdown', action='store_true', help='skip the final fast shutdown')
    args = parser.parse_args()
    dsn = f'host={args.host} port={args.port} user=postgres password={args.password} dbname=postgres'
    rng = random.Random(args.seed)

    control = psycopg.connect(dsn, autocommit=True)
    words = setup(control, args.rows)
    texts = queries(words, args.seed)
    assert_walks_hold_pages(control, texts)
    block = walker_block(texts)
    started = control.execute('SELECT pg_postmaster_start_time()').fetchone()[0]
    before = len(log_lines(args.container))

    terminated = 0
    for round in range(args.rounds):
        walker = Walker(dsn, block)
        # Let the session load and cache its readers, then walk a while.
        time.sleep(0.3 + rng.random() * 0.9)
        state = control.execute('SELECT state FROM pg_stat_activity WHERE pid = %s',
                                (walker.pid,)).fetchone()
        assert state == ('active',), (round, walker.pid, state, walker.error)
        assert control.execute('SELECT pg_terminate_backend(%s, 10000)', (walker.pid,)).fetchone()[0]
        error = walker.join()
        assert error is not None and 'terminating connection due to administrator command' in str(error), \
            (round, error)
        terminated += 1
        # A crash restart terminates this connection too.
        assert control.execute('SELECT pg_postmaster_start_time()').fetchone()[0] == started, round
        found = crashes(log_lines(args.container)[before:])
        assert not found, f'round {round}: server crashed after pg_terminate_backend:\n' + '\n'.join(found)
    print(f'pg_terminate_backend during a walk: {terminated} rounds, no crash')

    if not args.no_shutdown:
        walkers = [Walker(dsn, block) for _ in range(4)]
        time.sleep(1.0)
        assert all(control.execute('SELECT state FROM pg_stat_activity WHERE pid = %s', (w.pid,)).fetchone()
                   == ('active',) for w in walkers)
        control.close()
        # SIGINT to the postmaster, the container's PID 1, is a fast shutdown.
        subprocess.run(['docker', 'kill', '--signal', 'INT', args.container], check=True,
                       stdout=subprocess.DEVNULL)
        subprocess.run(['docker', 'wait', args.container], check=True, timeout=120,
                       stdout=subprocess.DEVNULL)
        for walker in walkers:
            walker.join()
        deadline = time.time() + 60
        while True:
            lines = log_lines(args.container)[before:]
            if any('database system is shut down' in line for line in lines) or time.time() > deadline:
                break
            time.sleep(0.5)
        found = crashes(lines)
        assert not found, 'server crashed in a fast shutdown during walks:\n' + '\n'.join(found)
        assert any('database system is shut down' in line for line in lines), '\n'.join(lines[-30:])
        print('fast shutdown during walks: clean')


if __name__ == '__main__':
    main()
