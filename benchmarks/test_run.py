import copy
import json
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

import mutation
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




class EngineIdentityTests(unittest.TestCase):
    def test_stannum_and_tin_use_distinct_names_with_the_same_query_shapes(self):
        for engine in ('stannum', 'tin'):
            queries = run.workload(engine, 'mixed')
            self.assertIn(f'USING {engine}(body)', run.index_sql(engine))
            ranked = [sql for name, sql in queries if name.endswith('_ranked')]
            self.assertTrue(all(f'{engine}.full_score(ctid)' in sql for sql in ranked))
        stannum = run.workload('stannum', 'mixed')
        tin = [(name, sql.replace('tin.full_score', 'stannum.full_score'))
               for name, sql in run.workload('tin', 'mixed')]
        self.assertEqual(stannum, tin)


class ProvenanceTests(unittest.TestCase):
    def test_segment_changes_change_build_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'segment').mkdir()
            source = root / 'segment/lib.rs'
            source.write_text('first version')
            out = root / 'metadata'
            out.mkdir()
            def git(command, **kwargs):
                if command[1] == 'ls-files':
                    return 'segment/lib.rs'
                if command[1] == 'rev-parse':
                    return 'test-commit'
                return ''
            with patch.object(run, 'ROOT', root), patch.object(run, 'command', side_effect=git):
                first = run.provenance(out)
                source.write_text('second version')
                second = run.provenance(out)
            self.assertIn('segment/lib.rs', first['source_files'])
            self.assertNotEqual(first['source_sha256'], second['source_sha256'])


def plan(actual_node, expected_node):
    return [{"Plan": {"Node Type": "Result", "Plans": [
        {"Node Type": actual_node, "Subplan Name": "CTE actual"},
        {"Node Type": expected_node, "Subplan Name": "CTE expected"},
        {"Node Type": "Limit", "Subplan Name": "CTE top", "Plans": [{"Node Type": actual_node}]},
        {"Node Type": "Aggregate", "Subplan Name": "InitPlan 4", "Plans": [{"Node Type": "CTE Scan"}]}]}}]


class MutationProfileTests(unittest.TestCase):
    def test_existing_profiles_keep_their_workload_and_fixture_hashes(self):
        # sql_sha256 / fixture_sha256 values recorded by manifests before the mutation profile existed.
        self.assertEqual(run.PROFILES[:3], ("count", "ranked", "mixed"))
        self.assertEqual(run.workload("stannum", "mutation"), run.workload("stannum", "mixed"))
        pinned = {("stannum", "count"): "3c7f8c4cd10392f93ca10773c8dab6d42871cb895b185284aecfe23156fb4301",
                  ("stannum", "ranked"): "a4e67fc20f63995ba7c685ee749b965d3bc239859317a9d7432d4ebab8a6ad0e",
                  ("stannum", "mixed"): "ea79ff283fd2e49d35fa1b03c5b3b3ed5f7add57110359307b351254761bb08f",
                  ("pg_textsearch", "mixed"): "7f326eba910828a3c4100e584b309d93089c1a2831b3fec8bf67a851684f5e14",
                  ("gin", "count"): "b6caaf0e6bb0b95646ccb29c63bfa41486e720c951cf9297dff82f57b972bfec"}
        for (engine, profile), expected in pinned.items():
            self.assertEqual(run.digest(run.canonical(run.workload(engine, profile))), expected, (engine, profile))
        self.assertTrue(run.digest(run.fixture_sql(10000).encode()).startswith("78858462c7b645cf"))

    def test_writer_scripts_cover_every_kind_and_only_drift_toward_real_terms(self):
        scripts = mutation.writer_scripts(10000, run.CASES, 12000)
        self.assertEqual(set(scripts), set(mutation.KINDS))
        self.assertIn("nextval('benchmark_ids')", scripts["insert"])
        self.assertIn("FROM benchmark_pool WHERE id = :src", scripts["insert"])
        self.assertIn("random(1, 12000)", scripts["delete"])
        self.assertIn("WHERE id = (SELECT id FROM documents WHERE id >= :id ORDER BY id LIMIT 1)", scripts["delete"])
        bundles = mutation.drift_bundles(run.CASES)
        self.assertEqual(bundles, ["", "rare", "common rare", "common rare", "alpha beta"])
        self.assertIn(f"random(0, {len(bundles) - 1})", scripts["update"])
        self.assertIn("' mutablea'", scripts["update"])
        self.assertNotIn("absenttoken", scripts["update"])
        self.assertIn("' alpha beta'", scripts["update"])

    def test_oracle_predicates_use_word_boundaries_without_the_index(self):
        by_name = {case[0]: case for case in run.CASES}
        self.assertEqual(mutation.regex_predicate(by_name["rare"]), "body ~ '\\mrare\\M'")
        self.assertEqual(mutation.regex_predicate(by_name["and"]), "body ~ '\\mcommon\\M' AND body ~ '\\mrare\\M'")
        self.assertEqual(mutation.regex_predicate(by_name["or"]), "body ~ '\\mcommon\\M' OR body ~ '\\mrare\\M'")
        self.assertEqual(mutation.regex_predicate(by_name["phrase"]), "body ~ '\\malpha beta\\M'")
        sql = mutation.check_sql(by_name["rare"], "body ==> 'rare'", "stannum.full_score(ctid)", "score DESC")
        self.assertIn("MATERIALIZED (SELECT id FROM documents WHERE body ==> 'rare')", sql)
        self.assertIn("MATERIALIZED (SELECT id FROM documents WHERE body ~ '\\mrare\\M')", sql)
        self.assertIn("ORDER BY score DESC LIMIT 10", sql)

    def test_check_evaluation_fails_on_any_difference_or_malformed_top_ten(self):
        good = {"count": 3, "expected": 3, "differences": 0, "delta_sample": [],
                "top": [[7, 2.5], [3, 2.5], [9, 1.0]], "top_outside": 0}
        self.assertEqual(mutation.evaluate_check("rare", good), {"name": "rare", "count": 3, "top": [7, 3, 9]})
        for field, value in (("differences", 1), ("expected", 4), ("top_outside", 1),
                             ("top", [[7, 1.0], [3, 2.5], [9, 1.0]]), ("top", [[7, 2.5], [7, 2.5], [9, 1.0]]),
                             ("top", [[7, 2.5], [3, 2.5]]), ("top", [[7, float("nan")], [3, 2.5], [9, 1.0]])):
            bad = dict(good, **{field: value})
            with self.assertRaises(ValueError):
                mutation.evaluate_check("rare", bad)

    def test_check_round_parses_one_result_per_query_inside_one_snapshot(self):
        seen = []
        result = json.dumps({"count": 0, "expected": 0, "differences": 0, "delta_sample": [], "top": [], "top_outside": 0})
        def psql(sql, env):
            seen.append(sql)
            return "\n".join([result, result])
        records = mutation.check_round(psql, {}, [("miss", "SELECT 1;"), ("rare", "SELECT 2;")])
        self.assertEqual(set(records), {"miss", "rare"})
        self.assertTrue(seen[0].startswith("BEGIN ISOLATION LEVEL REPEATABLE READ;"))
        self.assertIn("SET LOCAL enable_seqscan = off;", seen[0])
        self.assertTrue(seen[0].rstrip().endswith("COMMIT;"))
        with self.assertRaises(ValueError):
            mutation.check_round(lambda sql, env: result, {}, [("miss", "SELECT 1;"), ("rare", "SELECT 2;")])

    def test_oracle_plan_must_separate_index_and_sequential_sides(self):
        self.assertTrue(mutation.oracle_plan_is_independent(plan("Custom Scan", "Seq Scan")))
        self.assertTrue(mutation.oracle_plan_is_independent(plan("Bitmap Heap Scan", "Seq Scan")))
        self.assertFalse(mutation.oracle_plan_is_independent(plan("Seq Scan", "Seq Scan")))
        self.assertFalse(mutation.oracle_plan_is_independent(plan("Custom Scan", "Custom Scan")))

    def test_buckets_separate_execution_time_from_schedule_lag_and_group_shapes(self):
        with tempfile.TemporaryDirectory() as tmp:
            reader = Path(tmp) / "reader-log.1"
            reader.write_text("0 1 2000 0 100 500000\n0 2 4000 1 100 600000\n0 3 8000 0 111 0\n0 4 failed 1 111 0\n")
            writer = Path(tmp) / "writer-log.2"
            writer.write_text("0 1 5883 0 100 700000 5538\n0 2 12000 2 105 0 2000\n0 3 3000 1 112 0 0\n")
            buckets = mutation.bucket_logs([reader], ["rare_count", "rare_ranked"], 10, 100)
            self.assertEqual([b["start_seconds"] for b in buckets], [0, 10])
            self.assertEqual(buckets[0]["queries"]["rare_count"]["p50_ms"], 2)
            self.assertEqual(buckets[0]["shapes"]["count"]["completed"], 1)
            self.assertEqual(buckets[0]["shapes"]["ranked"]["max_ms"], 4)
            self.assertEqual(buckets[1]["queries"]["rare_ranked"]["completed"], 0)
            self.assertIsNone(buckets[1]["queries"]["rare_ranked"]["p99_ms"])
            writes = mutation.bucket_logs([writer], ["insert", "delete", "update"], 10, 100)
            self.assertAlmostEqual(writes[0]["queries"]["update"]["max_ms"], 10)
            self.assertAlmostEqual(writes[0]["queries"]["insert"]["p50_ms"], 0.345)
            self.assertEqual(writes[0]["schedule_lag_max_ms"], 5.538)
            self.assertEqual(writes[0]["shapes"]["update"]["completed"], 1)
            self.assertEqual(writes[1]["queries"]["delete"]["completed"], 1)

    def test_timeline_annotation_and_rendering_show_events_per_bucket(self):
        buckets = mutation.bucket_logs([], [], 10, 0)
        self.assertEqual(buckets, [])
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / "reader-log.1"
            log.write_text("0 1 2000 0 100 0\n0 2 3000 1 100 0\n0 3 2500 0 112 0\n")
            buckets = mutation.bucket_logs([log], ["rare_count", "rare_ranked"], 10, 100)
        sample = {"t": 11, "index_bytes": 2**20, "fsm_free_pages": 3,
                  "segments": {"immutable": 4, "max_generation": 9, "buffer_docs": 12}}
        vacuum = {"t": 12, "ms": 1500, "before": {}, "after": {}}
        check = {"t": 13, "ms": 900, "queries": {}}
        mutation.annotate(buckets, 10, [sample], [vacuum], [check])
        self.assertNotIn("sample", buckets[0])
        self.assertEqual(buckets[1]["sample"], sample)
        self.assertEqual(buckets[1]["vacuums"], [vacuum])
        text = mutation.render(buckets, ["insert"], 10)
        lines = text.splitlines()
        self.assertEqual(len(lines), 3)
        self.assertIn("insert max", lines[0])
        self.assertIn("vacuum 1.5s; check 0.9s ok", lines[2])
        self.assertIn("1.0", lines[2])
        self.assertEqual(mutation.worst_mutations(buckets, ["insert"]), {"insert": {"max_ms": None, "bucket_start_seconds": None}})

    def test_reclaim_summary_reports_pages_freed_and_dead_docs_per_vacuum(self):
        vacuums = [{"t": 60, "ms": 2000,
                    "before": {"index_bytes": 10, "fsm_free_pages": 0, "segments": {"dead_docs": 500, "immutable": 4}, "table_stats": {"n_dead_tup": 9}},
                    "after": {"index_bytes": 12, "fsm_free_pages": 7, "segments": {"dead_docs": 0, "immutable": 3}, "table_stats": {"n_dead_tup": 0}}}]
        rows = mutation.reclaim_summary(vacuums)
        self.assertEqual(rows[0]["fsm_free_pages_after"], 7)
        self.assertEqual(rows[0]["dead_docs_before"], 500)
        self.assertEqual(rows[0]["segments_after"], 3)
        self.assertEqual(rows[0]["n_dead_tup_after"], 0)

    def test_mix_parsing_rejects_unknown_kinds_and_empty_mixes(self):
        self.assertEqual(mutation.parse_mix("insert=2,delete=1"), {"insert": 2, "delete": 1, "update": 0})
        for text in ("insert=1,drop=1", "insert=0,delete=0,update=0", "insert=x"):
            with self.assertRaises(ValueError):
                mutation.parse_mix(text)

    def test_maintenance_failure_is_recorded_and_stops_the_thread(self):
        calls = []
        def sql_json(sql, env):
            calls.append(sql)
            return {"index_bytes": 1}
        def psql(sql, env):
            return "not json"
        with tempfile.TemporaryDirectory() as tmp:
            m = mutation.Maintenance(psql, sql_json, {}, {}, "stannum", [("rare", "SELECT 1;")], False, Path(tmp),
                                     sample_interval=.01, vacuum_interval=0, check_interval=.01)
            m.start(time.time())
            deadline = time.monotonic() + 5
            while (not m.failures or len(m.samples) < 2) and time.monotonic() < deadline:
                time.sleep(.01)
            m.finish()
        self.assertEqual(len(m.failures), 1)
        self.assertEqual(m.failures[0]["thread"], "check")
        self.assertGreaterEqual(len(m.samples), 2)
        self.assertEqual(m.checks, [])
        self.assertTrue(all("t" in s and "ms" in s for s in m.samples))
        self.assertIn("stannum.segment_info('search_idx')", calls[0])


if __name__ == "__main__":
    unittest.main()
