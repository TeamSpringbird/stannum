# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local import report


def sample(usec, rbytes, finished, phase=report.MEASUREMENT):
    return dict(phase=phase, finished=finished,
                counters={'cpu.stat': f'usage_usec {usec}\nuser_usec 0',
                          'io.stat': f'254:0 rbytes={rbytes} wbytes=0\n254:16 rbytes=0 wbytes=7'})


class LocalReportTest(unittest.TestCase):
    def test_window_keeps_the_longest_stretch_between_counter_resets(self):
        rows = [sample(5, 0, 0, phase='validation'), sample(10, 0, 1), sample(20, 0, 2),
                sample(1, 0, 3), sample(2, 0, 4), sample(3, 0, 5), {'phase': report.MEASUREMENT}]
        window = report.measurement_window(rows)
        self.assertEqual([r['finished'] for r in window], [3, 4, 5])

    def test_read_bytes_sums_every_device(self):
        row = sample(0, 1024, 0)
        row['counters']['io.stat'] += '\n259:0 rbytes=2048 wbytes=0'
        self.assertEqual(report.read_bytes(row), 3072)

    def test_workload_line_reports_the_update_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = Path(tmp)
            (run / 'stannum').mkdir()
            (run / 'manifest.json').write_text(json.dumps({'jobs': [{'status': 'complete'}]}))
            (run / 'comparison.json').write_text(json.dumps([dict(
                completed=600, p50_ms=10, p95_ms=20, p99_ms=30, families={'phrase': {'p50_ms': 12}})]))
            (run / 'stannum' / 'resources.jsonl').write_text(
                '\n'.join(json.dumps(r) for r in (sample(0, 0, 0), sample(2_000_000, 2**30, 2))) + '\n')
            line = report.workload_line(run, 'v42', 'mixed', 25, 60)
        self.assertTrue(line.startswith('v42 mixed u25: complete 10.0 QPS'), line)
        self.assertIn('disk read 1.0 GB (1.7 MB/query) cpu 1.00 cores', line)
        self.assertIn('phrase p50 12', line)

    def test_workload_line_survives_a_run_that_never_measured(self):
        with tempfile.TemporaryDirectory() as tmp:
            line = report.workload_line(Path(tmp), 'v1', 'mixed', 0, 60)
        self.assertTrue(line.startswith('v1 mixed u0: missing (no comparison'), line)


if __name__ == '__main__':
    unittest.main()
