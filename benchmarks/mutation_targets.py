#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Exercise generated writer SQL against sparse, locked and exhausted live targets."""
import os
from pathlib import Path
import select
import shutil
import subprocess
import tempfile

import mutation
import run


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-mutation-targets-'))
    env = dict(os.environ, PGHOST=str(root), PGPORT='28993', PGUSER='writer_test',
               PGDATABASE='postgres', PGOPTIONS='-c statement_timeout=10000')
    for key in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(key, None)
    def command(argv, **kwargs):
        return subprocess.run(argv, env=env, text=True, capture_output=True, check=True, timeout=30, **kwargs)
    def sql(statement):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=statement).stdout.strip()
    def statement(kind):
        script = mutation.accounted_writer(mutation.writer_scripts(2, run.CASES)[kind], kind)
        return '\n'.join(line for line in script.splitlines() if not line.startswith('\\set ')).replace(':probe', '2147483647').replace(':src', '1').replace(':k', '1')
    lock = None
    passed = False
    try:
        command(['initdb', '-D', str(root/'data'), '-U', 'writer_test', '-A', 'trust', '--no-locale'])
        with (root/'data/postgresql.conf').open('a') as config:
            config.write(f"\nlisten_addresses=''\nport=28993\nunix_socket_directories='{root}'\n")
        command(['pg_ctl', '-D', str(root/'data'), '-l', str(root/'server.log'), '-w', 'start'])
        sql("CREATE TABLE documents(id bigint PRIMARY KEY,body text); INSERT INTO documents VALUES (1,'common mutablea'),(2,'common mutablea');" + mutation.pool_sql(2))
        # Sparse, very high IDs must work independently of offered rate.
        sql('UPDATE documents SET id=1000000000 WHERE id=2;')
        lock = subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], env=env, text=True,
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        lock.stdin.write('BEGIN; SELECT id FROM documents WHERE id=1000000000 FOR UPDATE;\n')
        lock.stdin.flush()
        assert select.select([lock.stdout], [], [], 10)[0], 'lock holder did not start'
        assert lock.stdout.readline().strip() == '1000000000'
        # Highest row is locked; selection must wrap around and update ID 1.
        assert sql(statement('update')) == '1'
        assert sql("SELECT body LIKE '%rare%' FROM documents WHERE id=1") == 't'
        assert sql(statement('delete')) == '1'
        assert sql('SELECT count(*) FROM documents WHERE id=1') == '0'
        # Every remaining row is locked. Failure must not count as completion.
        for kind in ('delete', 'update'):
            result = subprocess.run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], env=env,
                                    text=True, input=statement(kind), capture_output=True, timeout=15)
            assert result.returncode and 'division by zero' in result.stderr, result
        lock.communicate('ROLLBACK;\n', timeout=10)
        assert lock.returncode == 0
        lock = None
        assert sql(statement('delete')) == '1'
        assert sql('SELECT count(*) FROM documents') == '0'
        assert sql(statement('insert')) == '1'
        assert sql('SELECT count(*) FROM documents') == '1'
        # A multi-row mutation must roll back, rather than overcount success.
        guard = mutation.accounted_writer("INSERT INTO documents VALUES(4,'a'),(5,'b');", 'insert')
        result = subprocess.run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], env=env,
                                text=True, input=guard, capture_output=True, timeout=15)
        assert result.returncode and 'division by zero' in result.stderr, result
        assert sql('SELECT count(*) FROM documents') == '1'
        passed = True
        print('PASS sparse keys, locked-key wraparound, exhaustion and atomic effect accounting')
    finally:
        if lock is not None and lock.poll() is None:
            lock.terminate()
            lock.communicate(timeout=10)
        if (root/'data/postmaster.pid').exists():
            command(['pg_ctl', '-D', str(root/'data'), '-m', 'fast', '-w', 'stop'])
        if passed:
            shutil.rmtree(root)
        else:
            print('Failure artifacts retained at ' + str(root))


if __name__ == '__main__':
    main()
