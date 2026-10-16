#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""The installed x86-64 loader (postgres/loader/loader.c) in a private cluster.

On x86-64 Linux, script/install-release installs `stannum.so` as a loader
beside two builds, `stannum-x86-64.so` and `stannum-x86-64-v4.so`. Each case
starts the server with the loader's environment set one way and checks which
build runs (`SHOW stannum.cpu_level`), that the maintenance launcher (a
background worker the loader's trampolines start) runs when preloaded, and
that an index answers the same queries alike under every build:

- auto: preloaded, no setting: the build the CPU runs (cpu_probe.c's choice);
- baseline: STANNUM_LOADER_LEVEL=x86-64;
- hidden: STANNUM_LOADER_HIDE=avx512f, as on a CPU without AVX-512;
- v4: STANNUM_LOADER_LEVEL=x86-64-v4: the v4 build, or on a CPU without it a
  server that refuses to start with the reason in its log;
- unknown: STANNUM_LOADER_LEVEL=x86-64-v9 refuses to start;
- lazy: not preloaded: the first call in a session loads the build.

Install first (`script/test-all install`). Elsewhere (aarch64, macOS, one
x86-64 build) the installed library is not the loader and this reports
`skipped`:

    python3 postgres/tests/loader_shim.py [case...]
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

PORT = 28966
ROOT = Path(__file__).resolve().parents[2]
MARKER = b'stannum: the loader has not loaded a build'

QUERIES = [
    "SELECT array_agg(id ORDER BY id) FROM docs WHERE body ==> 'beer'",
    "SELECT array_agg(id ORDER BY id) FROM docs WHERE body ==> 'beer AND festival'",
    "SELECT array_agg(id ORDER BY id) FROM docs WHERE body ==> '\"craft beer\"'",
    "SELECT count(*) FROM docs WHERE body ==> 'beer OR wine'",
    "SELECT array_agg(id) FROM (SELECT id FROM docs WHERE body ==> 'beer OR wine' "
    "ORDER BY stannum.score(ctid) DESC, id LIMIT 5) t",
    "SELECT array_agg(id) FROM (SELECT id FROM docs WHERE body ==> 'craft beer' "
    "ORDER BY stannum.full_score(ctid) DESC, id LIMIT 5) t",
]


def command(args, **kw):
    try:
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kw)
    except subprocess.CalledProcessError as error:
        raise AssertionError(error.output) from error


def main():
    libdir = Path(command(['pg_config', '--pkglibdir']).strip())
    loader = libdir / 'stannum.so'
    if not (loader.exists() and MARKER in loader.read_bytes()):
        print(json.dumps({'status': 'skipped', 'reason': f'{loader} is not the x86-64 loader'}))
        return
    for build in ('stannum-x86-64.so', 'stannum-x86-64-v4.so'):
        assert (libdir / build).exists(), f'the loader is installed without {build}'

    root = Path(tempfile.mkdtemp(prefix='stannum-loader-'))
    data = root / 'data'
    log = root / 'server.log'
    probe = root / 'cpu_probe'
    command([os.environ.get('CC', 'cc'), '-std=c11', '-O1', '-o', str(probe),
             str(ROOT / 'postgres' / 'loader' / 'cpu_probe.c')])
    real = dict(line.split(' ', 1) for line in command([str(probe), 'choose', '--real']).splitlines())
    cpu_level = real['library']
    base = {k: v for k, v in os.environ.items()
            if not k.startswith('STANNUM_LOADER_') and k not in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD')}
    client = dict(base, PGHOST=str(root), PGPORT=str(PORT), PGUSER='postgres', PGDATABASE='postgres')

    def sql(text):
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=client).strip()

    def start(preload=True, **settings):
        """Starts the server with STANNUM_LOADER_<KEY> settings; False if it would not start."""
        env = dict(base, **{f'STANNUM_LOADER_{k.upper()}': v for k, v in settings.items()})
        conf = data / 'postgresql.auto.conf'
        conf.write_text(f"shared_preload_libraries = '{'stannum' if preload else ''}'\n")
        log.write_text('')
        started = subprocess.run(['pg_ctl', '-D', str(data), '-l', str(log), '-w', '-t', '60', 'start'],
                                 env=env, capture_output=True, text=True).returncode == 0
        if not started and (data / 'postmaster.pid').exists():
            stop()
        return started

    def stop():
        command(['pg_ctl', '-D', str(data), '-m', 'fast', '-w', 'stop'], env=base)

    def observe(preload=True):
        """What the running server reports: build, launcher, answers."""
        level = sql('SELECT stannum.version(); SHOW stannum.cpu_level;').splitlines()[-1]
        launchers = sql("SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'stannum maintenance launcher';")
        answers = [sql(q) for q in QUERIES]
        stop()
        assert launchers == ('1' if preload else '0'), launchers
        return {'cpu_level': level, 'answers': answers}

    expected = {}

    def run(name, want_level, preload=True, **settings):
        assert start(preload, **settings), f'{name}: the server did not start:\n{log.read_text()}'
        seen = observe(preload)
        assert seen['cpu_level'] == want_level, (name, seen['cpu_level'], want_level)
        expected.setdefault('answers', seen['answers'])
        assert seen['answers'] == expected['answers'], (name, seen['answers'], expected['answers'])
        return seen['cpu_level']

    def refuses(name, message, **settings):
        assert not start(True, **settings), f'{name}: the server started'
        text = log.read_text()
        assert message in text, (name, text)
        return 'refused'

    cases = {
        'auto': lambda: run('auto', cpu_level),
        'baseline': lambda: run('baseline', 'x86-64', level='x86-64'),
        'hidden': lambda: run('hidden', 'x86-64', hide='avx512f'),
        'v4': lambda: (run('v4', 'x86-64-v4', level='x86-64-v4') if cpu_level == 'x86-64-v4'
                       else refuses('v4', 'lacks part of x86-64-v4', level='x86-64-v4')),
        'unknown': lambda: refuses('unknown', 'STANNUM_LOADER_LEVEL is auto, x86-64 or x86-64-v4',
                                   level='x86-64-v9'),
        'lazy': lambda: run('lazy', cpu_level, preload=False),
    }
    selected = sys.argv[1:] or list(cases)
    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8'],
                env=base)
        with (data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport={PORT}\nunix_socket_directories='{root}'\n"
                    "shared_buffers='64MB'\nautovacuum=off\nenable_seqscan=off\n")
        assert start(True), log.read_text()
        sql("CREATE EXTENSION stannum;"
            "CREATE TABLE docs (id int, body text);"
            "INSERT INTO docs SELECT n, (ARRAY['craft beer', 'wine', 'beer festival', 'wine and beer', "
            "'festival'])[1 + n % 5] || ' w' || n FROM generate_series(1, 2000) n;"
            "CREATE INDEX docs_idx ON docs USING stannum (body);")
        stop()
        results = {name: cases[name]() for name in selected}
        print(json.dumps({'status': 'passed', 'cpu': cpu_level, 'missing': real['missing'], 'cases': results}))
        print('Artifacts:', root)
    finally:
        if (data / 'postmaster.pid').exists():
            stop()


if __name__ == '__main__':
    main()
