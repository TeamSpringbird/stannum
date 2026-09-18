import unittest

import server_times


def plan(node_type, ms, child=None, provider=None):
    node = {"Node Type": node_type, "Actual Total Time": ms}
    if provider:
        node["Custom Plan Provider"] = provider
    if child:
        node["Plans"] = [child]
    return [{"Plan": node, "Execution Time": ms}]


class ServerTimesTests(unittest.TestCase):
    def test_parse_plans_splits_multiline_json_from_psql(self):
        import json
        text = "\n".join(json.dumps(plan("Seq Scan", 1.0), indent=2) for _ in range(3))
        self.assertEqual(len(server_times.parse_plans(text)), 3)

    def test_top_node_looks_through_limit_and_aggregate(self):
        scan = {"Node Type": "Custom Scan", "Custom Plan Provider": "Stannum Text Search Scan"}
        nested = plan("Limit", 2.0, child={"Node Type": "Result", "Plans": [scan]})
        self.assertEqual(server_times.top_node(nested), "Stannum Text Search Scan")
        self.assertEqual(server_times.top_node(plan("Aggregate", 1.0, child={"Node Type": "Bitmap Heap Scan"})),
                         "Bitmap Heap Scan")

    def test_summarize_discards_warmup_and_takes_the_median(self):
        queries = [("rare_count", "SELECT 1;")]
        plans = [plan("Seq Scan", ms) for ms in (50.0, 9.0, 1.0, 3.0, 2.0)]
        rows = server_times.summarize(queries, plans, repetitions=5, discard=2)
        self.assertEqual(rows[0]["median_ms"], 2.0)
        self.assertEqual(rows[0]["samples"], 3)
        self.assertEqual(rows[0]["min_ms"], 1.0)

    def test_workload_shapes_come_from_the_harness(self):
        queries = server_times.workload("stannum", "mixed", server_times.load_cases(None))
        names = [name for name, _ in queries]
        self.assertIn("rare_count", names)
        self.assertIn("rare_ranked", names)
        self.assertTrue(all("stannum.full_score" in sql for name, sql in queries if name.endswith("_ranked")))


if __name__ == "__main__":
    unittest.main()
