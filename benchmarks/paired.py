#!/usr/bin/env python3
"""Compare two immutable Stannum images locally with alternating paired trials."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import uuid

import campaign
import run as bench


def report(root, jobs):
    pairs = []
    invalid = []
    for number in sorted({j['pair'] for j in jobs}):
        selected = {j['variant']: j for j in jobs if j['pair'] == number}
        if any(j['status'] != 'complete' for j in selected.values()) or len(selected) != 2:
            continue
        paths = [root / selected[v]['directory'] for v in ('original', 'fork')]
        manifests = [json.loads((p / 'manifest.json').read_text()) for p in paths]
        mismatches = bench.comparison_mismatches(*manifests)
        if mismatches:
            invalid.append({'pair': number, 'reason': f'Incomparable: {mismatches}'})
            continue
        summaries = [json.loads((p / 'summary.json').read_text()) for p in paths]
        pressures = [campaign.pressure(p) for p in paths]
        if any(p['oom_events'] or p['oom_kills'] for p in pressures):
            invalid.append({'pair': number, 'reason': 'OOM during timed traffic'})
            continue
        pairs.append({'pair': number, 'manifests': manifests, 'summaries': summaries,
                      'pressure': pressures,
                      'sizes': [json.loads((p / 'after.json').read_text())['index_bytes'] for p in paths]})
    complete = len(pairs) == len({j['pair'] for j in jobs})
    result = {'complete_pairs': len(pairs), 'complete': complete, 'invalid_pairs': invalid,
              'jobs': jobs, 'queries': {}, 'variants': {}}
    lines = ['# Paired Stannum comparison', '',
             'Identical synthetic documents, SQL, runtime settings and scheduled writes. Fresh container and volume per trial; order alternates each pair.',
             'Each pair shares a random seed. All runs are retained, including failures. This is a small diagnostic benchmark, not a production or competitor claim.', '',
             f'Complete pairs: {len(pairs)} / {len({j["pair"] for j in jobs})}.', '',
             '| Query | Original p50 ms | Fork p50 ms | Median paired p50 speedup | Original p95 ms | Fork p95 ms |',
             '| --- | ---: | ---: | ---: | ---: | ---: |']
    if not complete:
        lines += ['', '**Incomplete comparison: aggregate speedups are withheld until every planned pair is valid.**']
    if pairs and complete:
        for name in pairs[0]['manifests'][0]['query_names']:
            metrics = {}
            for metric in ('p50_ms', 'p95_ms', 'completed_per_second'):
                metrics[metric] = [campaign.describe([p['summaries'][i]['reader']['queries'][name][metric] for p in pairs]) for i in (0, 1)]
            metrics['paired_p50_speedup'] = campaign.describe([
                p['summaries'][0]['reader']['queries'][name]['p50_ms'] / p['summaries'][1]['reader']['queries'][name]['p50_ms'] for p in pairs])
            result['queries'][name] = metrics
            lines.append(f"| {name} | {metrics['p50_ms'][0]['median']:.3f} | {metrics['p50_ms'][1]['median']:.3f} | {metrics['paired_p50_speedup']['median']:.2f}x | {metrics['p95_ms'][0]['median']:.3f} | {metrics['p95_ms'][1]['median']:.3f} |")
        lines += ['', '| Variant | Read QPS median | Trimmed mean | Min–max | Write QPS | Write p95 ms | Build seconds | Total index bytes |',
                  '| --- | ---: | ---: | --- | ---: | ---: | ---: | ---: |']
        for i, variant in enumerate(('original', 'fork')):
            values = {
                'read_qps': [sum(q['completed_per_second'] for q in p['summaries'][i]['reader']['queries'].values()) for p in pairs],
                'write_qps': [p['summaries'][i]['writer']['queries']['update']['completed_per_second'] for p in pairs],
                'write_p95_ms': [p['summaries'][i]['writer']['queries']['update']['p95_ms'] for p in pairs],
                'build_seconds': [p['manifests'][i]['index_build_seconds'] for p in pairs],
                'total_index_bytes': [p['sizes'][i] for p in pairs],
            }
            stats = {k: campaign.describe(v) for k, v in values.items()}
            result['variants'][variant] = stats
            q = stats['read_qps']
            trimmed = f"{q['trimmed_mean']:.2f}" if q['trimmed_mean'] is not None else '—'
            lines.append(f"| {variant} | {q['median']:.2f} | {trimmed} | {q['min']:.2f}–{q['max']:.2f} | {stats['write_qps']['median']:.2f} | {stats['write_p95_ms']['median']:.3f} | {stats['build_seconds']['median']:.3f} | {stats['total_index_bytes']['median']:.0f} |")
    result['resource_pressure'] = [{'pair': p['pair'], 'original': p['pressure'][0], 'fork': p['pressure'][1]} for p in pairs]
    lines += ['', 'Trimmed means remove one minimum and maximum per metric with five or more samples. Query throughput shares the same mixed reader stream; it is not isolated-query capacity.',
              'Index bytes include the primary key. Correctness before and after writes, query plans, source fingerprints, image IDs, raw transaction logs and cgroup counters are retained alongside this report.']
    bench.save(root / 'aggregate.json', result)
    (root / 'report.md').write_text('\n'.join(lines) + '\n')
    return complete


def execute(args):
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=False)
    protocol = root / 'protocol'
    protocol.mkdir()
    for name in ('paired.py', 'run.py', 'campaign.py', 'dataset.py', 'Dockerfile', 'Dockerfile.dockerignore'):
        shutil.copy2(Path(__file__).parent / name, protocol / name)
    recipe = bench.digest((protocol / 'Dockerfile').read_bytes() + (protocol / 'Dockerfile.dockerignore').read_bytes())
    images = {}
    for variant in ('original', 'fork'):
        source = json.loads(Path(getattr(args, variant + '_source')).read_text())
        bench.save(root / (variant + '-source.json'), source)
        image = json.loads(campaign.docker('image', 'inspect', getattr(args, variant + '_image')))[0]
        labels = image['Config'].get('Labels', {})
        if image['Architecture'] != 'arm64' or labels.get('benchmark.stannum_source_sha256') != source['source_sha256'] or labels.get('benchmark.recipe_sha256') != recipe:
            raise ValueError(f'{variant} image source, architecture or recipe mismatch')
        images[variant] = image['Id']
        bench.save(root / (variant + '-image.json'), image)
    info = json.loads(campaign.docker('info', '--format', '{{json .}}'))
    context = {'protocol': 'paired-docker-v1', 'cpus': args.cpus, 'cpuset': args.cpuset,
               'memory': args.memory, 'swap': 'disabled', 'shm': '1g',
               'postgres_settings': campaign.PG_SETTINGS, 'recipe_sha256': recipe,
               'runner_sha256': bench.digest(Path(__file__).read_bytes()),
               'vm': {k: info.get(k) for k in ('NCPU', 'MemTotal', 'Architecture', 'OperatingSystem', 'ServerVersion', 'KernelVersion')}}
    bench.save(root / 'context.json', context)
    jobs = [{'pair': n, 'variant': v, 'directory': f'r{n:02d}-{v}', 'status': 'pending'}
            for n in range(1, args.repetitions + 1)
            for v in (('original', 'fork') if n % 2 else ('fork', 'original'))]
    manifest = {'images': images, 'config': vars(args), 'jobs': jobs, 'status': 'running'}
    bench.save(root / 'paired.json', manifest)
    try:
        for job in jobs:
            name = 'stannum-paired-' + uuid.uuid4().hex[:12]
            volume = name + '-data'
            directory = job['directory']
            job['status'] = 'running'
            bench.save(root / 'paired.json', manifest)
            print(directory, flush=True)
            try:
                (root / (directory + '-background.txt')).write_text(campaign.docker('stats', '--no-stream', '--format', '{{.Name}} {{.CPUPerc}} {{.MemUsage}}'))
                campaign.start_server(images[job['variant']], name, volume, args)
                env = dict(os.environ, PGHOST='127.0.0.1', PGPORT=str(args.port), PGUSER='postgres')
                for key in ('PGOPTIONS', 'PGSERVICE', 'PGSERVICEFILE', 'PGDATABASE'):
                    env.pop(key, None)
                command = [sys.executable, str(protocol / 'run.py'), 'run', '--engine', 'stannum', '--profile', args.profile,
                           '--database', 'stannum_bench_campaign', '--output', str(root / directory),
                           '--environment', 'paired-' + bench.digest(bench.canonical(context))[:12],
                           '--build-id', images[job['variant']], '--source-manifest', str(root / (job['variant'] + '-source.json')),
                           '--context', str(root / 'context.json'), '--container', name,
                           '--seed', str(args.seed + 1009 * (job['pair'] - 1)), '--label', 'paired-stannum']
                for flag in ('rows', 'seconds', 'warmup', 'clients', 'write_rate', 'statement_timeout_ms'):
                    command += ['--' + flag.replace('_', '-'), str(getattr(args, flag))]
                with (root / (directory + '-runner.log')).open('w') as log:
                    subprocess.run(command, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
                job['status'] = 'complete'
            except Exception as error:
                job.update(status='failed', error=str(error))
                print(f'{directory}: {error}', flush=True)
            finally:
                logs = subprocess.run(['docker', 'logs', name], capture_output=True, text=True)
                (root / (directory + '-server.log')).write_text(logs.stdout + logs.stderr)
                # A failed removal can leave competing traffic or occupy the port.
                # Stop explicitly rather than silently contaminating later pairs.
                subprocess.run(['docker', 'rm', '-f', name], capture_output=True, check=True)
                subprocess.run(['docker', 'volume', 'rm', volume], capture_output=True, check=True)
                bench.save(root / 'paired.json', manifest)
                report(root, jobs)
        manifest['status'] = 'complete' if report(root, jobs) else 'incomplete'
    finally:
        if manifest['status'] == 'running':
            manifest['status'] = 'interrupted'
        bench.save(root / 'paired.json', manifest)
    return manifest['status'] == 'complete'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ('output', 'original_image', 'original_source', 'fork_image', 'fork_source'):
        parser.add_argument('--' + flag.replace('_', '-'), required=True)
    for flag, default in (('rows', 10000), ('seconds', 30), ('warmup', 5), ('clients', 2),
                          ('write_rate', 20), ('seed', 1729), ('repetitions', 5), ('statement_timeout_ms', 60000), ('port', 28919)):
        parser.add_argument('--' + flag.replace('_', '-'), type=bench.positive, default=default)
    parser.add_argument('--profile', choices=('count', 'mixed'), default='count')
    parser.add_argument('--cpus', type=int, default=4)
    parser.add_argument('--cpuset', default='0-3')
    parser.add_argument('--memory', default='4g')
    args = parser.parse_args()
    # Separate from the baseline lock: a supervised pause can retain that lock.
    # Other benchmark traffic must be stopped or paused between trials first.
    lock_path = bench.ROOT / 'benchmarks/results/.paired.lock'
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        def terminate(_signum, _frame):
            raise KeyboardInterrupt('Paired benchmark terminated')
        signal.signal(signal.SIGTERM, terminate)
        awake = subprocess.Popen(['caffeinate', '-i']) if sys.platform == 'darwin' else None
        try:
            success = execute(args)
        finally:
            if awake:
                awake.terminate()
                awake.wait()
    sys.exit(0 if success else 1)


if __name__ == '__main__':
    main()
