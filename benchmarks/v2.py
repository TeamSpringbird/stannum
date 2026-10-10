#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Benchmarks v2: Stannum under TIN v1.0.6's published setup.

  v2.py campaign ...   every scenario x scoring x CPU reading, through tin.py run
  v2.py report DIR     the campaign beside the published numbers

A campaign runs the four published scenarios (conjunction, disjunction,
phrase, mixed) on the 1,254-query Stack Exchange trace, with stannum.score
(TIN's default, tin.score) and stannum.full_score (TIN_FULL), from one saved
database. The report sets each run beside PlanetScale's published TIN,
TIN_FULL and ParadeDB 0.26.0 numbers (benchmarks/published/tin-results.json).

Locally, and only there, --paradedb-database adds ParadeDB 0.26.0 runs as a
one-off calibration anchor: our ParadeDB QPS over the published ParadeDB QPS,
per scenario, is a rough hardware factor against each published machine, and
the report scales Stannum by it and shows how much the factor varies across
scenarios. AWS runs never include ParadeDB. See "Matching TIN v1.0.6's
published setup" in docs/benchmarks.md.
"""
import argparse
import json
from pathlib import Path
import statistics
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
# (key, published dataset, instance) in the order the report shows them: the ARM
# table first, the closer architecture to an Apple Silicon Mac.
PLATFORMS = (('arm', 'tin-1.0.6-arm', 'i8g'), ('x86', 'tin-1.0.6-x86', 'i7i'))


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


def planned_runs(layouts, scenarios, score_functions, paradedb=False):
    return [(layout, engine, score, scenario)
            for layout in layouts for scenario in scenarios
            for engine, score, _, _ in VARIANTS
            if (engine == 'stannum' and score in score_functions) or (engine == 'paradedb' and paradedb)]


def tin_arguments(args, layout, engine, score, scenario, output):
    database = args.paradedb_database if engine == 'paradedb' else args.database
    command = [sys.executable, str(ROOT / 'benchmarks/tin.py'), '--driver', str(args.driver), 'run',
               '--published-corpus', 'stackexchange', '--rows', str(args.rows), '--validation-rows', '1000',
               '--validation-queries', str(args.validation_queries),
               '--ranked-validation-queries', str(args.ranked_validation_queries),
               '--engines', engine, '--workload', 'topk', '--style', scenario,
               '--score-function', score, '--profile', args.profile, '--cpu-layout', layout,
               '--seconds', str(args.seconds), '--warmup', str(args.warmup),
               '--setup-timeout-seconds', str(args.setup_timeout_seconds), '--port', str(args.port),
               '--load-database', str(database), '--image', args.image, '--output', str(output)]
    if engine == 'paradedb':
        command += ['--paradedb-image', args.paradedb_image]
    if args.dataset:
        command += ['--dataset', str(args.dataset)]
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
                Path(str(output) + '.caches-dropped').write_text(time.strftime('%Y-%m-%d %H:%M:%S\n'))
                return
        stop.wait(1)


def campaign(args):
    pinned = [layout for layout in args.layouts if layout != 'none']
    text = None
    if pinned:
        text = Path(args.lscpu_file).read_text() if args.lscpu_file else tin.cpu_topology(args.image)['lscpu']
    layouts = layouts_to_run(text, args.layouts) if text else list(args.layouts)
    failures = []
    for layout, engine, score, scenario in planned_runs(layouts, args.scenarios, args.score_functions,
                                                         bool(args.paradedb_database)):
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
        if args.drop_caches:
            threading.Thread(target=drop_caches_at_measurement, args=(output, stop), daemon=True).start()
        with open(str(output) + '.log', 'w') as log:
            status = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT).returncode
        stop.set()
        if status:
            failures.append(str(output))
            print(f'!! {output} failed ({status}); see {output}.log', flush=True)
    if not args.dry_run:
        report(argparse.Namespace(campaign=args.output, published=args.published))
    if failures:
        raise SystemExit('failed runs: ' + ', '.join(failures))


def load_run(path):
    if not (path / 'comparison.json').exists():
        return dict(status='missing')
    manifest = json.loads((path / 'manifest.json').read_text())
    row = json.loads((path / 'comparison.json').read_text())[0]
    if manifest.get('status') != 'complete' or row.get('status') != 'complete':
        return dict(status='incomplete')
    pinning = manifest.get('cpu_pinning') or {}
    return dict(status='complete', qps=row['qps'], p50_ms=row['p50_ms'], p99_ms=row['p99_ms'],
                mib_per_query=row.get('index_mib_per_query'), seconds=row['seconds'],
                index_bytes=row['index_bytes'], cpuset=pinning.get('server_cpuset'),
                host=(manifest.get('host') or {}).get('machine'), postgres=manifest.get('postgres_version'),
                profile=manifest.get('profile'))


def spread(values):
    """How far a factor moves across scenarios: a steady factor is worth more than one that swings."""
    if len(values) < 2:
        return None
    return dict(n=len(values), min=min(values), max=max(values), max_over_min=max(values) / min(values),
                cv=statistics.stdev(values) / statistics.mean(values))


def comparison(campaign_dir, published):
    """{layout: {scenarios: {scenario: {rows, factor}}, factor_spread}}: our runs beside
    PlanetScale's v1.0.6 numbers, with a ParadeDB calibration where we measured ParadeDB."""
    datasets = {d['id']: d['results'] for d in published['datasets']}
    reference = {platform: datasets[name] for platform, name, _ in PLATFORMS}
    result = {}
    for layout_dir in sorted(p for p in Path(campaign_dir).iterdir() if p.is_dir() and p.name in tin.CPU_LAYOUTS):
        scenarios = {}
        for scenario in SCENARIOS:
            measured = {theirs: (label, load_run(run_directory(campaign_dir, layout_dir.name, engine, score, scenario)))
                        for engine, score, label, theirs in VARIANTS}
            anchor = measured['ParadeDB'][1]
            factor = {p: (anchor['qps'] / r[scenario]['ParadeDB']['qps'] if anchor['status'] == 'complete' else None)
                      for p, r in reference.items()}
            rows = []
            for theirs, (label, ours) in measured.items():
                row = dict(ours=label, theirs=theirs, measured=ours,
                           published={p: r[scenario].get(theirs) for p, r in reference.items()})
                if ours['status'] == 'complete' and theirs != 'ParadeDB' and any(factor.values()):
                    row['scaled_qps'] = {p: ours['qps'] / f for p, f in factor.items() if f}
                rows.append(row)
            scenarios[scenario] = dict(rows=rows, factor=factor)
        result[layout_dir.name] = dict(scenarios=scenarios, factor_spread={
            p: spread([s['factor'][p] for s in scenarios.values() if s['factor'][p]]) for p, _, _ in PLATFORMS})
    return result


def fmt(value, digits=0):
    return '—' if value is None else f'{value:,.{digits}f}'


def render(result, published):
    sources = ', '.join(sorted({d['source'] for d in published['datasets'] if d['id'].startswith('tin-1.0.6')}))
    lines = ["# Benchmarks v2: Stannum beside TIN v1.0.6's published numbers", '',
             f'Published numbers: {sources} (benchmarks/published/tin-results.json).', '',
             "MiB/query is the benchmarker's PER QUERY, which the post reports as MB/query: (index",
             'blocks read + hit) x 8 KiB / completed queries, warm-up traffic included in the blocks.',
             'It is logical block traffic, not disk reads.', '']
    names = {p: instance for p, _, instance in PLATFORMS}
    for layout, data in result.items():
        complete = [row['measured'] for s in data['scenarios'].values() for row in s['rows']
                    if row['measured']['status'] == 'complete']
        hosts = sorted({str(m.get('host')) for m in complete}) or ['—']
        cpusets = sorted({str(m.get('cpuset')) for m in complete}) or ['—']
        lines += [f'## CPU reading: {layout} (host {", ".join(hosts)}, server cpuset {", ".join(cpusets)})', '',
                  '| Scenario | Ours | QPS | p99 ms | MiB/query | Published | i8g QPS / p99 ms / MB | i7i QPS / p99 ms / MB |',
                  '| --- | --- | ---: | ---: | ---: | --- | ---: | ---: |']
        for scenario, s in data['scenarios'].items():
            for row in s['rows']:
                m = row['measured']
                if m['status'] == 'missing' and row['theirs'] == 'ParadeDB':
                    label, ours = '(published only)', ' | | '
                elif m['status'] == 'complete':
                    label = row['ours']
                    ours = f"{fmt(m['qps'], 1)} | {fmt(m['p99_ms'], 1)} | {fmt(m['mib_per_query'], 1)}"
                else:
                    label, ours = row['ours'], f"{m['status']} | | "
                pub = {p: (f"{v['qps']:,} / {v['p99_ms']:,} / {v['mb_per_query']}" if v else '—')
                       for p, v in row['published'].items()}
                lines.append(f"| {scenario} | {label} | {ours} | {row['theirs']} | {pub['arm']} | {pub['x86']} |")
        lines.append('')
        if not any(f for s in data['scenarios'].values() for f in s['factor'].values()):
            lines += ['No ParadeDB runs in this campaign, so no calibration.', '']
            continue
        lines += ['### Calibration against ParadeDB 0.26.0 (a rough approximation)', '',
                  "Factor = our ParadeDB QPS / PlanetScale's published ParadeDB QPS, per scenario. Stannum",
                  "scaled = our Stannum QPS / that factor: Stannum's QPS on their machine if both engines",
                  'moved alike from it to ours, which nothing guarantees. Trust it only as far as the',
                  'factor holds still across scenarios (the spread below); the i8g table is the closer',
                  'architecture to an Apple Silicon Mac.', '',
                  '| Scenario | Factor vs i8g | Factor vs i7i | Stannum scaled to i8g (TIN) | Stannum scaled to i7i (TIN) '
                  '| Stannum full_score scaled to i8g (TIN_FULL) | Stannum full_score scaled to i7i (TIN_FULL) |',
                  '| --- | ---: | ---: | ---: | ---: | ---: | ---: |']
        for scenario, s in data['scenarios'].items():
            rows = {row['theirs']: row for row in s['rows']}
            cells = []
            for theirs in ('TIN', 'TIN_FULL'):
                for p in ('arm', 'x86'):
                    scaled = rows[theirs].get('scaled_qps', {}).get(p)
                    published_qps = (rows[theirs]['published'].get(p) or {}).get('qps')
                    cells.append(f'{fmt(scaled)} ({fmt(published_qps)})' if scaled else '—')
            lines.append(f"| {scenario} | {fmt(s['factor']['arm'], 2)} | {fmt(s['factor']['x86'], 2)} | "
                         + ' | '.join(cells) + ' |')
        lines += ['', 'Scaled QPS with the published figure in parentheses.', '']
        for p, _, _ in PLATFORMS:
            sp = data['factor_spread'][p]
            if sp:
                verdict = ('steady' if sp['max_over_min'] <= 1.25 else
                           'moderate' if sp['max_over_min'] <= 1.6 else 'unsteady: do not lean on the scaled figures')
                lines.append(f"- Factor vs {names[p]} across {sp['n']} scenarios: {sp['min']:.2f} to {sp['max']:.2f} "
                             f"(max/min {sp['max_over_min']:.2f}, CV {sp['cv']:.2f}): {verdict}.")
        lines.append('')
    return '\n'.join(lines) + '\n'


def report(args):
    published = json.loads(Path(args.published).read_text())
    result = comparison(args.campaign, published)
    Path(args.campaign, 'v2-report.json').write_text(json.dumps(result, indent=1) + '\n')
    text = render(result, published)
    Path(args.campaign, 'v2-report.md').write_text(text)
    print(text, end='')


def parser():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest='command', required=True)
    c = sub.add_parser('campaign', help='run (or, with --dry-run, print) every scenario, scoring and CPU reading')
    c.add_argument('--driver', type=Path, required=True)
    c.add_argument('--image', required=True, help='the Stannum benchmark image')
    c.add_argument('--database', type=Path, required=True, help='the Stannum database saved by tin.py run --save-database')
    c.add_argument('--paradedb-database', type=Path,
                   help='local only: a saved ParadeDB 0.26.0 database; adds the calibration runs')
    c.add_argument('--paradedb-image', default=tin.PARADEDB_IMAGE)
    c.add_argument('--dataset', type=Path, help='the corpus; optional, since the saved database vouches for its import')
    c.add_argument('--source-manifest', type=Path)
    c.add_argument('--output', type=Path, required=True)
    c.add_argument('--rows', type=int, default=150000000)
    c.add_argument('--profile', default='v2', choices=sorted(tin.PROFILES))
    c.add_argument('--layouts', nargs='+', default=['siblings', 'distinct-cores'], choices=tin.CPU_LAYOUTS,
                   help='readings of "8 vCPUs" to measure; a reading with the same CPUs as an earlier one is skipped')
    c.add_argument('--lscpu-file', type=Path, help='topology of another host, for --dry-run')
    c.add_argument('--scenarios', nargs='+', default=list(SCENARIOS), choices=SCENARIOS)
    c.add_argument('--score-functions', nargs='+', default=['score', 'full_score'], choices=['score', 'full_score'])
    c.add_argument('--seconds', type=int, default=600)
    c.add_argument('--warmup', type=int, default=10)
    # The build already checked 10 forms and 2 ranked; each run re-checks a smaller sample.
    c.add_argument('--validation-queries', type=int, default=3)
    c.add_argument('--ranked-validation-queries', type=int, default=1)
    c.add_argument('--setup-timeout-seconds', type=int, default=21600)
    c.add_argument('--port', type=int, default=28928)
    c.add_argument('--drop-caches', action='store_true',
                   help="drop the Docker VM's page cache as each measurement starts (local runs on a Mac)")
    c.add_argument('--published', type=Path, default=PUBLISHED)
    c.add_argument('--dry-run', action='store_true')
    c.add_argument('extra', nargs='*', help='more tin.py run arguments, after --')
    c.set_defaults(func=campaign)
    r = sub.add_parser('report', help='set a campaign beside the published numbers')
    r.add_argument('campaign', type=Path)
    r.add_argument('--published', type=Path, default=PUBLISHED)
    r.set_defaults(func=report)
    return p


def main():
    args = parser().parse_args()
    args.func(args)


if __name__ == '__main__':
    main()
