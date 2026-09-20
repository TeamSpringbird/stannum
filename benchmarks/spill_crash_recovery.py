#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Crash/restart opt-in spilling in private PostgreSQL 18 clusters.

Requires a pg_test package, not a production library. Package with cargo pgrx
package --debug --test --package stannum --features 'pg18 pg_test' --out-dir PATH.
No installed extension, existing cluster, or external connection is modified.
The shared native-test lock covers all cluster lifetimes. Failed cluster data
and all logs are retained. Successful cluster data is removed after shutdown.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def literal(value):
    return "'" + str(value).replace("'", "''") + "'"


def checked_pid(marker_pid, active_pid):
    """Never signal a PID that was not independently found in our own cluster."""
    if marker_pid <= 1 or str(marker_pid) != active_pid.strip():
        raise RuntimeError('crash marker PID does not match scratch backend')
    return marker_pid


def campaign(package, output, bindir, flush_wal):
    controls = list(package.rglob('stannum.control'))
    libraries = [p for p in package.rglob('stannum.*') if p.suffix in ('.so', '.dylib')]
    if len(controls) != 1 or len(libraries) != 1:
        raise RuntimeError('package must contain exactly one stannum control and library')
    control, library = controls[0], libraries[0]
    manifest = {
        'package': str(package), 'library_sha256': digest(library),
        'control_sha256': digest(control), 'harness_sha256': digest(Path(__file__)),
        'sql_sha256': {str(p.relative_to(package)): digest(p) for p in package.rglob('*.sql')},
        'flush_wal_at_pause': flush_wal, 'complete': False, 'cases': [],
        'limitations': ['Debug pg_test build, synthetic pause, tiny fixture; not performance data.',
                        'Process crashes/restarts, not hardware power loss or torn writes.',
                        'Output spilling only; no streaming-input or spilled VACUUM claims.'],
    }
    try:
        for mode in ('backend-sigkill', 'server-immediate'):
            for point in ('spill:appended', 'spill:page-written'):
                case = run_case(mode, point, flush_wal, output, bindir, control, library)
                manifest['cases'].append(case)
                print('PASS ' + case['name'], flush=True)
        manifest['complete'] = True
    except BaseException as error:
        manifest['failure'] = repr(error)
        raise
    finally:
        (output / 'results.json').write_text(json.dumps(manifest, indent=2) + '\n')


def run_case(mode, point, flush_wal, output, bindir, control, library):
    name = mode + '-' + point.split(':')[1]
    evidence = output / name
    evidence.mkdir()
    root = Path(tempfile.mkdtemp(prefix='stannum-spill-crash-'))
    data = root / 'data'
    server_log = evidence / 'server.log'
    env = {k: v for k, v in os.environ.items() if not k.startswith('PG')}
    env.update(PGHOST=str(root), PGPORT='28993', PGDATABASE='postgres', PGUSER='spill_crash')
    case = {'name': name, 'mode': mode, 'point': point, 'flush_wal': flush_wal,
            'cluster': str(root), 'passed': False}
    child = None
    stopped = False

    def command(argv, label, *, input=None, check=True, extra_env=None):
        result = subprocess.run([str(bindir / argv[0]), *argv[1:]],
                                env=dict(env, **(extra_env or {})), input=input,
                                text=True, capture_output=True, timeout=40)
        (evidence / (label + '.stdout')).write_text(result.stdout)
        (evidence / (label + '.stderr')).write_text(result.stderr)
        if check and result.returncode:
            raise RuntimeError(f'{label} failed ({result.returncode}): {result.stderr}')
        return result

    def sql(statement, label):
        return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], label,
                       input=statement).stdout.strip()

    def start(label):
        command(['pg_ctl', '-D', str(data), '-l', str(server_log), '-w', 'start'], label)

    def snapshot(label):
        text = sql("""SELECT json_build_object(
          'heap_ids',(SELECT json_agg(id ORDER BY id) FROM docs),
          'segments',(SELECT json_agg(s) FROM stannum.segment_info('docs_idx') s),
          'findings',(SELECT json_agg(s) FROM stannum.verify_index('docs_idx',true) s),
          'temp_files',(SELECT json_agg(s) FROM pg_ls_tmpdir() s))""", label)
        return json.loads(text)

    def matches(label):
        # Force each path and retain plans so membership equality cannot pass
        # merely because both requests accidentally used sequential scans.
        queries = ('needle', 'first OR second', 'needle THEN/0 first', 'third')
        results = {}
        for engine in ('heap', 'index'):
            settings = ("SET enable_seqscan=on; SET enable_indexscan=off; SET enable_bitmapscan=off;"
                        if engine == 'heap' else
                        "SET enable_seqscan=off; SET enable_indexscan=on; SET enable_bitmapscan=on;")
            settings += 'SET stannum.enable_custom_scan=off;'
            results[engine] = []
            for n, query in enumerate(queries):
                body = 'SELECT id FROM docs WHERE body ==> ' + literal(query) + ' ORDER BY id'
                plan = json.loads(sql(settings + 'EXPLAIN (FORMAT JSON) ' + body,
                                      f'{label}-{engine}-{n}-plan'))
                def nodes(node):
                    return [node['Node Type']] + [v for child in node.get('Plans', []) for v in nodes(child)]
                types = nodes(plan[0]['Plan'])
                assert ('Seq Scan' in types) == (engine == 'heap'), types
                if engine == 'index':
                    assert any('Index' in kind for kind in types), types
                    assert 'docs_idx' in json.dumps(plan), plan
                rows = sql(settings + body, f'{label}-{engine}-{n}-rows')
                results[engine].append(rows)
        assert results['heap'] == results['index'], results
        expected = ['1\n2', '1\n2', '1', ''] if label == 'recovered' else ['1\n2\n3', '1\n2', '1', '3']
        assert results['heap'] == expected, results
        return results

    try:
        command(['initdb', '-D', str(data), '-U', 'spill_crash', '-A', 'trust',
                 '--no-locale', '--encoding=UTF8'], 'initdb')
        settings = ("\nlisten_addresses=''\nport=28993\n"
                    f"unix_socket_directories={literal(root)}\n"
                    f"extension_control_path={literal(control.parent.parent)}\n"
                    f"dynamic_library_path={literal(library.parent)}\n"
                    "fsync=on\nfull_page_writes=on\nsynchronous_commit=on\n"
                    "autovacuum=off\nrestart_after_crash=on\nlog_min_messages=log\n"
                    "log_line_prefix='%m [%p] '\nshared_buffers='32MB'\n")
        with (data / 'postgresql.conf').open('a') as stream:
            stream.write(settings)
        (evidence / 'settings.conf').write_text(settings)
        start('start')
        assert Path(sql('SHOW data_directory', 'cluster-identity')).resolve() == data.resolve()
        case['server'] = json.loads(sql("""SELECT json_build_object('version',version(),
           'fsync',current_setting('fsync'),'full_page_writes',current_setting('full_page_writes'),
           'synchronous_commit',current_setting('synchronous_commit'))""", 'server-settings'))
        sql("""CREATE EXTENSION stannum;
          CREATE TABLE docs(id int, body text);
          CREATE INDEX docs_idx ON docs USING stannum(body);
          SET stannum.experimental_merge_output_kb=1;
          SET stannum.write_buffer_docs=1;
          SET stannum.merge_tier_factor=2;
          SET stannum.max_merge_docs=1024;
          INSERT INTO docs VALUES (1,repeat('needle first ',20000)),(2,repeat('needle second ',20000));
          CHECKPOINT;""", 'fixture')
        case['before'] = snapshot('before')
        assert case['before']['heap_ids'] == [1, 2]
        assert not case['before']['findings']
        appname = 'spill-crash-' + name
        mutation = ("SET stannum.experimental_merge_output_kb=1;"
                    "SET stannum.write_buffer_docs=1; SET stannum.merge_tier_factor=2;"
                    "SET stannum.max_merge_docs=1024;"
                    f"SELECT tests.arm_spill_crash({literal(point)},{str(flush_wal).lower()});"
                    "INSERT INTO docs VALUES (3,repeat('needle third ',20000));")
        (evidence / 'mutation.sql').write_text(mutation + '\n')
        with (evidence / 'mutation.stdout').open('w') as stdout, (evidence / 'mutation.stderr').open('w') as stderr:
            child = subprocess.Popen([str(bindir / 'psql'), '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', mutation],
                                     env=dict(env, PGAPPNAME=appname), stdout=stdout, stderr=stderr)
            deadline = time.monotonic() + 30
            marker = None
            while time.monotonic() < deadline:
                match = re.search(r'STANNUM_SPILL_CRASH_READY point=' + re.escape(point) +
                                  r' pid=(\d+) flush_wal=(true|false)', server_log.read_text())
                if match:
                    marker = match
                    break
                if child.poll() is not None:
                    raise RuntimeError('mutation exited before crash pause')
                time.sleep(.05)
            if marker is None:
                raise RuntimeError('timeout waiting for real spill pause')
            assert marker[2] == str(flush_wal).lower()
            pid = checked_pid(int(marker[1]), sql('SELECT pid FROM pg_stat_activity WHERE application_name=' +
                                                literal(appname), 'backend-identity'))
            case['backend_pid'] = pid
            case['paused_temp_files'] = json.loads(sql('SELECT json_agg(s) FROM pg_ls_tmpdir() s', 'paused-temp-files'))
            assert len(case['paused_temp_files']) >= 2
            assert sum(f['size'] for f in case['paused_temp_files']) > 0, 'must spill actual bytes'
            if mode == 'backend-sigkill':
                os.kill(pid, signal.SIGKILL)
            else:
                command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'], 'immediate-stop')
                start('restart')
            case['mutation_returncode'] = child.wait(timeout=30)
            assert case['mutation_returncode'] != 0
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            ready = command(['psql', '-XqAt', '-c', 'SELECT 1'], 'recovery-ready', check=False)
            if ready.returncode == 0:
                break
            time.sleep(.1)
        else:
            raise RuntimeError('server did not recover')
        case['recovered'] = snapshot('recovered')
        assert case['recovered']['heap_ids'] == [1, 2]
        assert not case['recovered']['temp_files'], 'startup did not clean temporary files'
        case['recovered_queries'] = matches('recovered')
        findings = case['recovered']['findings'] or []
        assert all(row['severity'] == 'warning' and (
            row['message'] == 'run page referenced by nothing; VACUUM reclaims it' or
            row['message'] == 'unreferenced and unreadable: not a Stannum LDP2 page'
        ) for row in findings), findings
        if flush_wal:
            assert findings, 'flushed unpublished page WAL must survive recovery'
        sql('VACUUM (INDEX_CLEANUP ON, VERBOSE) docs;', 'vacuum')
        case['after_vacuum'] = snapshot('after-vacuum')
        assert not case['after_vacuum']['findings']
        sql("""SET stannum.experimental_merge_output_kb=1; SET stannum.write_buffer_docs=1;
          SET stannum.merge_tier_factor=2; SET stannum.max_merge_docs=1024;
          INSERT INTO docs VALUES (3,repeat('needle third ',20000));""", 'retry')
        case['retry_queries'] = matches('retry')
        case['after_retry'] = snapshot('after-retry')
        assert not case['after_retry']['temp_files']
        assert not case['after_retry']['findings']
        log = server_log.read_text()
        assert 'redo starts at' in log and 'database system was interrupted' in log
        if mode == 'backend-sigkill':
            assert f'(PID {pid}) was terminated by signal 9' in log
        else:
            assert 'received immediate shutdown request' in log
        case['passed'] = True
        return case
    finally:
        try:
            if (data / 'postmaster.pid').exists():
                command(['pg_ctl', '-D', str(data), '-m', 'immediate', '-w', 'stop'], 'final-stop')
            stopped = True
        finally:
            if child is not None and child.poll() is None:
                child.wait(timeout=10)
            (evidence / 'result.json').write_text(json.dumps(case, indent=2) + '\n')
            if stopped and case['passed']:
                shutil.rmtree(root)
            else:
                print('Preserved scratch cluster at ' + str(root), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package-dir', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--pg-bindir', type=Path, required=True)
    parser.add_argument('--flush-wal', action='store_true',
                        help='force durability of unpublished page WAL at pause (separate from ordinary path)')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    with open('/tmp/stannum-pgrx.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        campaign(args.package_dir.resolve(strict=True), output,
                 args.pg_bindir.resolve(strict=True), args.flush_wal)


if __name__ == '__main__':
    main()
