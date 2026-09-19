#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Repeat mutation-rate/concurrency probes on an installed release in a private cluster.

Hold the installation/timing lock around the whole campaign. No binary swaps or
builds occur here. Each window gets a fresh database; failures retain the stopped
cluster for diagnosis. See docs/benchmarks/sustained-mutation.md.
"""
import argparse
import hashlib
import itertools
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid

import run as bench


def positive_list(text):
    try:
        values = [bench.positive(item) for item in text.split(',')]
    except (ValueError, argparse.ArgumentTypeError) as error:
        raise argparse.ArgumentTypeError('expected comma-separated positive integers') from error
    if len(set(values)) != len(values):
        raise argparse.ArgumentTypeError('duplicate values would repeat a mislabeled case')
    return values


def schedule(rates, writers, rounds):
    cases = list(itertools.product(rates, writers))
    # Reverse each repetition to reduce systematic load-order effects.
    return [(r, rate, count) for r in range(1, rounds + 1)
            for rate, count in (cases if r % 2 else reversed(cases))]


def verify_sources(expected):
    for filename, digest in expected.items():
        if hashlib.sha256(Path(filename).read_bytes()).hexdigest() != digest:
            raise RuntimeError('benchmark source changed during campaign: ' + filename)


def wal_delta(before, after):
    def value(lsn):
        high, low = lsn.split('/')
        return (int(high, 16) << 32) + int(low, 16)
    delta = value(after) - value(before)
    if delta < 0:
        raise ValueError('WAL position moved backwards')
    return delta


def result_row(directory, repetition, rate, writers):
    summary = json.loads((directory / 'summary.json').read_text())
    manifest = json.loads((directory / 'manifest.json').read_text())
    if manifest['status'] != 'complete':
        raise ValueError('cannot summarize an incomplete run as successful')
    before = json.loads((directory / 'before.json').read_text())
    after = json.loads((directory / 'after.json').read_text())
    drained = json.loads((directory / 'after-drain.json').read_text())
    return dict(wal_bytes_traffic_and_observers=wal_delta(before['wal_lsn'], after['wal_lsn']),
                wal_bytes_drain=wal_delta(after['wal_lsn'], drained['wal_lsn']),
                directory=str(directory), engine=manifest["engine"], index_definition=manifest["index_definition"],
                repetition=repetition, write_rate=rate, writers=writers,
                reader=summary['reader'], writer=summary['writer'], affected_rows=summary['affected_rows'],
                maintenance=summary['maintenance'], drain=summary['drain'])


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', type=Path, required=True)
    p.add_argument('--artifact', type=Path, required=True)
    p.add_argument('--engines', default='stannum', help='Comma-separated stannum,gin,gin-no-fastupdate variants')
    p.add_argument('--profile', choices=('mutation', 'mutation-count'), default='mutation')
    p.add_argument('--rows', type=bench.positive, default=10000)
    p.add_argument('--body-repeat', type=bench.positive, default=1)
    p.add_argument('--dataset', type=Path)
    p.add_argument('--seconds', type=bench.positive, default=60)
    p.add_argument('--warmup', type=bench.positive, default=3)
    p.add_argument('--read-rate', type=bench.positive, default=100)
    p.add_argument('--clients', type=bench.positive, default=2)
    p.add_argument('--write-rates', type=positive_list, default=[100, 1000, 3000])
    p.add_argument('--writer-counts', type=positive_list, default=[1, 2])
    p.add_argument('--rounds', type=bench.positive, default=3)
    p.add_argument('--vacuum-interval', type=bench.positive, default=10)
    p.add_argument('--check-interval', type=bench.positive, default=15)
    p.add_argument('--set', action='append', default=[])
    args = p.parse_args()
    engines = args.engines.split(',')
    if len(set(engines)) != len(engines) or any(e not in ('stannum', 'gin', 'gin-no-fastupdate') for e in engines):
        p.error('engines must be distinct stannum,gin,gin-no-fastupdate variants')
    if any(e != 'stannum' for e in engines) and args.profile != 'mutation-count':
        p.error('GIN comparisons require --profile mutation-count')
    if args.rows % 1000:
        p.error('rows must be a multiple of 1000')
    if args.dataset and args.body_repeat != 1:
        p.error('body-repeat applies only to synthetic fixtures')
    if args.seconds <= max(2 * args.vacuum_interval, args.check_interval):
        p.error('duration must allow two VACUUM cycles and one oracle round during traffic')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    artifact = args.artifact.resolve(strict=True)
    installed = Path(bench.command(['pg_config', '--pkglibdir'])) / ('stannum' + artifact.suffix)
    digest = hashlib.sha256(artifact.read_bytes()).hexdigest()
    postgres = Path(bench.command(['pg_config', '--bindir'])) / 'postgres'
    postgres_digest = hashlib.sha256(postgres.read_bytes()).hexdigest()

    sources = {str(Path(module.__file__).resolve()): hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest()
               for module in (bench, bench.mutation, bench.dataset)}

    def verify_binary():
        verify_sources(sources)
        if hashlib.sha256(installed.read_bytes()).hexdigest() != digest:
            raise RuntimeError('installed library does not match the retained release artifact')

    verify_binary()
    cluster = Path(tempfile.mkdtemp(prefix='stannum-sustained-'))
    env = dict(os.environ, PGHOST=str(cluster), PGPORT='28996', PGDATABASE='postgres', PGUSER='sustained_bench')
    for key in ('PGOPTIONS', 'PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(key, None)
    settings = ['stannum.write_buffer_docs=256', 'stannum.build_segment_docs=2000',
                'stannum.max_segments=16', 'stannum.max_merge_docs=2048', *args.set]
    record = dict(status='running', artifact_sha256=digest, installed_library=str(installed),
                  harness_sources=sources, postgres_sha256=postgres_digest, launcher_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                  cluster=str(cluster), config={k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
                  settings=settings, trials=[])
    bench.save(output / 'campaign.json', record)

    def command(argv, label, timeout=240):
        with (output / f'{label}.log').open('w') as log:
            subprocess.run(argv, env=env, check=True, stdout=log, stderr=subprocess.STDOUT, timeout=timeout)

    started = False
    try:
        command(['initdb', '-D', str(cluster / 'data'), '-U', 'sustained_bench', '-A', 'trust',
                 '--no-locale', '--encoding=UTF8'], 'initdb')
        with (cluster / 'data/postgresql.conf').open('a') as config:
            config.write(f"\nlisten_addresses=''\nport=28996\nunix_socket_directories='{cluster}'\n"
                         "shared_buffers='512MB'\njit=off\ncheckpoint_timeout='30min'\nmax_wal_size='16GB'\n")
        command(['pg_ctl', '-D', str(cluster / 'data'), '-l', str(cluster / 'server.log'), '-w', 'start'], 'start')
        started = True
        trials = [(r, rate, count, engine)
                  for r, rate, count in schedule(args.write_rates, args.writer_counts, args.rounds)
                  for engine in (engines if r % 2 else list(reversed(engines)))]
        for repetition, rate, writers, engine in trials:
            verify_binary()
            label = f'{engine}-round{repetition}-rate{rate}-writers{writers}'
            database = 'stannum_bench_sustained_' + uuid.uuid4().hex
            command(['createdb', database], label + '-create')
            command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', 'CHECKPOINT'], label + '-checkpoint')
            argv = [sys.executable, str(Path(__file__).with_name('run.py')), 'run', '--engine', 'stannum' if engine == 'stannum' else 'gin',
                    '--profile', args.profile, '--database', database, '--output', str(output / label),
                    '--environment', 'private-native-postgres',
                    '--build-id', digest if engine == 'stannum' else postgres_digest,
                    '--artifact', str(artifact if engine == 'stannum' else postgres),
                    '--rows', str(args.rows), '--body-repeat', str(args.body_repeat),
                    '--seconds', str(args.seconds), '--warmup', str(args.warmup),
                    '--read-rate', str(args.read_rate), '--write-rate', str(rate), '--writers', str(writers),
                    '--clients', str(args.clients), '--vacuum-interval', str(args.vacuum_interval),
                    '--check-interval', str(args.check_interval), '--sample-interval', '2', '--bucket-seconds', '5',
                    '--drain-vacuums', '2', '--min-vacuums', '2', '--min-checks', '1',
                    '--set', 'enable_seqscan=off']
            if args.dataset:
                argv += ['--dataset', str(args.dataset.resolve())]
            if engine != 'stannum':
                argv += ['--gin-fastupdate', 'off' if engine == 'gin-no-fastupdate' else 'on']
            for setting in (settings if engine == 'stannum' else args.set):
                argv += ['--set', setting]
            print('START ' + label, flush=True)
            command(argv, label, timeout=args.seconds + 900)
            record['trials'].append(result_row(output / label, repetition, rate, writers))
            bench.save(output / 'campaign.json', record)
            command(['dropdb', database], label + '-drop')
            print('PASS ' + label, flush=True)
        verify_binary()
        record['status'] = 'complete'
    except BaseException as error:
        record['status'] = 'failed'
        record['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        try:
            # A timed-out start can still have created a live postmaster.
            if started or (cluster / 'data/postmaster.pid').exists():
                command(['pg_ctl', '-D', str(cluster / 'data'), '-m', 'fast', '-w', 'stop'], 'stop')
            if (cluster / 'server.log').exists():
                shutil.copy2(cluster / 'server.log', output / 'server.log')
            if record['status'] == 'complete':
                shutil.rmtree(cluster)
        except BaseException as error:
            record['status'] = 'failed'
            record['cleanup_error'] = f'{type(error).__name__}: {error}'
            raise
        finally:
            bench.save(output / 'campaign.json', record)


if __name__ == '__main__':
    main()
