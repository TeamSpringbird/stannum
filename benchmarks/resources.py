# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Summarize sampled cgroup counters without claiming exact phase boundaries."""
import json


def fields(text):
    return None if text is None else {k: int(v) for k, v in
                                     (line.split() for line in text.splitlines())}


def io_fields(text):
    if text is None:
        return None
    totals = {}
    for line in text.splitlines():
        for pair in line.split()[1:]:
            key, value = pair.split('=')
            totals[key] = totals.get(key, 0) + int(value)
    return totals


def delta(before, after):
    if before is None or after is None:
        return None
    if before.keys() != after.keys() or any(after[k] < v for k, v in before.items()):
        raise ValueError('cgroup counters reset or changed shape within measurement')
    return {key: after[key] - value for key, value in before.items()}


def peak(records, metric):
    values = [r['counters'].get(metric) for r in records]
    return max((int(v) for v in values if v is not None), default=None)


def summarize(path, start, end):
    """Times are Unix seconds from the driver's exported measured interval."""
    if end <= start:
        raise ValueError('invalid resource measurement interval')
    if not path.exists():
        return dict(status='unavailable', reason='no resource samples')
    records = [json.loads(line) for line in path.read_text().splitlines()]
    good = [r for r in records if 'counters' in r]
    if any(r['finished'] < r['started'] for r in good) or any(
            b['started'] < a['finished'] for a, b in zip(good, good[1:])):
        raise ValueError('resource sample clock moved backwards or samples overlap')
    # Every constituent counter must have been read within the measured window.
    window = [r for r in good if start <= r['started'] and r['finished'] <= end]
    result = dict(status='insufficient_samples', measurement_samples=len(window),
                  sampling_errors=sum('error' in r for r in records),
                  build_sampled_peak_bytes=peak([r for r in good if r['phase'] == 'index-build'], 'memory.current'),
                  lifetime_peak_bytes=peak(good, 'memory.peak'))
    if len(window) < 2:
        return result
    a, b = window[0], window[-1]
    ac, bc = a['counters'], b['counters']
    keys = ('workingset_refault_file', 'pgmajfault', 'pgscan', 'pgsteal')
    def memory_fields(counters):
        parsed = fields(counters.get('memory.stat'))
        return None if parsed is None else {k: v for k, v in parsed.items() if k in keys}
    result.update(
        status='sampled', covered_seconds=b['started'] - a['started'],
        start_gap_seconds=a['started'] - start, end_gap_seconds=end - b['finished'],
        largest_sample_interval_seconds=max(y['started'] - x['started'] for x, y in zip(window, window[1:])),
        max_collection_seconds=max(r['finished'] - r['started'] for r in window),
        cpu_delta=delta(fields(ac.get('cpu.stat')), fields(bc.get('cpu.stat'))),
        memory_events_delta=delta(fields(ac.get('memory.events')), fields(bc.get('memory.events'))),
        memory_counters_delta=delta(memory_fields(ac), memory_fields(bc)),
        io_delta=delta(io_fields(ac.get('io.stat')), io_fields(bc.get('io.stat'))),
        sampled_peak_bytes=peak(window, 'memory.current'),
        pressure_available=all(r['counters'].get('memory.pressure') is not None and
                               r['counters'].get('io.pressure') is not None for r in window))
    return result
