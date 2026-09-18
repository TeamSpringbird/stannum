import unittest
import json
from pathlib import Path
import tempfile

import campaign


class CampaignTests(unittest.TestCase):
    def test_trimmed_mean_preserves_all_samples_and_removes_exactly_two(self):
        result = campaign.describe([100, 1, 4, 3, 2])
        self.assertEqual(result["values"], [100, 1, 4, 3, 2])
        self.assertEqual(result["median"], 3)
        self.assertEqual(result["trimmed_mean"], 3)
        self.assertEqual(result["max"], 100)
        self.assertEqual(result["n"], 5)

    def test_small_sample_does_not_pretend_to_have_five_run_estimate(self):
        self.assertIsNone(campaign.describe([1, 2, 3, 4])["trimmed_mean"])

    def test_schedule_pairs_seeds_rotates_order_and_excludes_gin_ranking(self):
        jobs = campaign.schedule(["stannum", "gin", "paradedb", "pg_textsearch"], ["count", "mixed"], 5)
        self.assertEqual(len(jobs), 35)
        self.assertFalse(any(j["engine"] == "gin" and j["profile"] == "mixed" for j in jobs))
        for engine in ("stannum", "gin", "paradedb", "pg_textsearch"):
            self.assertEqual(sum(j["engine"] == engine and j["profile"] == "count" for j in jobs), 5)
        firsts = [next(j["engine"] for j in jobs if j["repetition"] == r and j["profile"] == "count") for r in range(1, 5)]
        self.assertEqual(len(set(firsts)), 4)

    def test_pressure_excludes_startup_counters_and_preserves_oom(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            before = {"cpu.stat": "throttled_usec 1000000", "memory.events": "max 2\noom 0\noom_kill 0", "memory.peak": "1234"}
            after = {"cpu.stat": "throttled_usec 1500000", "memory.events": "max 5\noom 1\noom_kill 1", "memory.peak": "5678"}
            (root / "cgroup-before.json").write_text(json.dumps(before))
            (root / "cgroup-after.json").write_text(json.dumps(after))
            result = campaign.pressure(root)
        self.assertEqual(result["cpu_throttled_seconds"], .5)
        self.assertEqual(result["memory_limit_events"], 3)
        self.assertEqual(result["oom_kills"], 1)
        self.assertEqual(result["container_peak_bytes"], 5678)

    def test_missing_trials_are_reported_not_trimmed_away(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "campaign.json").write_text(json.dumps({"config": {"repetitions": 5},
                "jobs": [{"directory": f"r{i}", "engine": "stannum", "profile": "count"} for i in range(5)]}))
            campaign.aggregate(root)
            result = json.loads((root / "aggregate.json").read_text())
            self.assertEqual(len(result["invalid_runs"]), 5)
            self.assertEqual(result["groups"], {})
            self.assertIn("Incomplete campaign", (root / "report.md").read_text())


if __name__ == "__main__":
    unittest.main()
