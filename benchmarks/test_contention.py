import tempfile
from pathlib import Path
import unittest

from contention import ORACLE, assert_oracle, contained_checks, interval, summarize_samples, validate_traffic
from run import summarize_logs


class ContentionTests(unittest.TestCase):
    def test_same_snapshot_oracle_rejects_missing_or_extra_membership(self):
        good = dict(differences=0, broad_differences=0, heap_docs=512)
        assert_oracle(good, 512)
        for wrong in [dict(good, differences=1), dict(good, broad_differences=1),
                      dict(good, heap_docs=511), {}, dict(good, heap_docs='512')]:
            with self.assertRaises(ValueError):
                assert_oracle(wrong, 512)

    def test_only_checks_fully_inside_common_traffic_count(self):
        checks = [dict(query_start_monotonic=a, query_end_monotonic=b)
                  for a, b in [(0, 3), (1, 2), (2, 3), (3, 4), (3, 5), (5, 6)]]
        self.assertEqual(contained_checks(checks, 2, 4), checks[2:4])
        self.assertEqual(contained_checks(checks, 4, 2), [])
        self.assertEqual(contained_checks(checks, 2, 2), [])

    def test_oracle_preserves_multiplicity_on_both_sides(self):
        # EXCEPT would hide repeated result IDs; ALL retains their excess count.
        self.assertEqual(ORACLE.count('EXCEPT ALL'), 4)
        self.assertNotIn(' EXCEPT ', ORACLE.replace('EXCEPT ALL', ''))

    def test_generation_churn_is_interval_evidence_even_with_equal_count(self):
        before = dict(epoch=1, segments=[dict(generation=1), dict(generation=2)])
        after = dict(epoch=2, segments=[dict(generation=2), dict(generation=4)])
        result = interval(before, after)
        self.assertEqual(result['observation'], 'retirement_observed')
        self.assertEqual(result['retired_generations'], [1])
        self.assertEqual(result['new_generations'], [4])
        self.assertEqual(interval(after, after)['observation'], 'no_change_observed')
        self.assertEqual(interval(dict(epoch=0, segments=[]), before)['observation'], 'publication_observed')

    def test_wait_counts_do_not_treat_idle_clients_as_active_contention(self):
        def backend(state, wait_type, wait):
            return dict(application='stannum-contention-writer', state=state,
                        wait_type=wait_type, wait=wait)
        result = summarize_samples([dict(backends=[
            backend('active', 'LWLock', 'BufferContent'),
            backend('active', None, None), backend('idle', 'Client', 'ClientRead')])])
        self.assertEqual(result['active_backend_observations'], {'writer': 2})
        self.assertEqual(result['active_wait_observations'], {'writer:LWLock:BufferContent': 1})

    def test_failed_and_incomplete_pgbench_runs_cannot_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'writer-log.1'
            path.write_text('0 1 4000 0 1700000000 100\n1 1 8000 0 1700000000 200\n')
            summary = summarize_logs([path], ['writer'], 2)
            validate_traffic(summary, 2)
            self.assertEqual(summary['queries']['writer']['p50_ms'], 4)
            self.assertIsNone(summary['queries']['writer']['p99_ms'])
            with self.assertRaises(ValueError):
                validate_traffic(summary, 3)
            path.write_text('0 1 failed 0 1700000000 100\n')
            with self.assertRaises(ValueError):
                validate_traffic(summarize_logs([path], ['writer'], 2))
            with self.assertRaises(ValueError):
                validate_traffic(summarize_logs([], ['writer'], 2))
            path.write_text('truncated\n')
            with self.assertRaises(ValueError):
                summarize_logs([path], ['writer'], 2)


if __name__ == '__main__':
    unittest.main()
