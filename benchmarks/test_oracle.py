import json
import unittest
from unittest.mock import patch
from subprocess import CompletedProcess

import oracle


class OracleIdentityTests(unittest.TestCase):
    def test_differential_queries_bind_to_each_engines_schema(self):
        payload = {'ids': [1], 'full': [[1, 'bits']], 'dense': [[1, 'bits']], 'max': 'bits'}
        with patch.object(oracle, 'psql') as call:
            for engine in ('stannum', 'tin'):
                call.side_effect = [CompletedProcess([], 0, json.dumps(payload)), CompletedProcess([], 0, '[1, "<b>rare</b>"]\n'), CompletedProcess([], 0, '[1, "rare"]\n')]
                expected = dict(payload, highlights={'html': [[1, '<b>rare</b>']], 'ansi': [[1, 'rare']]})
                self.assertEqual(oracle.observe({}, 'rare', engine), expected)
                sql = call.call_args_list[-3].args[0]
                self.assertIn(f'{engine}.highlight(body)', call.call_args_list[-2].args[0])
                self.assertNotIn('FROM (', call.call_args_list[-2].args[0])
                self.assertIn('ORDER BY id', call.call_args_list[-2].args[0])
                self.assertIn(f'{engine}.highlight_ansi(body)', call.call_args_list[-1].args[0])
                for fn in ('full_score', 'score', 'max_score'):
                    self.assertIn(f'{engine}.{fn}(', sql)
                other = 'tin' if engine == 'stannum' else 'stannum'
                self.assertNotIn(f'{other}.', sql)
                self.assertIn(f'USING {engine}(body)', oracle.FIXTURE.format(rows=5000, engine=engine))


class OracleScoreModeTests(unittest.TestCase):
    def test_order_mode_ignores_score_bits_but_not_rank_or_membership(self):
        left = {'ids': [1, 2, 3], 'full': [[1, '\\x40000000'], [2, '\\x40400000'], [3, '\\x3f800000']],
                'dense': [[1, '\\x40000000'], [2, '\\x40400000'], [3, '\\x3f800000']], 'max': '\\x40400000', 'highlights': {'html': [], 'ansi': []}}
        # Same ranking (2, 1, 3) with different bits and a different max.
        right = {'ids': [1, 2, 3], 'full': [[1, '\\x40000001'], [2, '\\x40400001'], [3, '\\x3f800001']],
                 'dense': [[1, '\\x40000001'], [2, '\\x40400001'], [3, '\\x3f800001']], 'max': '\\x40400001', 'highlights': {'html': [], 'ansi': []}}
        self.assertNotEqual(oracle.comparable(left, 'bits'), oracle.comparable(right, 'bits'))
        self.assertEqual(oracle.comparable(left, 'order'), oracle.comparable(right, 'order'))
        self.assertEqual(oracle.ranking(left['full']), [2, 1, 3])
        swapped = dict(right, full=[[1, '\\x40400001'], [2, '\\x40000001'], [3, '\\x3f800001']])
        self.assertNotEqual(oracle.comparable(left, 'order'), oracle.comparable(swapped, 'order'))
        self.assertNotEqual(oracle.comparable(left, 'order'), oracle.comparable(dict(right, ids=[1, 2]), 'order'))
        self.assertEqual(oracle.comparable({'error': 'x'}, 'order'), {'error': 'x'})

    def test_order_mode_compares_only_membership_for_unscored_expansion_shapes(self):
        left = {'ids': [1, 2], 'full': [[1, '\\x40000000'], [2, '\\x40400000']], 'dense': [], 'max': '\\x40400000', 'highlights': {'html': [], 'ansi': []}}
        zero = {'ids': [1, 2], 'full': [[1, '\\x00000000'], [2, '\\x00000000']], 'dense': [], 'max': '\\x00000000', 'highlights': {'html': [], 'ansi': []}}
        self.assertIn('alp*', oracle.REFERENCE_UNSCORED)
        self.assertIn('alpha TO beta', oracle.REFERENCE_UNSCORED)
        self.assertNotIn('"alpha beta"', oracle.REFERENCE_UNSCORED)
        self.assertEqual(oracle.comparable(left, 'order', 'alp*'), oracle.comparable(zero, 'order', 'alp*'))
        self.assertNotEqual(oracle.comparable(left, 'order', 'rare'), oracle.comparable(zero, 'order', 'rare'))


class OracleHighlightTests(unittest.TestCase):
    def test_highlights_compare_exactly_even_when_expansion_scores_do_not(self):
        left = {'ids': [1], 'full': [], 'dense': [], 'highlights': {'html': [[1, '<b>alpha</b>']], 'ansi': [[1, 'alpha']]}}
        changed = dict(left, highlights={'html': [[1, 'alpha']], 'ansi': [[1, 'alpha']]})
        for query in ('rare', 'alp*'):
            self.assertNotEqual(oracle.comparable(left, 'order', query), oracle.comparable(changed, 'order', query))
        self.assertNotEqual(oracle.comparable(left, 'bits'), oracle.comparable(changed, 'bits'))

    def test_highlight_errors_remain_observable(self):
        payload = {'ids': [1], 'full': [], 'dense': [], 'max': None}
        with patch.object(oracle, 'psql', side_effect=[CompletedProcess([], 0, json.dumps(payload)),
                CompletedProcess([], 1, '', 'unsupported highlight'), CompletedProcess([], 0, '')]):
            observed = oracle.observe({}, "can't", 'tin')
        self.assertEqual(observed['ids'], [1])
        self.assertEqual(observed['highlights']['html'], {'error': 'unsupported highlight'})
        self.assertTrue(oracle.unexpected_highlight_error(observed, 'order', 'rare'))
        with patch.dict(oracle.REFERENCE_UNHIGHLIGHTED, {'rare': 'documented reference defect'}):
            self.assertFalse(oracle.unexpected_highlight_error(observed, 'order', 'rare'))
            self.assertTrue(oracle.unexpected_highlight_error(observed, 'bits', 'rare'))

    def test_reference_exclusion_keeps_membership_and_only_affects_order_mode(self):
        left = {'ids': [1], 'full': [], 'dense': [], 'highlights': {'html': [[1, 'a']], 'ansi': []}}
        right = dict(left, highlights={'html': [[1, 'b']], 'ansi': []})
        with patch.dict(oracle.REFERENCE_UNHIGHLIGHTED, {'rare': 'documented reference defect'}):
            self.assertEqual(oracle.comparable(left, 'order', 'rare'), oracle.comparable(right, 'order', 'rare'))
            self.assertNotEqual(oracle.comparable(left, 'bits', 'rare'), oracle.comparable(right, 'bits', 'rare'))
            self.assertNotEqual(oracle.comparable(left, 'order', 'rare'), oracle.comparable(dict(right, ids=[]), 'order', 'rare'))

    def test_audit_shapes_and_exclusions_are_explicit(self):
        self.assertEqual(len(oracle.QUERIES), 47)
        self.assertEqual(len(set(oracle.QUERIES)), 47)
        for query in ('eclair', '3.14', "can't", 'wi-fi', 'example.com', '👩‍💻'):
            self.assertIn(query, oracle.QUERIES)
        self.assertTrue(set(oracle.REFERENCE_UNHIGHLIGHTED) <= set(oracle.QUERIES))
        self.assertTrue(all(oracle.REFERENCE_UNHIGHLIGHTED.values()))
