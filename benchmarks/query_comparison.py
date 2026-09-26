#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Per-query diagnostics for two completed, compatible published-trace runs.

Reports sample counts, means, p95, summed query-time shares, and changes weighted
by the baseline query mix. Summed query time is not wall time or CPU time.
Single trials do not establish statistical significance. Inputs are read-only.
"""
import argparse
import collections
import datetime
import gzip
import json
import math
from pathlib import Path
import statistics

import tin


def read(root):
    manifest = json.loads((root / 'manifest.json').read_text())
    contract = tin.comparison_contract(manifest)
    exports = list((root / 'stannum').glob('result_*.json'))
    if len(exports) != 1:
        raise ValueError('expected exactly one dashboard export')
    run = json.loads(exports[0].read_text())['runs']['stannum']
    if run['endTime'] <= run['startTime']:
        raise ValueError('invalid measurement interval')
    groups = collections.defaultdict(list)
    with gzip.open(root / 'stannum/samples.json.gz', 'rt') as records:
        for line in records:
            point = json.loads(line)
            if point['type'] != 'Point':
                continue
            if point['metric'] == 'benchmark_query_errors' and point['data']['value']:
                raise ValueError('query errors in timed samples')
            if point['metric'] != 'query_duration':
                continue
            data = point['data']
            timestamp = datetime.datetime.fromisoformat(data['time'].replace('Z', '+00:00')).timestamp() * 1000
            if not run['startTime'] <= timestamp < run['endTime'] + 1:
                raise ValueError('sample outside exported measurement interval')
            if data['tags']['backend'] != 'stannum':
                raise ValueError('unexpected sample backend')
            value = data['value']
            if not math.isfinite(value) or value <= 0:
                raise ValueError('invalid query duration')
            groups[data['tags']['query_id']].append(value)
    if not groups:
        raise ValueError('no query samples')
    return contract, groups


def compare(before, after):
    if before.keys() != after.keys() or not before:
        raise ValueError('query coverage differs or is empty')
    totals = [sum(map(sum, groups.values())) for groups in (before, after)]
    count = sum(map(len, before.values()))
    rows = []
    for query in before:
        stats = []
        for groups, total in zip((before, after), totals):
            samples = groups[query]
            if not samples or any(not math.isfinite(v) or v <= 0 for v in samples):
                raise ValueError('invalid query samples')
            stats.append(dict(samples=len(samples), mean_ms=statistics.mean(samples),
                              p95_ms=tin.bench.percentile(samples, .95),
                              summed_query_ms=sum(samples), query_time_share=sum(samples)/total))
        baseline, candidate = stats
        delta = candidate['mean_ms'] - baseline['mean_ms']
        rows.append(dict(query_id=query, baseline=baseline, candidate=candidate,
                         mean_change_percent=100*delta/baseline['mean_ms'],
                         baseline_mix_delta_ms=delta*len(before[query])/count))
    return sorted(rows, key=lambda r: r['baseline_mix_delta_ms'], reverse=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('baseline', type=Path)
    parser.add_argument('candidate', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    left, before = read(args.baseline)
    right, after = read(args.candidate)
    if left != right:
        differences = [k for k in left if left[k] != right[k]]
        raise ValueError('incompatible trials: ' + ', '.join(differences))
    rows = compare(before, after)
    args.output.mkdir(parents=True, exist_ok=False)
    result = dict(baseline=str(args.baseline.resolve()), candidate=str(args.candidate.resolve()),
                  contract=left, baseline_mix_delta_ms=sum(r['baseline_mix_delta_ms'] for r in rows), queries=rows)
    (args.output/'comparison.json').write_text(json.dumps(result, indent=2)+'\n')
    lines = ['# Per-query comparison', '', 'Positive changes are slower. Sorted by added mean latency under the baseline query mix.',
             'Summed query time includes concurrent queries; it is not elapsed time or CPU time. Single-trial diagnostics, not significance claims.', '',
             '| Query | Samples before/after | Mean ms before/after | p95 ms before/after | Mean change | Baseline time share | Weighted delta ms |',
             '| --- | ---: | ---: | ---: | ---: | ---: | ---: |']
    for r in rows:
        a,b=r['baseline'],r['candidate']
        lines.append(f"| {r['query_id']} | {a['samples']}/{b['samples']} | {a['mean_ms']:.3f}/{b['mean_ms']:.3f} | {a['p95_ms']:.3f}/{b['p95_ms']:.3f} | {r['mean_change_percent']:+.1f}% | {a['query_time_share']:.1%} | {r['baseline_mix_delta_ms']:+.4f} |")
    (args.output/'report.md').write_text('\n'.join(lines)+'\n')


if __name__ == '__main__':
    main()
