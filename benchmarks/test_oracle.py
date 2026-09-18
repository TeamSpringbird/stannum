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


class OracleScoreModeTests(unittest.TestCase):
    def test_order_mode_ignores_score_bits_but_not_rank_or_membership(self):
        left = {'ids': [1, 2, 3], 'full': [[1, '\\x40000000'], [2, '\\x40400000'], [3, '\\x3f800000']],
                'dense': [[1, '\\x40000000'], [2, '\\x40400000'], [3, '\\x3f800000']], 'max': '\\x40400000'}
        # Same ranking (2, 1, 3) with different bits and a different max.
        right = {'ids': [1, 2, 3], 'full': [[1, '\\x40000001'], [2, '\\x40400001'], [3, '\\x3f800001']],
                 'dense': [[1, '\\x40000001'], [2, '\\x40400001'], [3, '\\x3f800001']], 'max': '\\x40400001'}
        self.assertNotEqual(oracle.comparable(left, 'bits'), oracle.comparable(right, 'bits'))
        self.assertEqual(oracle.comparable(left, 'order'), oracle.comparable(right, 'order'))
        self.assertEqual(oracle.ranking(left['full']), [2, 1, 3])
        swapped = dict(right, full=[[1, '\\x40400001'], [2, '\\x40000001'], [3, '\\x3f800001']])
        self.assertNotEqual(oracle.comparable(left, 'order'), oracle.comparable(swapped, 'order'))
        self.assertNotEqual(oracle.comparable(left, 'order'), oracle.comparable(dict(right, ids=[1, 2]), 'order'))
        self.assertEqual(oracle.comparable({'error': 'x'}, 'order'), {'error': 'x'})

    def test_order_mode_compares_only_membership_for_unscored_expansion_shapes(self):
        left = {'ids': [1, 2], 'full': [[1, '\\x40000000'], [2, '\\x40400000']], 'dense': [], 'max': '\\x40400000'}
        zero = {'ids': [1, 2], 'full': [[1, '\\x00000000'], [2, '\\x00000000']], 'dense': [], 'max': '\\x00000000'}
        self.assertIn('alp*', oracle.REFERENCE_UNSCORED)
        self.assertIn('alpha TO beta', oracle.REFERENCE_UNSCORED)
        self.assertNotIn('"alpha beta"', oracle.REFERENCE_UNSCORED)
        self.assertEqual(oracle.comparable(left, 'order', 'alp*'), oracle.comparable(zero, 'order', 'alp*'))
        self.assertNotEqual(oracle.comparable(left, 'order', 'rare'), oracle.comparable(zero, 'order', 'rare'))
