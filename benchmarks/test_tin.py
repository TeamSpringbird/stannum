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


class ExportTimingTests(unittest.TestCase):
    def test_report_rejects_idle_timeout_that_excludes_real_queries(self):
        import gzip
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            path = root / 'postgres'
            path.mkdir()
            (root / 'manifest.json').write_text(json.dumps(dict(status='complete', jobs=[dict(engine='postgres', status='complete', sizes={'index': 1, 'total': 2}, cross_engine_membership_differences=0)])))
            (path / 'result_test.json').write_text(json.dumps({'runs': {'postgres': {'startTime': 1000, 'endTime': 1268, 'queries': {'postgres': {}}}}}))
            with gzip.open(path / 'samples.json.gz', 'wt') as out:
                out.write(json.dumps(dict(type='Point', metric='query_duration', data=dict(time='1970-01-01T00:10:00.950999Z', value=4000, tags={'query_id':'1:disjunction'})))+'\n')
            with patch('tin.resources.summarize', return_value={}):
                with self.assertRaisesRegex(ValueError, 'outside exported measurement window'):
                    tin.report(root)
            self.assertFalse((root / 'comparison.json').exists())
            export = path / 'result_test.json'
            value = json.loads(export.read_text())
            value['runs']['postgres']['endTime'] = 600950
            export.write_text(json.dumps(value))
            with patch('tin.resources.summarize', return_value={}):
                tin.report(root)
            result = json.loads((root / 'comparison.json').read_text())[0]
            self.assertEqual(result['seconds'], 599.95)
            self.assertAlmostEqual(result['qps'], 1 / 599.95)


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

    def test_validation_sampling_is_explicit_deterministic_and_bounded(self):
        queries = list(range(906))
        self.assertEqual(tin.validation_queries(queries,0),queries)
        sample = tin.validation_queries(queries,18)
        self.assertEqual(len(sample),18)
        self.assertEqual(len(set(sample)),18)
        self.assertEqual(sample,tin.validation_queries(queries,18))
        self.assertEqual(len(queries),906)
        with self.assertRaises(ValueError):
            tin.validation_queries(queries,-1)

    def test_explicit_trace_is_used_and_duplicate_ids_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            trace = root/'alternate.json'
            entry = dict(source_id=99,text='alpha',engines={engine:{style:'alpha' for style in
                         ('conjunction','disjunction','phrase')} for engine in ('tin','postgres')})
            trace.write_text(json.dumps({'queries':[entry]}))
            self.assertEqual(tin.trace_queries(root,trace)[0][0],'99:conjunction')
            trace.write_text(json.dumps({'queries':[entry,entry]}))
            with self.assertRaisesRegex(ValueError,'unique'):
                tin.trace_queries(root,trace)

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
                        self.assertIn('stannum.score(ctid) AS score', statement)
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
                                       ranked_correctness={'mismatches': 0, 'queries': 1}, post_update_correctness={'mismatches': 0},
                                       post_update_ranked_correctness={'mismatches': 0, 'queries': 1},
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

    def test_trials_with_different_pinning_or_score_function_are_incompatible(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            self.assertTrue(tin.paired_report(root))
            for directory, key, value in (('r01-baseline', 'cpu_pinning', dict(server_cpuset='0-7')),
                                          ('r01-baseline', 'config', 'full_score')):
                path = root / directory / 'manifest.json'
                original = path.read_text()
                manifest = json.loads(original)
                if key == 'config':
                    manifest['config']['score_function'] = value
                else:
                    manifest[key] = value
                path.write_text(json.dumps(manifest))
                with self.subTest(key=key):
                    self.assertFalse(tin.paired_report(root))
                    self.assertIn('incompatible', (root / 'report.md').read_text())
                path.write_text(original)

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
            'effective_build_segment_docs': lambda m, r: m['jobs'][0].update(build_segment_docs=8192),
            'build_segment_docs': lambda m, r: m['config'].update(build_segment_docs=8192),
            'plan_cache_mode': lambda m, r: m['config'].update(plan_cache_mode='force_generic_plan'),
            'counts': lambda m, r: m['jobs'][0]['full_counts_before'].update({'1:disjunction': 9}),
            'failed': lambda m, r: m.update(status='failed'),
            'ranked': lambda m, r: m['jobs'][0]['ranked_correctness'].update(mismatches=1),
            'ranked_after': lambda m, r: m['jobs'][0]['post_update_ranked_correctness'].update(mismatches=1),
            'partial_ranked_after': lambda m, r: m['jobs'][0]['post_update_ranked_correctness'].update(queries=0),
            'different_ranked_after': lambda m, r: m['jobs'][0]['post_update_ranked_correctness'].update(queries=2),
            'missing_ranked_after': lambda m, r: m['jobs'][0].pop('post_update_ranked_correctness'),
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

class RawTextMembershipTests(unittest.TestCase):
    def test_raw_checks_use_tokenizer_not_ascii_regex(self):
        queries = [('1:phrase', '"Café Mixed"', 'café <-> mixed', 'Café Mixed')]
        raw = tin.check_sql(queries, raw_text=True)
        self.assertIn('SELECT id FROM reference WHERE body ==>', raw)
        self.assertNotIn('body ~', raw)
        self.assertIn('EXCEPT ALL', raw)
        semantics = tin.semantics_sql(queries, raw_text=True)
        self.assertIn('body ==>', semantics)
        self.assertNotIn('body ~', semantics)
        self.assertIn('body_tsv @@', semantics)

    def test_raw_trace_accepts_punctuation_without_changing_query_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp)/'queries.json'
            record = dict(source_id=1,text='Mixed C++',engines={e:{s:'"Mixed C++"' for s in
                          ('conjunction','disjunction','phrase')} for e in ('tin','postgres')})
            path.write_text(json.dumps({'queries':[record]}))
            with self.assertRaises(ValueError):
                tin.trace_queries(Path(tmp),path)
            queries=tin.trace_queries(Path(tmp),path,raw_text=True)
            self.assertEqual(len(queries),3)
            self.assertEqual(queries[0][1],'"Mixed C++"')


class SqlBatchTransportTests(unittest.TestCase):
    def test_large_batch_uses_stdin_and_preserves_transaction(self):
        batch = 'SELECT 1;\n' * 10000
        with patch.object(tin, 'output', return_value='ok') as call:
            self.assertEqual(tin.sql_output(batch, {}), 'ok')
            command = call.call_args.args[0]
            self.assertNotIn(batch, command)
            self.assertIn('--single-transaction', command)
            self.assertEqual(call.call_args.kwargs['input'], batch)
            tin.sql_output('VACUUM documents', {})
            self.assertIn('-c', call.call_args.args[0])
            self.assertNotIn('--single-transaction', call.call_args.args[0])


AWS_TOPOLOGY = Path(__file__).resolve().parent / 'aws/topology'


def i7i():
    # 16 cores x 2 threads, siblings numbered 16 apart, as Linux numbers an i7i.8xlarge.
    return (AWS_TOPOLOGY / 'i7i.8xlarge-assumed.lscpu').read_text()


class CpuPinningTests(unittest.TestCase):
    def test_cpusets_round_trip_and_reject_bad_input(self):
        self.assertEqual(tin.parse_cpuset('0-3,16-19'), [0, 1, 2, 3, 16, 17, 18, 19])
        self.assertEqual(tin.format_cpuset([19, 0, 1, 2, 3, 16, 17, 18]), '0-3,16-19')
        self.assertEqual(tin.format_cpuset([0, 2, 4]), '0,2,4')
        for bad in ('', '3-1', '0,0', 'a', '1-'):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                tin.parse_cpuset(bad)

    def test_lscpu_parsing_reads_vm_rows_without_numa(self):
        rows = tin.parse_lscpu('CPU CORE SOCKET NODE ONLINE\n  0    0      0    -    yes\n  1    1      0    -    no\n')
        self.assertEqual(rows, [dict(cpu=0, core=0, socket=0, node=0, online=True),
                                dict(cpu=1, core=1, socket=0, node=0, online=False)])
        with self.assertRaises(ValueError):
            tin.parse_lscpu('Architecture: x86_64\n')

    def test_siblings_take_whole_cores_and_the_client_gets_the_other_cores(self):
        plan = tin.plan_cpus(i7i(), 8, 'siblings')
        self.assertEqual((plan['server_cpuset'], plan['server_physical_cores'], plan['threads_per_core']),
                         ('0-3,16-19', 4, 2))
        self.assertEqual(plan['client_cpuset'], '4-15,20-31')
        self.assertEqual(plan['idle_siblings'], '')
        self.assertTrue(plan['smt'])

    def test_distinct_cores_leave_their_siblings_idle_rather_than_to_the_client(self):
        plan = tin.plan_cpus(i7i(), 8, 'distinct-cores')
        self.assertEqual((plan['server_cpuset'], plan['server_physical_cores']), ('0-7', 8))
        self.assertEqual(plan['idle_siblings'], '16-23')
        self.assertEqual(plan['client_cpuset'], '8-15,24-31')

    def test_without_smt_both_readings_are_the_same_cpus(self):
        text = (AWS_TOPOLOGY / 'i8g.8xlarge-assumed.lscpu').read_text()
        self.assertEqual(tin.plan_cpus(text, 8, 'siblings')['server_cpuset'],
                         tin.plan_cpus(text, 8, 'distinct-cores')['server_cpuset'])
        self.assertFalse(tin.plan_cpus(text, 8, 'siblings')['smt'])

    def test_explicit_cpusets_and_impossible_requests_fail_loudly(self):
        self.assertEqual(tin.plan_cpus(i7i(), 8, 'none', '8-11,24-27')['layout'], 'explicit')
        with self.assertRaisesRegex(ValueError, '--cpus'):
            tin.plan_cpus(i7i(), 8, 'none', '0-3')
        with self.assertRaisesRegex(ValueError, 'does not have'):
            tin.plan_cpus(i7i(), 8, 'none', '0-3,60-63')
        with self.assertRaisesRegex(ValueError, 'whole number'):
            tin.plan_cpus(i7i(), 7, 'siblings')
        with self.assertRaisesRegex(ValueError, 'fewer than'):
            tin.plan_cpus(i7i(), 17, 'distinct-cores')

    def test_client_is_pinned_only_where_the_driver_shares_the_kernel(self):
        plan = tin.plan_cpus(i7i(), 8, 'siblings')
        prefix, note = tin.client_pinning(plan, 'Darwin')
        self.assertEqual(prefix, [])
        self.assertIn('4-15,20-31', note)
        prefix, _ = tin.client_pinning(plan, 'Linux')
        self.assertEqual(prefix, ['taskset', '-c', '4-15,20-31'])
        self.assertEqual(tin.client_pinning(None, 'Linux')[0], [])


def parsed(*extra):
    return tin.parser().parse_args(['--driver', '/x/driver', 'run', '--dataset', '/x/data', '--output', '/x/out',
                                    '--image', 'stannum-bench:test', *extra])


class ProfileAndServerCommandTests(unittest.TestCase):
    def test_unprofiled_runs_keep_the_old_defaults(self):
        args = tin.apply_profile(parsed())
        self.assertEqual((args.cpus, args.memory, args.shared_buffers, args.maintenance_work_mem, args.clients,
                          args.shm_size, args.cpu_layout, args.build_memory, args.score_function),
                         (4, '4g', '1GB', '512MB', 2, '1g', 'none', None, 'score'))

    def test_v2_is_tin_106s_setup_and_explicit_flags_still_win(self):
        args = tin.apply_profile(parsed('--profile', 'v2'))
        self.assertEqual((args.cpus, args.memory, args.build_memory, args.shared_buffers, args.maintenance_work_mem,
                          args.cpu_layout, args.max_parallel_maintenance_workers, args.clients, args.shm_size),
                         (8, '64g', '64g', '24GB', '24GB', 'siblings', 8, 8, '16g'))
        args = tin.apply_profile(parsed('--profile', 'v2', '--memory', '8g', '--cpu-layout', 'distinct-cores'))
        self.assertEqual((args.memory, args.cpu_layout, args.build_memory), ('8g', 'distinct-cores', '64g'))

    def test_legacy_is_the_launch_posts_quota_without_pinning(self):
        args = tin.apply_profile(parsed('--profile', 'legacy'))
        self.assertEqual((args.memory, args.build_memory, args.cpu_layout, args.max_parallel_maintenance_workers),
                         ('32g', '64g', 'none', None))

    def test_namespaces_built_by_hand_get_the_new_options(self):
        args = tin.apply_profile(argparse.Namespace(cpus=8, memory='2g', shared_buffers='1GB'))
        self.assertEqual((args.profile, args.score_function, args.cpuset_cpus, args.paradedb_image),
                         (None, 'score', None, tin.PARADEDB_IMAGE))

    def test_server_command_pins_and_sizes_each_engine(self):
        args = tin.apply_profile(parsed('--profile', 'v2'))
        plan = tin.plan_cpus(i7i(), 8, 'siblings')
        stannum = tin.server_command(args, 'n', 'img', 'vol', Path('/p'), 'stannum', plan, ['--x'])
        self.assertEqual(stannum[stannum.index('--cpuset-cpus') + 1], '0-3,16-19')
        self.assertEqual(stannum[stannum.index('--memory') + 1], '64g')
        self.assertEqual(stannum[stannum.index('--shm-size') + 1], '16g')
        self.assertIn('--x', stannum)
        settings = [stannum[i + 1] for i, a in enumerate(stannum) if a == '-c']
        self.assertIn('max_parallel_maintenance_workers=8', settings)
        self.assertIn('jit=off', settings)
        paradedb = tin.server_command(args, 'n', 'pdb', 'vol', Path('/p'), 'paradedb', plan, [])
        settings = [paradedb[i + 1] for i, a in enumerate(paradedb) if a == '-c']
        # As the benchmarker starts ParadeDB: its auto-tuned work_mem and JIT stay.
        self.assertNotIn('jit=off', settings)
        self.assertNotIn('work_mem=16MB', settings)
        self.assertIn('max_parallel_workers_per_gather=2', settings)
        unpinned = tin.server_command(tin.apply_profile(parsed('--profile', 'legacy')), 'n', 'img', 'vol', Path('/p'),
                                      'stannum', None, [])
        self.assertNotIn('--cpuset-cpus', unpinned)
        self.assertNotIn('max_parallel_maintenance_workers=8', unpinned)

    def test_dry_run_prints_commands_without_docker_or_driver(self):
        import io
        from contextlib import redirect_stdout
        args = parsed('--profile', 'v2', '--lscpu-file', str(AWS_TOPOLOGY / 'i7i.8xlarge-assumed.lscpu'),
                      '--engines', 'stannum', 'paradedb', '--score-function', 'full_score', '--dry-run')
        out = io.StringIO()
        with redirect_stdout(out), patch.object(tin, 'output', side_effect=AssertionError('no docker')):
            tin.run(args)
        text = out.getvalue()
        self.assertEqual(text.count('docker run -d'), 2)
        self.assertIn('--cpuset-cpus 0-3,16-19', text)
        self.assertIn('STANNUM_SCORE_FUNCTION=full_score', text)
        self.assertIn('taskset -c 4-15,20-31', text)
        self.assertIn(tin.PARADEDB_IMAGE, text)

    def test_score_function_reaches_checks_and_plans(self):
        query = ('7:phrase', '"a b"', "'a' <-> 'b'", 'a b')
        self.assertIn('stannum.full_score(ctid)', tin.ranked_check_sql([query], 'stannum', 'full_score'))
        self.assertIn('stannum.full_score(ctid)',
                      tin.prepared_plan_sql(query, 'stannum', 'topk', 'force_custom_plan', 'full_score'))
        plan = tin.prepared_plan_sql(query, 'paradedb', 'topk', 'force_custom_plan')
        self.assertIn('body ### $1', plan)
        self.assertIn("EXECUTE stannum_bench_plan('a b')", plan)
        self.assertEqual(tin.paradedb_count_sql([('3:disjunction', '', '', "it's")]),
                         "SELECT '3:disjunction', count(*) FROM documents WHERE body ||| 'it''s';")

    def test_index_bytes_per_query_is_the_benchmarkers_per_query(self):
        self.assertEqual(tin.index_mib_per_query({'indexReadBytes': 2**20, 'indexHitBytes': 3 * 2**20}, 2), 2.0)
        self.assertIsNone(tin.index_mib_per_query({'indexReadBytes': 1}, 2))
        self.assertIsNone(tin.index_mib_per_query({'indexReadBytes': 1, 'indexHitBytes': 1}, 0))

    def test_target_cpu_is_refused_off_x86(self):
        args = argparse.Namespace(target_cpu='x86-64-v4', output=Path('/nonexistent/never'), base=None)
        with patch.object(tin.platform, 'machine', return_value='aarch64'):
            with self.assertRaisesRegex(ValueError, 'x86-64'):
                tin.build_image(args)
