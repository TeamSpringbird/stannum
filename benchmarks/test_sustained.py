# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

from pathlib import Path
import json
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import mutation
import run
import sustained


class SustainedTests(unittest.TestCase):
    def test_only_asserted_single_row_completions_are_counted(self):
        summary = dict(failures={}, queries={'insert': {'completed': 5}, 'delete': {'completed': 4}})
        self.assertEqual(mutation.affected_rows(summary)['delete'], dict(completed=4, noops=0, affected=4))
        with self.assertRaises(ValueError):
            mutation.affected_rows(dict(summary, failures={'delete:failed': 1}))

    def test_writers_assert_one_effect_without_shell_calls(self):
        for kind, original in mutation.writer_scripts(10000, run.CASES).items():
            script = mutation.accounted_writer(original, kind)
            self.assertIn('SELECT 1 / (count(*) = 1)::int FROM changed;', script)
            self.assertNotIn('\\shell', script)
            self.assertNotIn('BEGIN;', script)

    def test_scheduling_pressure_is_not_confused_with_execution_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'log'
            # Completion order is opposite arrival order. First scheduled at
            # 100.0, second at 100.5, but the first waits substantially longer.
            path.write_text('0 1 2000000 0 102 0 1900000\n1 1 100000 0 100 600000 0\n'
                            '1 2 failed 0 103 0 0\n')
            result = mutation.traffic_load([path], 10, 2)
        self.assertEqual(result['nominal_arrivals'], 20)
        self.assertEqual(result['logged'], 3)
        self.assertEqual(result['completed'], 2)
        self.assertEqual(result['failures'], {'failed': 1})
        self.assertEqual(result['first_decile']['scheduled']['p95_ms'], 2000)
        self.assertEqual(result['first_decile']['execution']['p95_ms'], 100)
        self.assertEqual(result['last_decile']['lag_p95_ms'], 0)
        self.assertIsNone(result['overall']['scheduled']['p99_ms'])

    def test_invalid_lag_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'log'
            path.write_text('0 1 2000 0 102 0 3000\n')
            with self.assertRaises(ValueError):
                mutation.traffic_load([path], 10, 2)

    def test_slow_maintenance_records_missed_slots_without_a_catchup_storm(self):
        self.assertEqual(mutation.next_tick(10, 11, 5), (15, 0))
        self.assertEqual(mutation.next_tick(10, 26, 5), (30, 3))
        self.assertEqual(mutation.next_tick(10, 15, 5), (20, 1))

    def test_repeated_campaign_reverses_rate_and_concurrency_order(self):
        cases = sustained.schedule([100, 1000], [1, 2], 2)
        self.assertEqual(len(cases), 8)
        self.assertEqual([x[1:] for x in cases[:4]], list(reversed([x[1:] for x in cases[4:]])))

    def test_failed_start_and_failed_shutdown_preserve_cluster_evidence(self):
        for fail_stop in (False, True):
            with self.subTest(fail_stop=fail_stop), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                library = root / 'stannum.so'
                library.write_bytes(b'retained release')
                (root / 'postgres').write_bytes(b'postgres executable')
                cluster = root / 'cluster'
                cluster.mkdir()
                output = root / 'results'
                stopped = []

                def execute(argv, **kwargs):
                    if argv[0] == 'initdb':
                        (cluster / 'data').mkdir()
                    if argv[0] == 'pg_ctl' and argv[-1] == 'start':
                        (cluster / 'data/postmaster.pid').write_text('123')
                        raise subprocess.TimeoutExpired(argv, 240)
                    if argv[0] == 'pg_ctl' and argv[-1] == 'stop':
                        stopped.append(True)
                        if fail_stop:
                            raise subprocess.CalledProcessError(1, argv)
                        (cluster / 'data/postmaster.pid').unlink()
                    return subprocess.CompletedProcess(argv, 0)

                argv = ['sustained', '--artifact', str(library), '--output', str(output)]
                with patch('sys.argv', argv), patch.object(sustained.bench, 'command', return_value=str(root)), \
                     patch.object(sustained.tempfile, 'mkdtemp', return_value=str(cluster)), \
                     patch.object(sustained.subprocess, 'run', side_effect=execute):
                    with self.assertRaises((subprocess.TimeoutExpired, subprocess.CalledProcessError)):
                        sustained.main()
                self.assertEqual(stopped, [True])
                self.assertTrue(cluster.exists())
                result = json.loads((output / 'campaign.json').read_text())
                self.assertEqual(result['status'], 'failed')
                self.assertEqual('cleanup_error' in result, fail_stop)


if __name__ == '__main__':
    unittest.main()


class GinBaselineTests(unittest.TestCase):
    def test_count_mutation_does_not_request_ranking(self):
        for engine in ('gin', 'stannum'):
            queries = run.workload(engine, 'mutation-count')
            self.assertEqual(queries, run.workload(engine, 'count'))
            self.assertTrue(all(name.endswith('_count') for name, _ in queries))
        case = run.CASES[4]
        sql = mutation.check_sql(case, run.predicate('gin', case))
        self.assertIn('EXCEPT SELECT id FROM expected', sql)
        self.assertNotIn('score', sql)
        checked = dict(count=4, expected=4, differences=0, delta_sample=[], ranked=False)
        self.assertEqual(mutation.evaluate_check('phrase', checked)['count'], 4)
        with self.assertRaises(ValueError):
            mutation.evaluate_check('phrase', dict(checked, differences=1))

    def test_gin_variant_and_pending_measurement_are_explicit(self):
        self.assertIn('fastupdate=off', run.index_sql('gin', 'off'))
        self.assertIn('fastupdate=on', run.index_sql('gin', 'on'))
        self.assertIn('pgstatginindex', mutation.sample_sql('gin', False))
        self.assertNotIn('stannum.segment_info', mutation.sample_sql('gin', False))

    def test_wal_accounting_crosses_high_word_boundary(self):
        self.assertEqual(sustained.wal_delta('1/FFFFFFF0', '2/10'), 32)
        with self.assertRaises(ValueError):
            sustained.wal_delta('2/10', '1/FFFFFFF0')

    def test_cross_engine_comparison_keeps_workload_and_server_guards(self):
        a = dict(status='complete', settings={'shared_buffers': '65536', 'stannum.max_segments': '16'},
                 config={'profile': 'mutation-count', 'read_rate': 100,
                         'mutation': {'gin_fastupdate': None, 'settings': ['enable_seqscan=off', 'stannum.max_segments=16']}})
        b = json.loads(json.dumps(a))
        b['settings'].pop('stannum.max_segments')
        b['config']['mutation'] = {'gin_fastupdate': 'on', 'settings': ['enable_seqscan=off']}
        self.assertEqual(run.comparison_mismatches(a, b, True), [])
        b['config']['read_rate'] = 101
        self.assertIn('config', run.comparison_mismatches(a, b, True))
        b['settings']['shared_buffers'] = '32768'
        self.assertIn('PostgreSQL settings', run.comparison_mismatches(a, b, True))
        b['config']['profile'] = 'mutation'
        self.assertIn('ranking contract', run.comparison_mismatches(a, b, True))
