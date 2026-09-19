# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import argparse
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import tin


class TraceCorrectnessTests(unittest.TestCase):
    def test_resource_snapshot_rejects_missing_counters_and_failed_exec(self):
        from subprocess import CompletedProcess
        raw = ''.join('\n@@' + metric + '\n42\n' for metric in tin.CGROUP_METRICS)
        with patch('tin.subprocess.run', return_value=CompletedProcess([], 0, raw, '')):
            result = tin.resource_snapshot('owned-container')
            self.assertEqual(set(result['counters']), set(tin.CGROUP_METRICS))
            self.assertGreaterEqual(result['finished'], result['started'])
        optional = raw.replace('@@memory.pressure\n42', '@@memory.pressure\nunavailable')
        with patch('tin.subprocess.run', return_value=CompletedProcess([], 0, optional, '')):
            self.assertIsNone(tin.resource_snapshot('owned-container')['counters']['memory.pressure'])
        with patch('tin.subprocess.run', return_value=CompletedProcess([], 0, '\n@@cpu.stat\n42', '')):
            with self.assertRaises(ValueError):
                tin.resource_snapshot('owned-container')
        with patch('tin.subprocess.run', return_value=CompletedProcess([], 1, raw, 'stopped')):
            with self.assertRaises(RuntimeError):
                tin.resource_snapshot('owned-container')

    def test_waiting_runner_rejects_source_edits_and_missing_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / 'runner.py'
            source.write_text('original')
            loaded = {str(source): tin.dataset.sha256(source)}
            tin.verify_sources(loaded)
            source.write_text('edited while waiting for the timing lock')
            with self.assertRaises(ValueError):
                tin.verify_sources(loaded)
            source.unlink()
            with self.assertRaises(ValueError):
                tin.verify_sources(loaded)

    def test_oracle_fails_closed_on_mismatch_missing_duplicate_or_reordered_output(self):
        names = ['1:phrase', '2:phrase']
        tin.validate_result('1:phrase|0\n2:phrase|0\n', names)
        for text in ['1:phrase|0\n2:phrase|1', '1:phrase|0',
                     '1:phrase|0\n1:phrase|0', '2:phrase|0\n1:phrase|0', '',
                     '1:phrase|0\n2:phrase|0\n3:phrase|0']:
            with self.subTest(text=text), self.assertRaises(ValueError):
                tin.validate_result(text, names)

    def test_trace_preserves_repeated_text_and_all_three_forms(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / 'datasets/wikipedia/queries.json'
            path.parent.mkdir(parents=True)
            record = dict(text='same', engines={engine: {style: 'same' for style in
                          ('conjunction', 'disjunction', 'phrase')} for engine in ('tin', 'postgres')})
            path.write_text(json.dumps({'queries': [dict(record, source_id=1), dict(record, source_id=2)]}))
            queries = tin.trace_queries(root)
        self.assertEqual(len(queries), 6)
        self.assertEqual([q[0] for q in queries], [f'{i}:{s}' for i in (1, 2)
                         for s in ('conjunction', 'disjunction', 'phrase')])

    def test_plan_diagnostics_match_parameterized_ranked_projection_and_restore_mode(self):
        query = ('1:disjunction', "'quoted' OR text", 'quoted | text', 'quoted text')
        for engine in ('stannum', 'postgres'):
            for mode in ('force_custom_plan', 'force_generic_plan'):
                with self.subTest(engine=engine, mode=mode):
                    statement = tin.prepared_plan_sql(query, engine, 'topk', mode)
                    self.assertIn('SET plan_cache_mode=' + mode, statement)
                    self.assertIn('PREPARE stannum_bench_plan AS SELECT id, body,', statement)
                    self.assertIn('ORDER BY score DESC LIMIT 10;', statement)
                    self.assertIn('DEALLOCATE stannum_bench_plan;\nRESET plan_cache_mode;', statement)
                    if engine == 'stannum':
                        self.assertIn('stannum.full_score(ctid) AS score', statement)
                        self.assertIn('WHERE body ==> $1', statement)
                        self.assertIn('EXECUTE stannum_bench_plan(' + tin.literal(query[1]) + ')', statement)
                    else:
                        self.assertIn("ts_rank_cd(body_tsv, to_tsquery('simple', $1)) AS score", statement)
                        self.assertIn("WHERE body_tsv @@ to_tsquery('simple', $1)", statement)
        count = tin.prepared_plan_sql(query, 'stannum', 'count', 'force_custom_plan')
        self.assertIn('SELECT count(*)', count)
        self.assertNotIn('ORDER BY', count)
        with self.assertRaises(ValueError):
            tin.prepared_plan_sql(query, 'stannum', 'topk', 'invalid')

    def test_sql_keeps_query_text_inside_literal(self):
        text = "'quoted' ; DROP TABLE documents; --"
        self.assertEqual(tin.literal(text), "'''quoted'' ; DROP TABLE documents; --'")

class PairedMeasurementsTests(unittest.TestCase):
    def fixture(self, root):
        source = {'source_files': {'postgres/lib.rs': 'a' * 64}}
        source['source_sha256'] = tin.bench.digest(tin.bench.canonical(source['source_files']))
        campaign = dict(status='complete', repetitions=2, jobs=tin.paired_jobs(2),
                        sources={v: source for v in ('baseline', 'candidate')},
                        images={v: {'Id': 'sha256:' + v} for v in ('baseline', 'candidate')},
                        query_ids=['1:disjunction'])
        for job in campaign['jobs']:
            job['status'] = 'complete'
            path = root / job['directory']
            path.mkdir()
            manifest = dict(status='complete', image=campaign['images'][job['variant']]['Id'], source=source,
                            adapter={'binary_sha256': 'driver'}, corpus={'files': {'documents.csv': 'corpus'}},
                            harness_sources={'tin.py': 'runner'}, runner_sha256='runner',
                            config={k: 1 for k in tin.COMPARISON_SETTINGS},
                            host=dict(system='test', machine='arm64', docker={'NCPU': 4}),
                            jobs=[dict(status='complete', engine='stannum', correctness={'mismatches': 0},
                                       ranked_correctness={'mismatches': 0}, post_update_correctness={'mismatches': 0},
                                       input_sha256='input', settings={'work_mem': '16MB'},
                                       extensions={'stannum': '1'}, full_counts_before={'1:disjunction': 10})])
            manifest['jobs'][0]['workload_state'] = dict(
                protocol='postvacuum-observed-v1', **{
                    phase: dict(heap_pages=100, all_visible_pages=99, all_frozen_pages=0, table_options=None)
                    for phase in ('after_vacuum', 'before_driver', 'after_restart')})
            manifest['config']['workload'] = 'topk'
            tin.bench.save(path / 'manifest.json', manifest)
            # Both pairs improve 2x, but second pair runs on a slower background.
            latency = job['pair'] * (2 if job['variant'] == 'baseline' else 1)
            row = dict(engine='stannum', status='complete', qps=100 / latency,
                       p50_ms=latency, p95_ms=latency * 2, update_errors=0,
                       updates_attempted=1, updates_completed=1,
                       queries={'1:disjunction': dict(p50_ms=latency, p95_ms=latency * 2)})
            tin.bench.save(path / 'comparison.json', [row])
        tin.bench.save(root / 'paired.json', campaign)
        return campaign

    def test_visibility_drift_rejected_but_mutation_outcome_is_not_an_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            job = json.loads((root / 'r01-baseline/manifest.json').read_text())['jobs'][0]
            original = tin.workload_state_contract(job, updates=0)
            job['workload_state']['after_restart']['all_visible_pages'] = 50
            with self.assertRaisesRegex(ValueError, 'read-only trial'):
                tin.workload_state_contract(job, updates=0)
            self.assertEqual(original, tin.workload_state_contract(job, updates=1))
            job['workload_state']['before_driver']['all_visible_pages'] = 50
            with self.assertRaisesRegex(ValueError, 'untimed validation'):
                tin.workload_state_contract(job, updates=1)

    def test_visibility_requires_actual_valid_map_counts(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            job = json.loads((root / 'r01-baseline/manifest.json').read_text())['jobs'][0]
            for value in (101, -1, 99.5, None):
                with self.subTest(value=value):
                    job['workload_state']['before_driver']['all_visible_pages'] = value
                    with self.assertRaisesRegex(ValueError, 'invalid observed'):
                        tin.workload_state_contract(job, updates=1)

    def test_alternation_and_paired_ratios(self):
        self.assertEqual([j['variant'] for j in tin.paired_jobs(3)],
                         ['baseline', 'candidate', 'candidate', 'baseline', 'baseline', 'candidate'])
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            self.assertTrue(tin.paired_report(root))
            result = json.loads((root / 'aggregate.json').read_text())
            self.assertEqual(result['paired']['qps_ratio']['median'], 2)
            self.assertEqual(result['paired']['p95_speedup']['median'], 2)
            self.assertEqual(result['variants']['baseline']['qps']['min'], 25)
            self.assertGreater(result['variants']['baseline']['qps']['cv'], 0)

    def test_incompatible_or_incomplete_trials_withhold_all_ratios(self):
        mutations = {
            'table_options': lambda m, r: m['jobs'][0]['workload_state']['before_driver'].update(
                table_options=['autovacuum_enabled=false']),
            'missing_visibility': lambda m, r: m['jobs'][0].pop('workload_state'),
            'visibility': lambda m, r: [m['jobs'][0]['workload_state'][phase].update(all_visible_pages=80)
                                      for phase in ('after_vacuum', 'before_driver', 'after_restart')],
            'dataset': lambda m, r: m['corpus'].update(files={'documents.csv': 'changed'}),
            'input': lambda m, r: m['jobs'][0].update(input_sha256='changed'),
            'driver': lambda m, r: m['adapter'].update(binary_sha256='changed'),
            'source': lambda m, r: m['source'].update(source_sha256='changed'),
            'image': lambda m, r: m.update(image='retagged'),
            'settings': lambda m, r: m['jobs'][0]['settings'].update(work_mem='32MB'),
            'plan_cache_mode': lambda m, r: m['config'].update(plan_cache_mode='force_generic_plan'),
            'counts': lambda m, r: m['jobs'][0]['full_counts_before'].update({'1:disjunction': 9}),
            'failed': lambda m, r: m.update(status='failed'),
            'ranked': lambda m, r: m['jobs'][0]['ranked_correctness'].update(mismatches=1),
            'query_coverage': lambda m, r: r['queries'].clear(),
            'extra_query': lambda m, r: r['queries'].update({'2:phrase': {'p50_ms': 1, 'p95_ms': 1}}),
            'updates': lambda m, r: r.update(updates_completed=0),
            'zero_updates': lambda m, r: r.update(updates_attempted=0, updates_completed=0),
            'nan': lambda m, r: r.update(qps=float('nan')),
        }
        for name, mutate in mutations.items():
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                self.fixture(root)
                path = root / 'r02-candidate'
                manifest = json.loads((path / 'manifest.json').read_text())
                rows = json.loads((path / 'comparison.json').read_text())
                mutate(manifest, rows[0])
                tin.bench.save(path / 'manifest.json', manifest)
                tin.bench.save(path / 'comparison.json', rows)
                self.assertFalse(tin.paired_report(root))
                result = json.loads((root / 'aggregate.json').read_text())
                self.assertFalse(result['paired'])
                self.assertTrue(result['invalid_trials'])

    def test_interrupted_and_missing_schedule_do_not_publish_partial_speedups(self):
        for change in ('status', 'schedule', 'missing'):
            with self.subTest(change=change), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                campaign = self.fixture(root)
                if change == 'status':
                    campaign['status'] = 'failed'
                elif change == 'schedule':
                    campaign['jobs'].pop()
                else:
                    (root / 'r02-baseline/comparison.json').unlink()
                tin.bench.save(root / 'paired.json', campaign)
                self.assertFalse(tin.paired_report(root))
                self.assertFalse(json.loads((root / 'aggregate.json').read_text())['paired'])

    def test_read_only_comparison_allows_zero_updates(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            campaign = self.fixture(root)
            for job in campaign['jobs']:
                path = root / job['directory']
                manifest = json.loads((path / 'manifest.json').read_text())
                manifest['config']['updates'] = 0
                rows = json.loads((path / 'comparison.json').read_text())
                rows[0].update(updates_attempted=0, updates_completed=0)
                tin.bench.save(path / 'manifest.json', manifest)
                tin.bench.save(path / 'comparison.json', rows)
            self.assertTrue(tin.paired_report(root))

    def test_source_manifest_checks_file_fingerprint_consistency(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = self.fixture(root)['sources']['baseline']
            path = root / 'source.json'
            tin.bench.save(path, source)
            self.assertEqual(tin.recorded_source(path), source)
            source['source_files']['postgres/lib.rs'] = 'b' * 64
            tin.bench.save(path, source)
            with self.assertRaisesRegex(ValueError, 'inconsistent'):
                tin.recorded_source(path)

    def test_runner_pins_tags_once_and_alternates_fresh_trials(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture = root / 'fixture'
            fixture.mkdir()
            source = self.fixture(fixture)['sources']['baseline']
            source_path = root / 'source.json'
            tin.bench.save(source_path, source)
            args = argparse.Namespace(repetitions=2, output=root / 'comparison',
                                      baseline_image='baseline:tag', candidate_image='candidate:tag',
                                      baseline_source=source_path, candidate_source=source_path,
                                      driver=root / 'driver', style='disjunction')
            def inspect(command):
                variant = command[-1].split(':')[0]
                return json.dumps([{'Id': 'sha256:' + variant,
                                    'Config': {'Labels': {'benchmark.stannum_source_sha256': source['source_sha256']}}}])
            visited = []
            def run(trial):
                visited.append((trial.image, trial.output.name, trial.engines))
                self.assertEqual(tin.recorded_source(trial.source_manifest), source)
            with patch.object(tin, 'output', side_effect=inspect) as inspect_image, \
                    patch.object(tin, 'trace_queries', return_value=[('1:disjunction', '', '', '')]), \
                    patch.object(tin, 'run', side_effect=run), \
                    patch.object(tin, 'paired_report', return_value=True):
                tin.compare(args)
            self.assertEqual(inspect_image.call_count, 2)
            self.assertEqual(visited, [('sha256:' + v, name, ['stannum']) for v, name in
                                     [('baseline', 'r01-baseline'), ('candidate', 'r01-candidate'),
                                      ('candidate', 'r02-candidate'), ('baseline', 'r02-baseline')]])
            campaign = json.loads((args.output / 'paired.json').read_text())
            self.assertEqual(campaign['status'], 'complete')
            self.assertTrue(all(j['status'] == 'complete' for j in campaign['jobs']))

    def test_runner_keeps_failed_trial_and_pending_schedule_on_interruption(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture = root / 'fixture'
            fixture.mkdir()
            source = self.fixture(fixture)['sources']['baseline']
            source_path = root / 'source.json'
            tin.bench.save(source_path, source)
            args = argparse.Namespace(repetitions=2, output=root / 'comparison',
                                      baseline_image='baseline', candidate_image='candidate',
                                      baseline_source=source_path, candidate_source=source_path,
                                      driver=root / 'driver', style='mixed')
            image = json.dumps([{'Id': 'sha256:image', 'Config': {
                'Labels': {'benchmark.stannum_source_sha256': source['source_sha256']}}}])
            with patch.object(tin, 'output', return_value=image), \
                    patch.object(tin, 'trace_queries', return_value=[('1:disjunction', '', '', '')]), \
                    patch.object(tin, 'run', side_effect=KeyboardInterrupt('stopped')), \
                    self.assertRaises(KeyboardInterrupt):
                tin.compare(args)
            campaign = json.loads((args.output / 'paired.json').read_text())
            self.assertEqual(campaign['status'], 'failed')
            self.assertEqual([j['status'] for j in campaign['jobs']], ['failed', 'pending', 'pending', 'pending'])
            result = json.loads((args.output / 'aggregate.json').read_text())
            self.assertFalse(result['complete'])
            self.assertFalse(result['paired'])

    def test_single_repetition_is_not_a_variation_measurement(self):
        with self.assertRaisesRegex(ValueError, 'at least two'):
            tin.compare(argparse.Namespace(repetitions=1))


if __name__ == '__main__':
    unittest.main()
