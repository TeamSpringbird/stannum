# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest

import resources


class ResourceSummaryTests(unittest.TestCase):
    def record(self, started, value, phase='driver-warmup-and-measurement'):
        return dict(started=started, finished=started + .1, phase=phase, counters={
            'cpu.stat': f'usage_usec {value}', 'memory.events': 'oom 0',
            'memory.stat': f'pgmajfault {value}\nanon 100',
            'memory.current': str(value), 'memory.peak': '1000',
            'io.stat': f'1:0 rbytes={value} wbytes=0\n2:0 rbytes={value} wbytes=0',
            'memory.pressure': None, 'io.pressure': None})

    def summarize(self, records, start=10, end=20):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'resources.jsonl'
            path.write_text(''.join(json.dumps(r) + '\n' for r in records))
            return resources.summarize(path, start, end)

    def test_excludes_warmup_and_straddling_boundary_and_reports_gaps(self):
        result = self.summarize([self.record(0, 500, 'index-build'), self.record(9.95, 600),
                                 self.record(11, 700), self.record(19, 800), self.record(19.95, 900)])
        self.assertEqual(result['status'], 'sampled')
        self.assertEqual(result['cpu_delta'], {'usage_usec': 100})
        self.assertEqual(result['io_delta']['rbytes'], 200)
        self.assertEqual(result['memory_counters_delta'], {'pgmajfault': 100})
        self.assertEqual(result['covered_seconds'], 8)
        self.assertEqual(result['start_gap_seconds'], 1)
        self.assertAlmostEqual(result['end_gap_seconds'], .9)
        self.assertEqual(result['build_sampled_peak_bytes'], 500)
        self.assertEqual(result['sampled_peak_bytes'], 800)
        self.assertEqual(result['lifetime_peak_bytes'], 1000)
        self.assertFalse(result['pressure_available'])

    def test_missing_samples_are_not_zero_resource_use(self):
        result = self.summarize([{'started': 12, 'error': 'stopped', 'phase': 'driver'}])
        self.assertEqual(result['status'], 'insufficient_samples')
        self.assertEqual(result['sampling_errors'], 1)
        self.assertNotIn('io_delta', result)
        self.assertIsNone(resources.delta(None, {'rbytes': 100}))

    def test_counter_reset_clock_jump_and_shape_change_are_rejected(self):
        for records in ([self.record(11, 100), self.record(19, 50)],
                        [self.record(19, 100), self.record(11, 200)]):
            with self.assertRaises(ValueError):
                self.summarize(records)
        with self.assertRaises(ValueError):
            resources.delta({'oom': 0}, {'oom_kill': 0})
