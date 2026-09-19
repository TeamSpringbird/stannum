#!/usr/bin/env python3
"""Repeat fixed-input merge-budget probes using foreground_writes.py.

This measures complete INSERT execution and WAL, not individual merge phases.
Each window owns a fresh disposable database. Hold the installation lock for
the whole campaign when running on a shared development server.
"""
import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import sys

from foreground_writes import nonnegative, positive


def schedule(budgets, rounds):
    """Rotate order so each budget occupies each position over N rounds."""
    if not budgets or len(set(budgets)) != len(budgets) or rounds < 1:
        raise ValueError('need distinct budgets and at least one round')
    return [(r + 1, budget) for r in range(rounds)
            for budget in budgets[r % len(budgets):] + budgets[:r % len(budgets)]]


def window_metrics(result):
    """Keep phase labels observational; absent merge samples stay absent."""
    if result.get('status') != 'passed':
        raise ValueError('incomplete or failed window')
    docs = result['config']['docs']
    samples = result['samples']
    if [s['id'] for s in samples] != list(range(1, docs + 1)):
        raise ValueError('missing, duplicated or reordered insert samples')
    expected = dict(differences=0, heap_docs=docs, index_docs=docs, verify_errors=0)
    if result['correctness'] != expected:
        raise ValueError('window failed correctness')
    metrics = {
        'execution_ms_total': sum(s['execution_ms'] for s in samples),
        'wal_bytes_total': sum(s['wal_bytes'] for s in samples),
        'shared_read_blocks_total': sum(s['shared_read_blocks'] for s in samples),
        'final_immutable_segments': samples[-1]['segments_after'],
        'peak_immutable_segments': max(s['segments_after'] for s in samples),
    }
    groups = {}
    for kind in ('buffered', 'fold', 'fold_and_merge'):
        selected = [s for s in samples if s['kind'] == kind]
        groups[kind] = {
            'samples': len(selected),
            'execution_ms_total': sum(s['execution_ms'] for s in selected),
            'wal_bytes_total': sum(s['wal_bytes'] for s in selected),
            'p50_ms': statistics.median(s['execution_ms'] for s in selected) if selected else None,
            'max_ms': max((s['execution_ms'] for s in selected), default=None),
        }
    metrics['groups'] = groups
    # This is a lower bound on pre-existing input documents, NOT merge bytes,
    # CPU work or complete cascade work (intermediate runs are not observed).
    metrics['retired_existing_docs_total'] = sum(s['retired_existing_docs'] for s in samples)
    metrics['merge_insert_ids'] = [s['id'] for s in samples if s['kind'] == 'fold_and_merge']
    return metrics


def summarize(windows):
    if not windows:
        raise ValueError('empty campaign')
    builds = {w['result']['artifact_sha256'] for w in windows}
    fixtures = {tuple(w['result']['config'][k] for k in ('docs', 'repeat', 'write_buffer_docs'))
                for w in windows}
    servers = {json.dumps(w['result']['server'], sort_keys=True) for w in windows}
    if len(builds) != 1 or len(fixtures) != 1 or len(servers) != 1:
        raise ValueError('campaign mixed builds, fixtures or server settings')
    seen = set()
    grouped = {}
    for window in windows:
        budget = window['result']['config']['max_merge_docs']
        key = (window['round'], budget)
        if key in seen:
            raise ValueError('duplicate budget window in a round')
        seen.add(key)
        grouped.setdefault(budget, []).append(window_metrics(window['result']))
    rounds = {r for r, _ in seen}
    if seen != {(r, b) for r in rounds for b in grouped}:
        raise ValueError('incomplete budget round')
    medians = {}
    for budget, metrics in grouped.items():
        medians[budget] = {'windows': len(metrics)}
        for key in ('execution_ms_total', 'wal_bytes_total', 'shared_read_blocks_total',
                    'final_immutable_segments', 'peak_immutable_segments', 'retired_existing_docs_total'):
            medians[budget][key] = statistics.median(m[key] for m in metrics)
    return {'artifact_sha256': next(iter(builds)), 'median_window_totals': medians,
            'windows': [{'round': w['round'], 'budget': w['result']['config']['max_merge_docs'],
                         **window_metrics(w['result'])} for w in windows]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--docs', type=positive, default=4161)
    parser.add_argument('--repeat', type=positive, default=20)
    parser.add_argument('--write-buffer-docs', type=positive, default=32)
    parser.add_argument('--budgets', nargs='+', type=nonnegative, default=[0, 256, 2048])
    parser.add_argument('--rounds', type=positive, default=3)
    parser.add_argument('--artifact', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.write_buffer_docs > 1_000_000 or args.repeat > 10_000:
        parser.error('buffer cap must be <= 1000000 and repeat <= 10000')
    try:
        order = schedule(args.budgets, args.rounds)
    except ValueError as error:
        parser.error(str(error))
    args.output.mkdir(parents=True, exist_ok=False)
    initial_hash = hashlib.sha256(args.artifact.read_bytes()).hexdigest()
    (args.output / 'schedule.json').write_text(json.dumps(order, indent=2) + '\n')
    windows = []
    for round_number, budget in order:
        destination = args.output / f'round-{round_number}-budget-{budget}'
        argv = [sys.executable, str(Path(__file__).with_name('foreground_writes.py')),
                '--docs', str(args.docs), '--repeat', str(args.repeat),
                '--write-buffer-docs', str(args.write_buffer_docs),
                '--max-merge-docs', str(budget), '--artifact', str(args.artifact),
                '--output', str(destination)]
        print(f'START round={round_number} budget={budget}', flush=True)
        # The delegated probe bounds individual subprocesses and preserves raw
        # errors/SQL/results. Do not obscure those artifacts with a second DB layer.
        subprocess.run(argv, check=True)
        result = json.loads((destination / 'results.json').read_text())
        if result['artifact_sha256'] != initial_hash:
            raise RuntimeError('installed extension changed between windows')
        windows.append({'round': round_number, 'result': result})
    summary = summarize(windows)
    if hashlib.sha256(args.artifact.read_bytes()).hexdigest() != initial_hash:
        raise RuntimeError('installed extension changed during campaign')
    summary['status'] = 'passed'
    (args.output / 'results.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(json.dumps(summary['median_window_totals'], indent=2))


if __name__ == '__main__':
    main()
