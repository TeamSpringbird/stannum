# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Workload scheduling and oracle contracts, without a PostgreSQL server."""
from pathlib import Path
import tempfile
import unittest
import argparse
from types import SimpleNamespace

from vacuum_cleanup import phase_lines, load_metrics, term_expression, membership_query, ranked_script, validate_workload, percentage, require_selective_matches, verify_strategy, ranked_accounting


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
        self.assertLess(script.index('enable_custom_scan=on'), script.rindex('top AS MATERIALIZED'))
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


class FixtureTests(unittest.TestCase):
    def test_empty_selective_fixture_is_rejected_before_reader_traffic(self):
        # Hot distribution retains wN only on multiples of five; vocab100
        # therefore has no w7 documents, even before applying deletion.
        expected = sum(n % 5 == 0 and n % 100 == 7 for n in range(1, 32769))
        with self.assertRaisesRegex(ValueError, 'no live w7 matches'):
            require_selective_matches(expected)
        require_selective_matches(1)


class StrategySettingTests(unittest.TestCase):
    def test_registration_is_checked_in_backend_that_loads_extension(self):
        def fresh_backend(statement):
            # Registration in a previous sql() backend is not inherited.
            return 'Direct' if statement.startswith("LOAD 'stannum';") else ''
        verify_strategy(fresh_backend, 'direct')

    def test_absent_registered_setting_and_wrong_setting_are_rejected(self):
        for observed in ('', 'auto'):
            with self.assertRaisesRegex(ValueError, 'not registered'):
                verify_strategy(lambda statement: observed, 'reconstruct')


class RankedAccountingTests(unittest.TestCase):
    def test_invalidated_comparisons_are_not_counted_as_correct(self):
        result = ranked_accounting('STANNUM_RANKED_STABLE\nSTANNUM_RANKED_INVALIDATED\n', 2)
        self.assertEqual(result, dict(stable_checked=1, invalidated=1, completed=2))

    def test_missing_markers_and_no_stable_coverage_fail(self):
        with self.assertRaisesRegex(ValueError, 'accounting mismatch'):
            ranked_accounting('STANNUM_RANKED_STABLE\n', 2)
        with self.assertRaisesRegex(ValueError, 'no stable'):
            ranked_accounting('STANNUM_RANKED_INVALIDATED\n', 1)

    def test_fingerprint_statements_bracket_candidate(self):
        script = ranked_script()
        self.assertLess(script.index('AS scoring_state'), script.index('all_matches AS MATERIALIZED'))
        self.assertLess(script.index('AS correct'), script.index('AS stable'))
        self.assertIn('AS unique_ids\n\\gset\nSELECT', script)
