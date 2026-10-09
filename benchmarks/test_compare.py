# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent / "compare"))
import compare  # noqa: E402


def tin_plan():
    scan = {"Node Type": "Custom Scan", "Custom Plan Provider": "Text Search Scan", "Index": "d_body_idx",
            "Query": "AND(a, b)", "Predicted Work": {"Rows": 5.0, "Posting Pages": 8.0},
            "Execution Strategy": {"Strategy": "Conjunction", "Postings": "PagePack", "Positions": "Not read"},
            "Page Touches": {"Total": 45, "Approx MiB": 0.4, "Metadata": 7, "Term Map": 12, "Postings Footer": 26},
            "Elided Terms": "a", "Shared Hit Blocks": 26, "Shared Read Blocks": 3}
    projector = {"Node Type": "Custom Scan", "Custom Plan Provider": "Projector",
                 "Predicted Work": {"Rows": 10.0}, "Plans": [scan]}
    return {"Plan": {"Node Type": "Limit", "Shared Hit Blocks": 36, "Shared Read Blocks": 3,
                     "Plans": [projector]},
            "Planning": {"Shared Hit Blocks": 94, "Shared Read Blocks": 2},
            "Planning Time": 2.5, "Execution Time": 6.4}


def stannum_plan():
    scan = {"Node Type": "Custom Scan", "Custom Plan Provider": "Stannum Count", "Index": "d_body",
            "Count Strategy": "ordinal fold", "Candidates": 95, "Chunks Loaded": 0,
            "Bytes Fetched": "ordinals 222, pages 35504", "Disk Pages By Area": "",
            "Shared Hit Blocks": 5, "Shared Read Blocks": 0}
    return {"Plan": scan, "Planning Time": 0.1, "Execution Time": 0.01}


class SummarizePlanTests(unittest.TestCase):
    def test_tin_page_touches_strategy_and_predicted_work_of_the_index_scan(self):
        summary = compare.summarize_plan(tin_plan())
        self.assertEqual(summary["execution_ms"], 6.4)
        self.assertEqual((summary["shared_hit"], summary["shared_read"]), (36, 3))
        self.assertEqual(summary["planning_shared_hit"], 94)
        self.assertEqual(summary["nodes"], ["Limit", "Custom Scan (Projector)", "Custom Scan (Text Search Scan)"])
        self.assertEqual(summary["page_touches"]["Total"], 45)
        self.assertEqual(summary["page_touches"]["Term Map"], 12)
        self.assertEqual(summary["execution_strategy"], "Conjunction/PagePack/Not read")
        self.assertEqual(summary["elided_terms"], "a")
        # Only the node with an Index contributes its prediction; the projector's is its own.
        self.assertEqual(summary["predicted_work"], {"Rows": 5.0, "Posting Pages": 8.0})

    def test_stannum_counters_and_bytes_fetched_by_area(self):
        summary = compare.summarize_plan(stannum_plan())
        self.assertEqual(summary["count_strategy"], "ordinal fold")
        self.assertEqual(summary["stannum_bytes_by_area"], {"ordinals": 222, "pages": 35504})
        self.assertNotIn("stannum_pages_by_area", summary)
        self.assertEqual(summary["custom_counters"]["Candidates"], 95)


class StatementTests(unittest.TestCase):
    def test_shapes(self):
        answer, measured = compare.statements("tin", "se_10", "or_tiebreak_top10", "a OR b's", 50)
        self.assertIn("body ==> 'a OR b''s'", measured)
        self.assertIn("ORDER BY tin.score(ctid) DESC, id LIMIT 10", measured)
        self.assertIn("float4send", answer)
        _, measured = compare.statements("stannum", "se_10", "and_filtered_top10", "a AND b", 50)
        self.assertIn("AND id <= 50 ORDER BY stannum.score(ctid) DESC LIMIT 10", measured)
        answer, measured = compare.statements("tin", "se_10", "phrase_count", '"a b"', 50)
        self.assertEqual(answer, measured)
        self.assertTrue(measured.startswith("SELECT count(*) FROM compare.se_10"))


class EqualityTests(unittest.TestCase):
    def test_equal_ties_and_differences(self):
        tin = {"answer": [[1, "40"], [2, "3f"], [3, "3e"]]}
        self.assertEqual(compare.equality("or_top10", tin, {"answer": [[1, "40"], [2, "3f"], [3, "3e"]]}), "equal")
        tied_tin = {"answer": [[1, "40"], [2, "3e"], [3, "3e"]]}
        tied_other = {"answer": [[1, "40"], [3, "3e"], [7, "3e"]]}
        self.assertEqual(compare.equality("or_top10", tied_tin, tied_other), "equal up to ties")
        self.assertEqual(compare.equality("or_top10", tin, {"answer": [[1, "41"], [2, "3f"], [3, "3e"]]}),
                         "scores differ")
        self.assertEqual(compare.equality("or_count", {"answer": 4}, {"answer": 5}), "differ")
        self.assertEqual(compare.equality("or_count", {"answer": 4}, {"error": {}}), "error")


class SelectQueriesTests(unittest.TestCase):
    def test_samples_buckets_and_single_terms(self):
        trace = {"queries": [
            {"source_id": n, "token_bucket": 1 if n == 5 else 2, "text": f"w{n} x",
             "conjunction": "w5" if n == 5 else f"w{n} AND x", "disjunction": f"w{n} OR x", "phrase": f'"w{n} x"'}
            for n in range(1, 11)]}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "queries.json"
            path.write_text(json.dumps(trace))
            args = type("Args", (), {"queries": str(path), "buckets": "2", "per_shape": 3})()
            chosen = compare.select_queries(args)
        self.assertEqual([q for _, q in chosen["conjunction"]], ["w1 AND x", "w4 AND x", "w8 AND x"])
        self.assertEqual([q for _, q in chosen["term"]], ["w5", "w1", "w4"])


if __name__ == "__main__":
    unittest.main()
