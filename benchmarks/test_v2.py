# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import argparse
import json
from pathlib import Path
import tempfile
import unittest

import v2

TOPOLOGY = Path(__file__).resolve().parent / 'aws/topology'


def save_run(root, layout, engine, score, scenario, qps, mib=10.0, status='complete'):
    path = v2.run_directory(root, layout, engine, score, scenario)
    path.mkdir(parents=True)
    (path / 'manifest.json').write_text(json.dumps(dict(
        status=status, profile='v2', postgres_version='18.6', host=dict(machine='arm64'),
        cpu_pinning=dict(server_cpuset='0-7'))))
    (path / 'comparison.json').write_text(json.dumps([dict(
        engine=engine, status=status, qps=qps, p50_ms=5.0, p99_ms=50.0, seconds=600,
        index_bytes=1, index_mib_per_query=mib)]))


def arguments(**overrides):
    values = dict(driver=Path('/d'), dataset=None, rows=150000000, validation_queries=3,
                  ranked_validation_queries=1, profile='v2', seconds=600, warmup=10, setup_timeout_seconds=1,
                  port=1, image='img', paradedb_image='pdb', database=Path('/s'), paradedb_database=None,
                  source_manifest=None, lscpu_file=None, extra=[])
    values.update(overrides)
    return argparse.Namespace(**values)


class PublishedDataTests(unittest.TestCase):
    def test_every_v106_scenario_has_tin_tin_full_and_paradedb_on_both_platforms(self):
        published = json.loads(v2.PUBLISHED.read_text())
        datasets = {d['id']: d for d in published['datasets']}
        for name in ('tin-1.0.6-x86', 'tin-1.0.6-arm'):
            self.assertEqual(datasets[name]['source'], 'https://planetscale.com/blog/tin-v106')
            for scenario in v2.SCENARIOS:
                for engine in ('TIN', 'TIN_FULL', 'ParadeDB'):
                    value = datasets[name]['results'][scenario][engine]
                    self.assertEqual(set(value), {'qps', 'p99_ms', 'mb_per_query'})
        self.assertEqual(datasets['tin-1.0.6-x86']['results']['mixed']['TIN']['qps'], 413)
        self.assertEqual(datasets['tin-1.0.6-arm']['results']['disjunction']['ParadeDB']['qps'], 45)
        self.assertEqual(datasets['paradedb-opening-a-closed-tin']['kind'], 'third-party')
        self.assertTrue(all(d['source'].startswith('https://') for d in published['datasets']))


class CampaignTests(unittest.TestCase):
    def test_both_readings_run_on_smt_hosts_and_once_without_smt(self):
        i7i = (TOPOLOGY / 'i7i.8xlarge-assumed.lscpu').read_text()
        i8g = (TOPOLOGY / 'i8g.8xlarge-assumed.lscpu').read_text()
        self.assertEqual(v2.layouts_to_run(i7i, ['siblings', 'distinct-cores']), ['siblings', 'distinct-cores'])
        self.assertEqual(v2.layouts_to_run(i8g, ['siblings', 'distinct-cores']), ['siblings'])

    def test_paradedb_runs_only_when_its_database_is_given(self):
        runs = v2.planned_runs(['siblings'], v2.SCENARIOS, ['score', 'full_score'])
        self.assertEqual(len(runs), 8)
        self.assertFalse(any(engine == 'paradedb' for _, engine, _, _ in runs))
        runs = v2.planned_runs(['siblings'], v2.SCENARIOS, ['score', 'full_score'], paradedb=True)
        self.assertEqual(sum(engine == 'paradedb' for _, engine, _, _ in runs), 4)
        self.assertEqual(len(v2.planned_runs(['siblings'], ['mixed'], ['score'])), 1)

    def test_tin_arguments_select_the_database_and_reading_and_need_no_corpus(self):
        args = arguments(paradedb_database=Path('/p'), extra=['--x'])
        command = v2.tin_arguments(args, 'distinct-cores', 'paradedb', 'score', 'phrase', Path('/o'))
        self.assertEqual(command[command.index('--load-database') + 1], '/p')
        self.assertEqual(command[command.index('--paradedb-image') + 1], 'pdb')
        self.assertEqual(command[command.index('--cpu-layout') + 1], 'distinct-cores')
        self.assertEqual(command[-1], '--x')
        command = v2.tin_arguments(args, 'siblings', 'stannum', 'full_score', 'mixed', Path('/o'))
        self.assertEqual(command[command.index('--load-database') + 1], '/s')
        self.assertEqual(command[command.index('--score-function') + 1], 'full_score')
        self.assertNotIn('--dataset', command)
        self.assertNotIn('--paradedb-image', command)


class ReportTests(unittest.TestCase):
    def test_without_paradedb_runs_the_published_numbers_stand_alone(self):
        published = json.loads(v2.PUBLISHED.read_text())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            save_run(root, 'siblings', 'stannum', 'score', 'mixed', qps=300.0, mib=27.5)
            save_run(root, 'siblings', 'stannum', 'full_score', 'mixed', qps=90.0, status='failed')
            data = v2.comparison(root, published)['siblings']
            rows = {row['theirs']: row for row in data['scenarios']['mixed']['rows']}
            self.assertEqual(rows['TIN']['published']['x86']['qps'], 413)
            self.assertEqual(rows['TIN_FULL']['measured']['status'], 'incomplete')
            self.assertEqual(rows['ParadeDB']['measured']['status'], 'missing')
            self.assertNotIn('scaled_qps', rows['TIN'])
            self.assertIsNone(data['factor_spread']['arm'])
            text = v2.render(v2.comparison(root, published), published)
            self.assertIn('| mixed | Stannum | 300.0 | 50.0 | 27.5 | TIN | 265 / 218 / 27 | 413 / 119 / 27 |', text)
            self.assertIn('| mixed | (published only) |', text)
            self.assertIn('no calibration', text)

    def test_paradedb_factor_scales_stannum_and_its_spread_is_reported(self):
        published = json.loads(v2.PUBLISHED.read_text())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            # Twice the published i8g ParadeDB QPS in every scenario: a steady factor of 2.
            for scenario, arm_qps in (('conjunction', 142), ('disjunction', 45), ('phrase', 122), ('mixed', 79)):
                save_run(root, 'siblings', 'paradedb', 'score', scenario, qps=2 * arm_qps)
                save_run(root, 'siblings', 'stannum', 'score', scenario, qps=1000.0)
            data = v2.comparison(root, published)['siblings']
            mixed = data['scenarios']['mixed']
            self.assertAlmostEqual(mixed['factor']['arm'], 2.0)
            self.assertAlmostEqual(mixed['factor']['x86'], 158 / 98)
            tin = [r for r in mixed['rows'] if r['theirs'] == 'TIN'][0]
            self.assertAlmostEqual(tin['scaled_qps']['arm'], 500.0)
            self.assertAlmostEqual(data['factor_spread']['arm']['max_over_min'], 1.0)
            self.assertGreater(data['factor_spread']['x86']['max_over_min'], 1.0)
            text = v2.render(v2.comparison(root, published), published)
            self.assertIn('rough approximation', text)
            self.assertIn('Factor vs i8g across 4 scenarios: 2.00 to 2.00 (max/min 1.00, CV 0.00): steady.', text)
            self.assertIn('| mixed | 2.00 | 1.61 | 500 (265) |', text)


if __name__ == '__main__':
    unittest.main()
