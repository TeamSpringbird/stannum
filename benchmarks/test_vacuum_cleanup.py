"""Workload scheduling and oracle contracts, without a PostgreSQL server."""
from pathlib import Path
import tempfile
import unittest
import argparse
from types import SimpleNamespace

from vacuum_cleanup import phase_lines, load_metrics, term_expression, membership_query, ranked_script, validate_workload, percentage


class TrafficTests(unittest.TestCase):
    def fixture(self, lines):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        path = Path(directory.name) / 'log'
        path.write_text(lines)
        return [path]

    def test_phase_membership_uses_execution_start_not_scheduled_start(self):
        # Scheduled at 9.95, executes 10.10–10.15. Entire execution overlaps.
        paths = self.fixture('0 1 200000 0 10 150000 150000\n'
                             '0 2 300000 0 10 200000 0\n'
                             '0 3 100000 0 11 50000 0\n')
        self.assertEqual(len(phase_lines(paths, 10, 11)), 1)
        self.assertEqual(load_metrics(paths, 100, 1)['completed'], 3)

    def test_fixed_rate_reports_lag_failures_and_shutdown_accounting(self):
        paths = self.fixture('0 1 200000 0 10 150000 150000\n'
                             '0 2 1000 0 10 200000 0\n'
                             '0 3 skipped 0 10 500000\n'
                             '0 4 failed 0 10 600000\n')
        result = load_metrics(paths, 100, 1)
        self.assertEqual(result['nominal_offered'], 100)
        self.assertEqual(result['scheduled_logged'], 4)
        self.assertEqual(result['completed'], 2)
        self.assertEqual(result['skipped'], 1)
        self.assertEqual(result['failed'], 1)
        self.assertEqual(result['late_over_1ms'], 1)
        self.assertEqual(result['schedule_lag_max_ms'], 150)
        with self.assertRaises(ValueError):
            phase_lines(paths, 0, 20)

    def test_closed_loop_has_no_claimed_offered_rate(self):
        result = load_metrics(self.fixture('0 1 1000 0 10 0\n'), None, 4)
        self.assertIsNone(result['nominal_offered'])
        self.assertEqual(result['schedule_lag_max_ms'], 0)

    def test_ranked_oracle_forces_exhaustive_same_snapshot_reference(self):
        script = ranked_script()
        self.assertIn('BEGIN ISOLATION LEVEL REPEATABLE READ', script)
        self.assertLess(script.index('enable_custom_scan=off'), script.index('all_matches AS MATERIALIZED'))
        self.assertLess(script.index('enable_custom_scan=on'), script.index('top AS MATERIALIZED'))
        self.assertIn('ORDER BY score DESC, id LIMIT 10', script)

    def test_membership_oracles_follow_distribution_without_text_search(self):
        for distribution in ('uniform', 'hot'):
            term = term_expression(97, distribution, 'id')
            query, predicate = membership_query('selective', term)
            self.assertEqual(query, 'common AND w7')
            self.assertIn('id', predicate)
            self.assertNotIn('body', predicate)
        self.assertEqual(membership_query('phrase', term), ('"common filler"', 'true'))


class ConfigurationTests(unittest.TestCase):
    def test_rewrite_rejects_density_that_does_not_trigger_rewrite(self):
        with self.assertRaisesRegex(ValueError, 'at least 50%'):
            validate_workload(SimpleNamespace(scenario='rewrite', delete_percent=49, vocabulary=97))
        validate_workload(SimpleNamespace(scenario='rewrite', delete_percent=50, vocabulary=97))
        validate_workload(SimpleNamespace(scenario='mixed', delete_percent=10, vocabulary=97))

    def test_all_dead_timing_is_explicitly_out_of_scope(self):
        with self.assertRaises(argparse.ArgumentTypeError):
            percentage('100')
        self.assertEqual(percentage('99'), 99)
