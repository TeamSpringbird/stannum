#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""One-line summaries of the local rehearsal runs.

    report.py build RUN_DIR SECONDS
    report.py workload RUN_DIR LABEL STYLE UPDATES SECONDS

`build` summarizes a mock-build.sh output directory; `workload` summarizes a
workload.sh run: QPS, latency percentiles, disk read per query and CPU cores,
from the harness's manifest, comparison and resources.jsonl.
"""
import argparse
import json
import math
from pathlib import Path

MEASUREMENT = 'driver-warmup-and-measurement'


def load(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return None


def cpu_usec(row):
    """usage_usec, the first line of cgroup cpu.stat."""
    return int(row['counters']['cpu.stat'].split()[1])


def read_bytes(row):
    total = 0
    for line in row['counters'].get('io.stat', '').splitlines():
        for pair in line.split()[1:]:
            key, value = pair.split('=')
            if key == 'rbytes':
                total += int(value)
    return total


def measurement_window(rows):
    """Measurement-phase samples from the longest stretch of monotonic counters.

    The counters reset when the harness restarts the container; the longest
    stretch without a reset is the measured server.
    """
    samples = [r for r in rows if 'counters' in r and r.get('phase') == MEASUREMENT]
    bounds = [0] + [i for i in range(1, len(samples))
                    if cpu_usec(samples[i]) < cpu_usec(samples[i - 1])] + [len(samples)]
    lo, hi = max(zip(bounds, bounds[1:]), key=lambda b: b[1] - b[0])
    return samples[lo:hi]


def workload_line(run_dir, label, style, updates, seconds):
    run_dir = Path(run_dir)
    manifest = load(run_dir / 'manifest.json') or {}
    job = (manifest.get('jobs') or [{}])[0]
    comparison = load(run_dir / 'comparison.json')
    head = f'{label} {style} u{updates}: {job.get("status", manifest.get("status", "missing"))}'
    if not comparison:
        return f'{head} (no comparison; see {run_dir}.log) {(job.get("error") or "")[:200]}'
    c = comparison[0]
    try:
        with (run_dir / 'stannum' / 'resources.jsonl').open() as stream:
            window = measurement_window(json.loads(line) for line in stream)
    except OSError:
        window = []
    if len(window) > 1:
        gb = (read_bytes(window[-1]) - read_bytes(window[0])) / 2**30
        elapsed = window[-1]['finished'] - window[0]['finished']
        cores = (cpu_usec(window[-1]) - cpu_usec(window[0])) / 1e6 / elapsed
    else:
        gb = cores = math.nan
    per_query = gb * 1024 / c['completed'] if c['completed'] else math.nan
    families = ' '.join(f"{f} p50 {v['p50_ms']:.0f}" for f, v in c['families'].items())
    return (f"{head} {c['completed'] / seconds:.1f} QPS p50 {c['p50_ms']:.0f} p95 {c['p95_ms']:.0f} "
            f"p99 {c['p99_ms']:.0f} ms | disk read {gb:.1f} GB ({per_query:.1f} MB/query) "
            f"cpu {cores:.2f} cores | {families}")


def build_lines(run_dir, seconds):
    run_dir = Path(run_dir)
    manifest = load(run_dir / 'manifest.json') or {}
    job = (manifest.get('jobs') or [{}])[0]
    lines = [f"status {manifest.get('status', 'missing')} {job.get('status')} {(job.get('error') or '')[:200]}",
             f"import {round(job.get('import_seconds', 0))} build {round(job.get('index_build_seconds', 0))} "
             f"segments {len(job.get('segments_after_build', []))} sizes {job.get('sizes')}"]
    comparison = load(run_dir / 'comparison.json')
    if comparison:
        c = comparison[0]
        families = {f: (v['completed'], v['p50_ms']) for f, v in c['families'].items()}
        lines.append(f"baseline {c.get('style', 'mixed')}: {round(c['completed'] / seconds, 1)} QPS "
                     f"p50 {c['p50_ms']} p99 {c['p99_ms']} {families}")
    return lines


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    build = commands.add_parser('build', help='summarize a mock-build.sh output directory')
    build.add_argument('run_dir', type=Path)
    build.add_argument('seconds', type=float)
    workload = commands.add_parser('workload', help='summarize a workload.sh run')
    workload.add_argument('run_dir', type=Path)
    workload.add_argument('label')
    workload.add_argument('style')
    workload.add_argument('updates', type=int)
    workload.add_argument('seconds', type=float)
    args = parser.parse_args()
    if args.command == 'build':
        print('\n'.join(build_lines(args.run_dir, args.seconds)))
    else:
        print(workload_line(args.run_dir, args.label, args.style, args.updates, args.seconds))


if __name__ == '__main__':
    main()
