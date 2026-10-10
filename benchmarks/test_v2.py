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
        status=status, profile='v2', postgres_version='18.6', cpu_pinning=dict(server_cpuset='0-3,16-19'))))
    (path / 'comparison.json').write_text(json.dumps([dict(
        engine=engine, status=status, qps=qps, p50_ms=5.0, p99_ms=50.0, seconds=600,
        index_bytes=1, index_mib_per_query=mib)]))


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

    def test_plan_covers_every_scenario_and_variant_with_one_paradedb_run_each(self):
        runs = v2.planned_runs(['siblings'], v2.SCENARIOS, ['stannum', 'paradedb'], ['score', 'full_score'])
        self.assertEqual(len(runs), 12)
        self.assertEqual(sum(engine == 'paradedb' for _, engine, _, _ in runs), 4)
        self.assertEqual(len(v2.planned_runs(['siblings'], ['mixed'], ['stannum'], ['score'])), 1)

    def test_tin_arguments_select_the_engines_database_and_the_reading(self):
        args = argparse.Namespace(
            driver=Path('/d'), dataset=Path('/ds'), rows=150000000, validation_queries=10,
            ranked_validation_queries=2, profile='v2', seconds=600, warmup=10, setup_timeout_seconds=1,
            port=1, image='img', paradedb_image='pdb', stannum_database=Path('/s'), paradedb_database=Path('/p'),
            source_manifest=None, lscpu_file=None, extra=['--x'])
        command = v2.tin_arguments(args, 'distinct-cores', 'paradedb', 'score', 'phrase', Path('/o'))
        self.assertEqual(command[command.index('--load-database') + 1], '/p')
        self.assertEqual(command[command.index('--cpu-layout') + 1], 'distinct-cores')
        self.assertEqual(command[command.index('--style') + 1], 'phrase')
        self.assertEqual(command[-1], '--x')
        command = v2.tin_arguments(args, 'siblings', 'stannum', 'full_score', 'mixed', Path('/o'))
        self.assertEqual(command[command.index('--load-database') + 1], '/s')
        self.assertEqual(command[command.index('--score-function') + 1], 'full_score')


class ReportTests(unittest.TestCase):
    def test_hardware_factor_comes_from_paradedb_and_translates_stannum(self):
        published = json.loads(v2.PUBLISHED.read_text())
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            save_run(root, 'siblings', 'paradedb', 'score', 'mixed', qps=49.0)        # 0.5x i7i's 98
            save_run(root, 'siblings', 'stannum', 'score', 'mixed', qps=300.0, mib=27.5)
            save_run(root, 'siblings', 'stannum', 'full_score', 'mixed', qps=90.0, status='failed')
            result = v2.comparison(root, published)['siblings']
            mixed = result['mixed']
            self.assertAlmostEqual(mixed['hardware_factor']['x86'], 0.5)
            self.assertAlmostEqual(mixed['hardware_factor']['arm'], 49 / 79)
            rows = {row['theirs']: row for row in mixed['rows']}
            self.assertAlmostEqual(rows['TIN']['estimated_qps_on']['x86'], 600.0)
            self.assertEqual(rows['TIN']['published']['x86']['qps'], 413)
            self.assertEqual(rows['TIN_FULL']['measured']['status'], 'incomplete')
            self.assertNotIn('estimated_qps_on', rows['TIN_FULL'])
            self.assertIsNone(result['conjunction']['hardware_factor']['x86'])
            text = v2.render(v2.comparison(root, published), published)
            self.assertIn('| Stannum | 300.0 | 50.0 | 27.5 | TIN | 413 / 119 / 27 | 265 / 218 / 27 | 600 |', text)

    def test_missing_paradedb_runs_come_from_the_anchor_campaign(self):
        published = json.loads(v2.PUBLISHED.read_text())
        with tempfile.TemporaryDirectory() as tmp:
            anchor, campaign = Path(tmp) / 'baseline', Path(tmp) / 'x86-64-v4'
            save_run(anchor, 'siblings', 'paradedb', 'score', 'phrase', qps=83.0)
            save_run(campaign, 'siblings', 'stannum', 'score', 'phrase', qps=400.0)
            phrase = v2.comparison(campaign, published, anchor)['siblings']['phrase']
            self.assertAlmostEqual(phrase['hardware_factor']['x86'], 0.5)
            paradedb = [r for r in phrase['rows'] if r['theirs'] == 'ParadeDB'][0]
            self.assertEqual(paradedb['measured']['from_campaign'], str(anchor))


if __name__ == '__main__':
    unittest.main()
