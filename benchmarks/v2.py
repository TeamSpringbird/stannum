#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Benchmarks v2: TIN v1.0.6's published setup, Stannum beside ParadeDB 0.26.0.

  v2.py campaign ...   every scenario x variant x CPU reading, through tin.py run
  v2.py report DIR     the comparison with the published numbers

A campaign runs the four published scenarios (conjunction, disjunction,
phrase, mixed) on the 1,254-query Stack Exchange trace: Stannum with
stannum.score (TIN) and stannum.full_score (TIN_FULL), and ParadeDB 0.26.0
through its |||, &&&, ### operators, the calibration anchor. Our ParadeDB
QPS over PlanetScale's published ParadeDB QPS is a per-scenario hardware
factor; Stannum's QPS over that factor estimates Stannum on their machine.
See "Matching TIN v1.0.6's published setup" in docs/benchmarks.md.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / 'benchmarks'))
import tin  # noqa: E402

SCENARIOS = ('conjunction', 'disjunction', 'phrase', 'mixed')
# (engine, score function, our label, the published engine it corresponds to)
VARIANTS = (('stannum', 'score', 'Stannum', 'TIN'),
            ('stannum', 'full_score', 'Stannum full_score', 'TIN_FULL'),
            ('paradedb', 'score', 'ParadeDB 0.26.0 (ours)', 'ParadeDB'))
PUBLISHED = ROOT / 'benchmarks/published/tin-results.json'


def run_directory(output, layout, engine, score, scenario):
    variant = engine if engine == 'paradedb' else f'{engine}-{score}'
    return Path(output) / layout / f'{variant}-{scenario}'


def layouts_to_run(lscpu_text, layouts, cpus=8):
    """Every requested reading of "8 vCPUs" that gives a distinct cpuset; on a host
    without SMT (Graviton, the Docker VM on a Mac) both readings are the same CPUs."""
    chosen, seen = [], {}
    for layout in layouts:
        if layout == 'none':
            chosen.append(layout)
            continue
        cpuset = tin.plan_cpus(lscpu_text, cpus, layout)['server_cpuset']
        if cpuset in seen:
            print(f'# {layout}: the same CPUs ({cpuset}) as {seen[cpuset]}; not run again', flush=True)
            continue
        seen[cpuset] = layout
        chosen.append(layout)
    return chosen


def planned_runs(layouts, scenarios, engines, score_functions):
    return [(layout, engine, score, scenario)
            for layout in layouts for scenario in scenarios
            for engine, score, _, _ in VARIANTS
            if engine in engines and (engine == 'paradedb' or score in score_functions)]


def tin_arguments(args, layout, engine, score, scenario, output):
    database = args.stannum_database if engine == 'stannum' else args.paradedb_database
    command = [sys.executable, str(ROOT / 'benchmarks/tin.py'), '--driver', str(args.driver), 'run',
               '--published-corpus', 'stackexchange', '--dataset', str(args.dataset),
               '--rows', str(args.rows), '--validation-rows', '1000',
               '--validation-queries', str(args.validation_queries),
               '--ranked-validation-queries', str(args.ranked_validation_queries),
               '--engines', engine, '--workload', 'topk', '--style', scenario,
               '--score-function', score, '--profile', args.profile, '--cpu-layout', layout,
               '--seconds', str(args.seconds), '--warmup', str(args.warmup),
               '--setup-timeout-seconds', str(args.setup_timeout_seconds), '--port', str(args.port),
               '--image', args.image, '--paradedb-image', args.paradedb_image, '--output', str(output)]
    if database:
        command += ['--load-database', str(database)]
    if args.source_manifest:
        command += ['--source-manifest', str(args.source_manifest)]
    if args.lscpu_file:
        command += ['--lscpu-file', str(args.lscpu_file)]
    return command + list(args.extra)


def drop_caches_at_measurement(output, stop):
    """On the Mac's Docker VM the copied database and the validation leave the index in the
    VM's page cache, uncharged to the container: drop it as measurement starts (as
    benchmarks/local/workload.sh does), so the run reads cold but for its shared buffers."""
    while not stop.is_set():
        for log in Path(output).glob('*/resources.jsonl'):
            lines = log.read_text().splitlines()
            if lines and 'driver-warmup-and-measurement' in lines[-1]:
                subprocess.run(['docker', 'run', '--rm', '--privileged', 'alpine', 'sh', '-c',
                                'sync; echo 3 > /proc/sys/vm/drop_caches'], check=False)
                (Path(str(output) + '.caches-dropped')).write_text(time.strftime('%Y-%m-%d %H:%M:%S\n'))
                return
        stop.wait(1)


def campaign(args):
    pinned = [layout for layout in args.layouts if layout != 'none']
    text = None
    if pinned:
        text = Path(args.lscpu_file).read_text() if args.lscpu_file else tin.cpu_topology(args.image)['lscpu']
    layouts = layouts_to_run(text, args.layouts) if text else list(args.layouts)
    failures = []
    for layout, engine, score, scenario in planned_runs(layouts, args.scenarios, args.engines, args.score_functions):
        output = run_directory(args.output, layout, engine, score, scenario)
        command = tin_arguments(args, layout, engine, score, scenario, output)
        if args.dry_run:
            print(f'# {output}', flush=True)
            subprocess.run(command + ['--dry-run'], check=True)
            continue
        manifest = output / 'manifest.json'
        if manifest.exists() and json.loads(manifest.read_text()).get('status') == 'complete':
            print(f'== {output}: complete; skipped', flush=True)
            continue
        if output.exists():
            output.rename(output.with_name(output.name + f'.failed-{int(time.time())}'))
        output.parent.mkdir(parents=True, exist_ok=True)
        print(f'== {output} {time.strftime("%Y-%m-%d %H:%M:%S")}', flush=True)
        stop = threading.Event()
        watcher = threading.Thread(target=drop_caches_at_measurement, args=(output, stop), daemon=True)
        if args.drop_caches:
            watcher.start()
        with open(str(output) + '.log', 'w') as log:
            status = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
        stop.set()
        if status:
            failures.append(str(output))
            print(f'!! {output} failed ({status}); see {output}.log', flush=True)
    if not args.dry_run:
        report(argparse.Namespace(campaign=args.output, published=args.published, anchor=None))
    if failures:
        raise SystemExit('failed runs: ' + ', '.join(failures))


def load_run(path):
    if not (path / 'comparison.json').exists():
        return dict(status='missing')
    manifest = json.loads((path / 'manifest.json').read_text())
    row = json.loads((path / 'comparison.json').read_text())[0]
    if manifest.get('status') != 'complete' or row.get('status') != 'complete':
        return dict(status='incomplete')
    return dict(status='complete', qps=row['qps'], p50_ms=row['p50_ms'], p99_ms=row['p99_ms'],
                mib_per_query=row.get('index_mib_per_query'), seconds=row['seconds'],
                index_bytes=row['index_bytes'], cpuset=(manifest.get('cpu_pinning') or {}).get('server_cpuset'),
                postgres=manifest.get('postgres_version'), profile=manifest.get('profile'))


def comparison(campaign_dir, published, anchor_dir=None):
    """{layout: {scenario: {hardware_factor, rows}}}: ours beside PlanetScale's v1.0.6 numbers.
    ParadeDB runs missing from the campaign are read from anchor_dir (a campaign on the same
    host, such as the runtime-dispatch one when this one measures an x86-64-v4 build)."""
    datasets = {d['id']: d for d in published['datasets']}
    reference = {'x86': datasets['tin-1.0.6-x86']['results'], 'arm': datasets['tin-1.0.6-arm']['results']}
    result = {}
    for layout_dir in sorted(p for p in Path(campaign_dir).iterdir() if p.is_dir() and p.name in tin.CPU_LAYOUTS):
        layout = result.setdefault(layout_dir.name, {})
        for scenario in SCENARIOS:
            measured = {}
            for engine, score, label, theirs in VARIANTS:
                ours = load_run(run_directory(campaign_dir, layout_dir.name, engine, score, scenario))
                if ours['status'] == 'missing' and anchor_dir and engine == 'paradedb':
                    ours = dict(load_run(run_directory(anchor_dir, layout_dir.name, engine, score, scenario)),
                                from_campaign=str(anchor_dir))
                measured[theirs] = (label, ours)
            anchor = measured['ParadeDB'][1]
            factors = {platform: (anchor['qps'] / results[scenario]['ParadeDB']['qps']
                                  if anchor['status'] == 'complete' else None)
                       for platform, results in reference.items()}
            rows = []
            for theirs, (label, ours) in measured.items():
                row = dict(ours=label, theirs=theirs, measured=ours,
                           published={p: r[scenario].get(theirs) for p, r in reference.items()})
                if ours['status'] == 'complete':
                    row['estimated_qps_on'] = {p: ours['qps'] / f for p, f in factors.items() if f}
                rows.append(row)
            layout[scenario] = dict(hardware_factor=factors, rows=rows)
    return result


def fmt(value, digits=0):
    return '—' if value is None else f'{value:,.{digits}f}'


def render(result, published):
    sources = ', '.join(sorted({d['source'] for d in published['datasets'] if d['id'].startswith('tin-1.0.6')}))
    lines = ["# Benchmarks v2: Stannum against TIN v1.0.6's published numbers", '',
             f'Published numbers: {sources} (benchmarks/published/tin-results.json).', '',
             "Hardware factor = our ParadeDB 0.26.0 QPS / PlanetScale's published ParadeDB 0.26.0 QPS, per",
             'scenario and platform. "Est. QPS" = our QPS / that factor: the QPS that keeps our ratio to',
             "ParadeDB on their machine. MiB/query is the benchmarker's PER QUERY, which the post reports",
             'as MB/query: (index blocks read + hit) x 8 KiB / completed queries, warm-up traffic included',
             'in the blocks. It is logical block traffic, not disk reads.', '']
    for layout, scenarios in result.items():
        lines += [f'## CPU reading: {layout}', '']
        for scenario, data in scenarios.items():
            factor = data['hardware_factor']
            lines += [f'### {scenario} (hardware factor: i7i {fmt(factor["x86"], 2)}, i8g {fmt(factor["arm"], 2)})', '',
                      '| Ours | QPS | p99 ms | MiB/query | Published | i7i QPS / p99 ms / MB | i8g QPS / p99 ms / MB '
                      '| Est. QPS on i7i | Est. QPS on i8g |',
                      '| --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |']
            for row in data['rows']:
                m = row['measured']
                pub = {p: (f"{v['qps']:,} / {v['p99_ms']:,} / {v['mb_per_query']}" if v else '—')
                       for p, v in row['published'].items()}
                est = row.get('estimated_qps_on', {})
                ours = (f"{fmt(m['qps'], 1)} | {fmt(m['p99_ms'], 1)} | {fmt(m['mib_per_query'], 1)}"
                        if m['status'] == 'complete' else f"{m['status']} | | ")
                lines.append(f"| {row['ours']} | {ours} | {row['theirs']} | {pub['x86']} | {pub['arm']} | "
                             f"{fmt(est.get('x86'))} | {fmt(est.get('arm'))} |")
            lines.append('')
    return '\n'.join(lines) + '\n'


def report(args):
    published = json.loads(Path(args.published).read_text())
    result = comparison(args.campaign, published, getattr(args, 'anchor', None))
    Path(args.campaign, 'v2-report.json').write_text(json.dumps(result, indent=1) + '\n')
    text = render(result, published)
    Path(args.campaign, 'v2-report.md').write_text(text)
    print(text, end='')


def parser():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='command', required=True)
    c = sub.add_parser('campaign', help='run (or, with --dry-run, print) every scenario, variant and CPU reading')
    c.add_argument('--driver', type=Path, required=True)
    c.add_argument('--dataset', type=Path, required=True)
    c.add_argument('--image', required=True, help='the Stannum benchmark image')
    c.add_argument('--paradedb-image', default=tin.PARADEDB_IMAGE)
    c.add_argument('--source-manifest', type=Path)
    c.add_argument('--stannum-database', type=Path, help='Stannum database saved by tin.py run --save-database')
    c.add_argument('--paradedb-database', type=Path, help='ParadeDB database saved by tin.py run --save-database')
    c.add_argument('--output', type=Path, required=True)
    c.add_argument('--rows', type=int, default=150000000)
    c.add_argument('--profile', default='v2', choices=sorted(tin.PROFILES))
    c.add_argument('--layouts', nargs='+', default=['siblings', 'distinct-cores'], choices=tin.CPU_LAYOUTS,
                   help='readings of "8 vCPUs" to measure; a reading with the same CPUs as an earlier one is skipped')
    c.add_argument('--lscpu-file', type=Path, help='topology of another host, for --dry-run')
    c.add_argument('--scenarios', nargs='+', default=list(SCENARIOS), choices=SCENARIOS)
    c.add_argument('--engines', nargs='+', default=['stannum', 'paradedb'], choices=['stannum', 'paradedb'])
    c.add_argument('--score-functions', nargs='+', default=['score', 'full_score'], choices=['score', 'full_score'])
    c.add_argument('--seconds', type=int, default=600)
    c.add_argument('--warmup', type=int, default=10)
    c.add_argument('--validation-queries', type=int, default=10)
    c.add_argument('--ranked-validation-queries', type=int, default=2)
    c.add_argument('--setup-timeout-seconds', type=int, default=21600)
    c.add_argument('--port', type=int, default=28928)
    c.add_argument('--drop-caches', action='store_true',
                   help="drop the Docker VM's page cache as each measurement starts (local runs on a Mac)")
    c.add_argument('--published', type=Path, default=PUBLISHED)
    c.add_argument('--dry-run', action='store_true')
    c.add_argument('extra', nargs='*', help='more tin.py run arguments, after --')
    c.set_defaults(func=campaign)
    r = sub.add_parser('report', help='compare a campaign with the published numbers')
    r.add_argument('campaign', type=Path)
    r.add_argument('--published', type=Path, default=PUBLISHED)
    r.add_argument('--anchor', type=Path, help="campaign whose ParadeDB runs calibrate this one's")
    r.set_defaults(func=report)
    return p


def main():
    args = parser().parse_args()
    args.func(args)


if __name__ == '__main__':
    main()
