#!/usr/bin/env python3
"""Replay paired fold measurements using two already-built release libraries.

Both libraries must match the installed extension SQL. Uses a disposable private
PostgreSQL cluster and restores the installed library on exit. Invoke through
the installation lock when sharing a development machine. Raw artifacts and the
server log are retained; temporary cluster data is removed after shutdown.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from foreground_writes import positive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--codec', type=Path, required=True)
    parser.add_argument('--combined', type=Path, required=True)
    parser.add_argument('--installed-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--checkpoint-control', action='store_true')
    parser.add_argument('--seconds', type=positive, default=20)
    parser.add_argument('--rounds', type=positive, default=3)
    args = parser.parse_args()
    binaries = {name: getattr(args, name).resolve(strict=True)
                for name in ('codec', 'combined')}
    library = args.installed_library.resolve(strict=True)
    if library in binaries.values():
        parser.error('source libraries must be retained copies, not the installed library')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    harness = Path(__file__).resolve().with_name('contention.py')
    metadata = {
        'checkpoint_control': args.checkpoint_control,
        'seconds': args.seconds, 'rounds': args.rounds,
        'builds': {name: {'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                   for name, path in binaries.items()},
        'harness_sha256': hashlib.sha256(harness.read_bytes()).hexdigest(),
    }
    (output / 'builds.json').write_text(json.dumps(metadata, indent=2) + '\n')
    original = output / 'original-library'
    shutil.copy2(library, original)

    def install(source):
        temporary = library.with_name(library.name + '.fold-reproduction')
        shutil.copy2(source, temporary)
        os.replace(temporary, library)

    cluster = Path(tempfile.mkdtemp(prefix='stannum-fold-repro-'))
    env = dict(os.environ, PGHOST=str(cluster), PGPORT='28995',
               PGDATABASE='postgres', PGUSER='fold_bench')
    for key in ('PGOPTIONS', 'PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(key, None)

    def run(command, label):
        with (output / (label + '.log')).open('w') as log:
            subprocess.run(command, env=env, check=True, stdout=log,
                           stderr=subprocess.STDOUT)

    started = False
    stopped = False
    try:
        run(['initdb', '-D', str(cluster / 'data'), '-U', 'fold_bench', '-A', 'trust',
             '--no-locale', '--encoding=UTF8'], 'initdb')
        settings = ("\nlisten_addresses=''\nport=28995\n"
                    f"unix_socket_directories='{cluster}'\nshared_buffers='512MB'\n")
        if args.checkpoint_control:
            settings += "checkpoint_timeout='30min'\nmax_wal_size='16GB'\n"
        with (cluster / 'data/postgresql.conf').open('a') as config:
            config.write(settings)
        (output / 'settings.conf').write_text(settings)
        run(['pg_ctl', '-D', str(cluster / 'data'), '-l', str(cluster / 'server.log'),
             '-w', 'start'], 'start')
        started = True
        run(['psql', '-XqAt', '-c', 'SELECT version()'], 'version')
        for number in range(1, args.rounds + 1):
            order = ['codec', 'combined'] if number % 2 else ['combined', 'codec']
            for name in order:
                label = f'round{number}-{name}'
                install(binaries[name])
                if args.checkpoint_control:
                    run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', 'CHECKPOINT'],
                        label + '-checkpoint')
                print('START ' + label, flush=True)
                run(['python3', str(harness), '--seconds', str(args.seconds), '--writers', '2',
                     '--readers', '2', '--write-buffer-docs', '32', '--repeat', '200',
                     '--sample-ms', '50', '--max-merge-docs', '1024',
                     '--artifact', str(library), '--output', str(output / label)], label)
                print('PASS ' + label, flush=True)
    finally:
        try:
            if started:
                run(['pg_ctl', '-D', str(cluster / 'data'), '-m', 'fast', '-w', 'stop'], 'stop')
            stopped = True
        finally:
            install(original)
            if (cluster / 'server.log').exists():
                shutil.copy2(cluster / 'server.log', output / 'server.log')
            if stopped:
                shutil.rmtree(cluster)
            else:
                print('Shutdown failed; cluster retained at ' + str(cluster), flush=True)


if __name__ == '__main__':
    main()
