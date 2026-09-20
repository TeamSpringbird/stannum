# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import query_shapes as shapes


class QueryShapesTests(unittest.TestCase):
    def test_wide_queries_use_distinct_terms_and_independent_membership(self):
        entries = {name: (query, predicate) for name, query, predicate in shapes.expressions()}
        for size in (2, 8, 32, 128):
            for op in ('and', 'or'):
                query, predicate = entries[f'{op}_{size}']
                self.assertEqual(len(set(query.split(f' {op.upper()} '))), size)
                self.assertNotIn('==>', predicate)
                self.assertEqual(predicate.count('strpos('), size)

    def test_engine_validation_precedes_sql_generation(self):
        for engine, rows in [('tin;DROP TABLE x',128), ('stannum',0)]:
            with self.assertRaises(ValueError):
                shapes.fixture(rows,engine)

    def test_validation_keeps_failure_and_checks_complete_output(self):
        cases = shapes.catalog('stannum')[:2]
        responses = [{'metadata': {'version':'test'}},
                     {'case':cases[0]['name'],'status':'passed'},
                     {'case':cases[1]['name'],'status':'failed','sqlstate':'XX000'}]
        result = SimpleNamespace(returncode=0,stdout='\n'.join(map(json.dumps,responses)),stderr='')
        with patch.object(shapes.subprocess,'run',return_value=result):
            checks, metadata = shapes.validate(cases,[],{})
            self.assertEqual(checks[1]['status'],'failed')
            self.assertEqual(metadata['version'],'test')
            with self.assertRaisesRegex(RuntimeError,'incomplete'):
                shapes.validate(cases+[cases[0]],[],{})

    def test_ranked_oracle_materializes_unlimited_scores(self):
        case = next(c for c in shapes.catalog('stannum') if c['name']=='or_128_ranked')
        self.assertIn('AS MATERIALIZED',case['reference'])
        self.assertNotIn('LIMIT',case['reference'].split(') SELECT')[0])
        self.assertIn('EXCEPT ALL',shapes.checks(case))
        self.assertIn('float4send',shapes.checks(case))
        self.assertEqual(len({c['name'] for c in shapes.catalog('tin')}),44)


if __name__ == '__main__':
    unittest.main()
