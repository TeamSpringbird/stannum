#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Replay paired contention measurements using two already-built release libraries.

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
from vacuum_cleanup import add_workload_arguments


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--integrated', type=Path, required=True)
    parser.add_argument('--installed-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--checkpoint-control', action='store_true')
    parser.add_argument('--workload', choices=['contention', 'vacuum'], default='contention')
    parser.add_argument('--docs', type=positive, default=32768)
    parser.add_argument('--scenario', choices=['merge', 'rewrite', 'mixed'], default='merge')
    parser.add_argument('--baseline-vacuum-strategy', choices=['auto', 'direct', 'reconstruct'])
    parser.add_argument('--integrated-vacuum-strategy', choices=['auto', 'direct', 'reconstruct'])
    parser.add_argument('--writer-rate', type=positive)
    parser.add_argument('--repeat', type=positive, default=200)
    parser.add_argument('--seconds', type=positive, default=20)
    parser.add_argument('--rounds', type=positive, default=3)
    add_workload_arguments(parser)
    args = parser.parse_args()
    binaries = {name: getattr(args, name).resolve(strict=True)
                for name in ('baseline', 'integrated')}
    library = args.installed_library.resolve(strict=True)
    if library in binaries.values():
        parser.error('source libraries must be retained copies, not the installed library')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    harness = Path(__file__).resolve().with_name('contention.py' if args.workload == 'contention' else 'vacuum_cleanup.py')
    metadata = {
        'checkpoint_control': args.checkpoint_control, 'workload': args.workload, 'scenario': args.scenario, 'docs': args.docs,
        'seconds': args.seconds, 'rounds': args.rounds, 'repeat': args.repeat, 'writer_rate': args.writer_rate,
        'vacuum_config': {key: getattr(args, key) for key in ('delete_percent', 'vocabulary', 'distribution', 'query_shapes', 'reader_rate', 'reader_seconds', 'readers')},
        'builds': {name: {'vacuum_strategy': getattr(args, name + '_vacuum_strategy'), 'path': str(path), 'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}
                   for name, path in binaries.items()},
        'harness_sha256': hashlib.sha256(harness.read_bytes()).hexdigest(),
    }
    (output / 'builds.json').write_text(json.dumps(metadata, indent=2) + '\n')
    original = output / 'original-library'
    shutil.copy2(library, original)

    def install(source):
        temporary = library.with_name(library.name + '.paired-libraries')
        shutil.copy2(source, temporary)
        os.replace(temporary, library)

    cluster = Path(tempfile.mkdtemp(prefix='stannum-paired-libraries-'))
    env = dict(os.environ, PGHOST=str(cluster), PGPORT='28995',
               PGDATABASE='postgres', PGUSER='paired_bench')
    for key in ('PGOPTIONS', 'PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(key, None)

    def run(command, label):
        with (output / (label + '.log')).open('w') as log:
            subprocess.run(command, env=env, check=True, stdout=log,
                           stderr=subprocess.STDOUT)

    started = False
    stopped = False
    try:
        run(['initdb', '-D', str(cluster / 'data'), '-U', 'paired_bench', '-A', 'trust',
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
            order = ['baseline', 'integrated'] if number % 2 else ['integrated', 'baseline']
            for name in order:
                label = f'round{number}-{name}'
                # Each postmaster and all children must exit before replacing
                # a mapped extension library, including background workers.
                run(['pg_ctl', '-D', str(cluster / 'data'), '-m', 'fast', '-w', 'stop'], label + '-stop')
                started = False
                install(binaries[name])
                run(['pg_ctl', '-D', str(cluster / 'data'), '-l', str(cluster / 'server.log'),
                     '-w', 'start'], label + '-start')
                started = True
                if args.checkpoint_control:
                    run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', 'CHECKPOINT'],
                        label + '-checkpoint')
                print('START ' + label, flush=True)
                if args.workload == 'vacuum':
                    strategy = getattr(args, name + '_vacuum_strategy')
                    workload = [] if strategy is None else ['--vacuum-strategy', strategy]
                    for key in ('delete_percent', 'vocabulary', 'distribution', 'query_shapes', 'reader_rate', 'reader_seconds', 'readers'):
                        value = getattr(args, key)
                        if value is not None:
                            workload.extend(['--' + key.replace('_', '-'), str(value)])
                    run(['python3', str(harness), *workload, '--docs', str(args.docs), '--repeat', str(args.repeat),
                         '--scenario', args.scenario, '--artifact', str(library),
                         '--output', str(output / label)], label)
                else:
                    rate = [] if args.writer_rate is None else ['--writer-rate', str(args.writer_rate)]
                    run(['python3', str(harness), '--seconds', str(args.seconds), '--writers', '2',
                         '--readers', '2', '--write-buffer-docs', '32', '--repeat', str(args.repeat),
                         '--sample-ms', '50', '--max-merge-docs', '1024',
                         '--artifact', str(library), '--output', str(output / label), *rate], label)
                print('PASS ' + label, flush=True)
    finally:
        try:
            # pg_ctl can time out after the postmaster was created. Do not
            # delete a possibly live cluster merely because startup raised.
            if started or (cluster / 'data/postmaster.pid').exists():
                run(['pg_ctl', '-D', str(cluster / 'data'), '-m', 'fast', '-w', 'stop'], 'stop')
            stopped = True
        finally:
            if stopped:
                install(original)
            else:
                print('Shutdown unconfirmed; library left in place. Original retained at ' + str(original), flush=True)
            if (cluster / 'server.log').exists():
                shutil.copy2(cluster / 'server.log', output / 'server.log')
            if stopped:
                shutil.rmtree(cluster)
            else:
                print('Shutdown failed; cluster retained at ' + str(cluster), flush=True)


if __name__ == '__main__':
    main()
