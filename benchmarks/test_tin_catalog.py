# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import tin_catalog


class TinCatalogTests(unittest.TestCase):
    def test_matrix_compares_scoring_modes_and_scales_filter_selectivity(self):
        cases = tin_catalog.cases(10000)
        self.assertEqual(len({c['name'] for c in cases}), len(cases))
        self.assertEqual({c['mode'] for c in cases},
                         {'literal', 'force_custom_plan', 'force_generic_plan', 'auto'})
        dense = next(c for c in cases if c['name'] == 'score:p101')
        full = next(c for c in cases if c['name'] == 'full_score:p101')
        self.assertEqual(dense['query'], full['query'])
        for c in cases:
            if 'filter_fraction' in c:
                self.assertEqual(c['filter'], f"id <= {max(1, int(c['filter_fraction'] * 10000))}")
        self.assertIn('body ==> $1', tin_catalog.query_sql('fixture', full, parameter=True))
        full['query'] = "'quoted'"
        self.assertIn("'''quoted'''", tin_catalog.query_sql('fixture', full))

    def test_failure_cleans_only_a_schema_it_created_and_redacts_connection_errors(self):
        for fail_create in (False, True):
            calls = []
            def fake_run(argv, **kwargs):
                sql = argv[-1]
                calls.append(sql)
                if sql.startswith('SELECT json_build_object'):
                    return subprocess.CompletedProcess(argv, 0, json.dumps({'extensions': {'tin': 'test'}}), '')
                fail = sql.startswith('CREATE SCHEMA') if fail_create else sql.startswith('CREATE TABLE')
                return subprocess.CompletedProcess(argv, int(fail), '', 'SECRET credential error')
            with tempfile.TemporaryDirectory() as tmp, patch('tin_catalog.subprocess.run', side_effect=fake_run):
                out = Path(tmp) / 'output'
                with self.assertRaises(RuntimeError):
                    tin_catalog.run(argparse.Namespace(rows=[1000], output=out))
                metadata = (out / 'catalog.json').read_text()
                self.assertNotIn('SECRET', metadata)
                self.assertEqual(json.loads(metadata)['status'], 'failed')
                drops = [s for s in calls if s.startswith('DROP SCHEMA')]
                self.assertEqual(len(drops), 0 if fail_create else 1)
                if drops:
                    created = next(s for s in calls if s.startswith('CREATE SCHEMA')).split()[2].rstrip(';')
                    self.assertEqual(drops, [f'DROP SCHEMA {created} CASCADE;'])

    def test_invalid_sizes_fail_before_creating_output_or_connecting(self):
        with tempfile.TemporaryDirectory() as tmp, patch('tin_catalog.subprocess.run') as run:
            for rows in ([999], [50001], [1000,1000]):
                with self.assertRaises(ValueError):
                    tin_catalog.run(argparse.Namespace(rows=rows,output=Path(tmp)/'output'))
            run.assert_not_called()
            self.assertFalse((Path(tmp)/'output').exists())
