import json
import unittest
from unittest.mock import patch
from subprocess import CompletedProcess

import oracle


class OracleIdentityTests(unittest.TestCase):
    def test_differential_queries_bind_to_each_engines_schema(self):
        payload = {'ids': [1], 'full': [[1, 'bits']], 'dense': [[1, 'bits']], 'max': 'bits'}
        with patch.object(oracle, 'psql', return_value=CompletedProcess([], 0, json.dumps(payload))) as call:
            for engine in ('stannum', 'tin'):
                self.assertEqual(oracle.observe({}, 'rare', engine), payload)
                sql = call.call_args.args[0]
                for fn in ('full_score', 'score', 'max_score'):
                    self.assertIn(f'{engine}.{fn}(', sql)
                other = 'tin' if engine == 'stannum' else 'stannum'
                self.assertNotIn(f'{other}.', sql)
                self.assertIn(f'USING {engine}(body)', oracle.FIXTURE.format(rows=5000, engine=engine))
