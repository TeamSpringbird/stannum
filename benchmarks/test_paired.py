# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import paired


class PairedReportTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.jobs = []
        pressure = patch.object(paired.campaign, "pressure", return_value={
            "oom_events": 0, "oom_kills": 0, "cpu_throttled_seconds": 0,
        })
        self.pressure = pressure.start()
        self.addCleanup(pressure.stop)
        comparison = patch.object(paired.bench, "comparison_mismatches", return_value=[])
        self.comparison = comparison.start()
        self.addCleanup(comparison.stop)

    def add_pair(self, number, original_ms=10, fork_ms=5):
        for variant, latency in (("original", original_ms), ("fork", fork_ms)):
            name = f"r{number:02d}-{variant}"
            self.jobs.append({"pair": number, "variant": variant,
                              "directory": name, "status": "complete"})
            path = self.root / name
            path.mkdir()
            files = {
                "manifest.json": {"query_names": ["rare"], "index_build_seconds": 2},
                "summary.json": {
                    "reader": {"queries": {"rare": {
                        "p50_ms": latency, "p95_ms": latency * 2,
                        "completed_per_second": 1000 / latency,
                    }}},
                    "writer": {"queries": {"update": {
                        "completed_per_second": 20, "p95_ms": 1,
                    }}},
                },
                "after.json": {"index_bytes": 8192},
            }
            for filename, contents in files.items():
                (path / filename).write_text(json.dumps(contents))

    def result(self):
        return json.loads((self.root / "aggregate.json").read_text())

    def assert_withheld(self, complete_pairs):
        result = self.result()
        self.assertFalse(result["complete"])
        self.assertEqual(result["complete_pairs"], complete_pairs)
        self.assertEqual(result["queries"], {})
        self.assertEqual(result["variants"], {})
        self.assertEqual(result["jobs"], self.jobs)
        self.assertIn("aggregate speedups are withheld", (self.root / "report.md").read_text())

    def test_complete_campaign_reports_paired_ratios_and_retains_all_samples(self):
        for number, latencies in enumerate(zip([10, 20, 30, 40, 50], [5, 5, 10, 10, 10]), 1):
            self.add_pair(number, *latencies)
        self.assertTrue(paired.report(self.root, self.jobs))
        result = self.result()
        self.assertTrue(result["complete"])
        self.assertEqual(result["complete_pairs"], 5)
        self.assertEqual(result["invalid_pairs"], [])
        speedup = result["queries"]["rare"]["paired_p50_speedup"]
        self.assertEqual(speedup["values"], [2, 4, 3, 4, 5])
        self.assertEqual(speedup["median"], 4)
        self.assertAlmostEqual(speedup["trimmed_mean"], 11 / 3)
        self.assertEqual(len(result["resource_pressure"]), 5)
        self.assertEqual(self.comparison.call_count, 5)
        self.assertIn("4.00x", (self.root / "report.md").read_text())

    def test_failed_pending_and_missing_partner_withhold_survivor_statistics(self):
        self.add_pair(1)
        for status in ("failed", "pending", "running", "missing"):
            with self.subTest(status=status):
                self.jobs = self.jobs[:2] + [
                    {"pair": 2, "variant": "original", "directory": "unavailable-a", "status": "complete"},
                ]
                if status != "missing":
                    self.jobs.append({"pair": 2, "variant": "fork",
                                      "directory": "unavailable-b", "status": status})
                self.assertFalse(paired.report(self.root, self.jobs))
                self.assert_withheld(1)

    def test_incomparable_pair_is_recorded_without_aborting_other_pairs(self):
        self.add_pair(1)
        self.add_pair(2)
        self.comparison.side_effect = [["settings"], []]
        self.assertFalse(paired.report(self.root, self.jobs))
        self.assert_withheld(1)
        invalid = self.result()["invalid_pairs"]
        self.assertEqual(len(invalid), 1)
        self.assertEqual(invalid[0]["pair"], 1)
        self.assertIn("settings", invalid[0]["reason"])
        self.assertEqual(self.comparison.call_count, 2)

    def test_oom_events_or_kills_in_either_variant_invalidate_pair(self):
        self.add_pair(1)
        self.add_pair(2)
        for variant in ("original", "fork"):
            for counter in ("oom_events", "oom_kills"):
                with self.subTest(variant=variant, counter=counter):
                    def pressure(path):
                        result = {"oom_events": 0, "oom_kills": 0}
                        if path.name == f"r01-{variant}":
                            result[counter] = 1
                        return result
                    self.pressure.side_effect = pressure
                    self.assertFalse(paired.report(self.root, self.jobs))
                    self.assert_withheld(1)
                    self.assertEqual(self.result()["invalid_pairs"], [
                        {"pair": 1, "reason": "OOM during timed traffic"},
                    ])


if __name__ == "__main__":
    unittest.main()
