import copy
from pathlib import Path
import tempfile
import unittest

import run


class MeasurementTests(unittest.TestCase):
    def test_failed_transactions_do_not_become_fast_samples(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "log"
            path.write_text("0 1 2000 0 123 0 500\n0 2 failed 0 123 0 0\n"
                            "0 3 skipped 0 123 0 0\n1 1 4000 1 123 0 1500\n")
            result = run.summarize_logs([path], ["count", "ranked"], 2)
        self.assertEqual(result["queries"]["count"]["completed"], 1)
        self.assertEqual(result["queries"]["count"]["p50_ms"], 2)
        self.assertEqual(result["queries"]["ranked"]["completed_per_second"], .5)
        self.assertEqual(result["failures"], {"count:failed": 1, "count:skipped": 1})
        self.assertIsNone(result["queries"]["count"]["p99_ms"])
        self.assertEqual(result["schedule_lag_p95_ms"], 1.5)

    def test_comparison_rejects_workload_changes_and_failed_runs(self):
        before = {"status": "complete", "config": {"rows": 1000, "profile": "count"}}
        after = copy.deepcopy(before)
        after["config"]["rows"] = 10000
        self.assertIn("config", run.comparison_mismatches(before, after))
        after = copy.deepcopy(before)
        after["status"] = "failed"
        self.assertIn("completion status", run.comparison_mismatches(before, after))

    def test_cross_engine_rankings_are_not_silently_equated(self):
        before = {"status": "complete", "config": {"profile": "ranked"}, "settings": {}}
        self.assertIn("ranking contract", run.comparison_mismatches(before, before, True))

    def test_code_change_is_allowed_but_environment_change_is_not(self):
        before = {"status": "complete", "source": {"commit": "old"}, "environment": "host-a"}
        after = {"status": "complete", "source": {"commit": "new"}, "environment": "host-a"}
        self.assertEqual(run.comparison_mismatches(before, after), [])
        after["environment"] = "host-b"
        self.assertIn("environment", run.comparison_mismatches(before, after))


if __name__ == "__main__":
    unittest.main()
