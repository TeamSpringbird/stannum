# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

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


class TraceOracleTests(unittest.TestCase):
    def test_trace_checks_exact_membership_scores_and_topk(self):
        ref = [[1, '\\x40000000'], [2, '\\x3f800000']]
        self.assertEqual(oracle.compare_trace(ref, ref, ref), [])
        self.assertIn('membership', oracle.compare_trace(ref[:1], ref, ref))
        changed = [[1, '\\x40400000'], ref[1]]
        self.assertIn('full_score_bits', oracle.compare_trace(changed, ref, ref))
        self.assertIn('top10', oracle.compare_trace(ref, ref, list(reversed(ref))))
        self.assertIn('top10', oracle.compare_trace(ref, ref, ref[:1]))
        self.assertIn('top10', oracle.compare_trace(ref, ref, [[9, ref[0][1]], ref[1]]))

    def test_topk_allows_boundary_ties_but_not_duplicates_or_nonfinite_scores(self):
        rows = [[n, '\\x3f800000'] for n in range(20)]
        self.assertEqual(oracle.compare_trace(rows, rows, list(reversed(rows[10:]))), [])
        for invalid in [rows[:9] + [rows[0]], [[1, '\\x7f800000']], [[1, '\\x7fc00000']]]:
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                oracle.compare_trace(rows, rows, invalid)
        self.assertEqual(oracle.compare_trace([], [], []), [])

    def test_trace_keeps_all_forms_and_repeated_text_but_rejects_duplicate_ids(self):
        import tempfile
        from pathlib import Path
        record = {'source_id': 1, 'engines': {'tin': {style: 'same' for style in
                  ('conjunction', 'disjunction', 'phrase')}}}
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'queries.json'
            path.write_text(json.dumps({'queries': [record, dict(record, source_id=2)]}))
            self.assertEqual(len(oracle.trace_queries(path)), 6)
            path.write_text(json.dumps({'queries': [record, record]}))
            with self.assertRaises(ValueError):
                oracle.trace_queries(path)

    def test_trace_sql_projects_all_scores_and_real_topk_without_id_tiebreak(self):
        query = "can't OR rare"
        exhaustive = oracle.trace_sql(query, 'tin')
        ranked = oracle.trace_sql(query, 'stannum', True)
        self.assertIn("can't".replace("'", "''"), exhaustive)
        self.assertTrue(exhaustive.endswith('ORDER BY id'))
        self.assertTrue(ranked.endswith('ORDER BY stannum.full_score(ctid) DESC LIMIT 10'))
        self.assertIn('float4send(tin.full_score(ctid))', exhaustive)


class BoundaryOracleTests(unittest.TestCase):
    def test_maximum_is_checked_independently_of_cross_engine_bits(self):
        case = oracle.boundary_cases('stannum')[0]
        case = dict(case, expected={'count': 2})
        good = {'rows': [[1, '\\x3f800000', '\\x40000000'], [2, '\\x40000000', '\\x40000000']]}
        normalized, issues = oracle.boundary_check(case, good)
        self.assertEqual(issues, [])
        self.assertTrue(normalized['max_ok'])
        for rows in [[[1, '\\x3f800000', '\\x3f800000'], [2, '\\x40000000', '\\x40000000']],
                     [[1, '\\x3f800000', None], [2, '\\x40000000', None]]]:
            self.assertIn('max_score_invariant', oracle.boundary_check(case, {'rows': rows})[1])

    def test_density_and_error_expectations_cannot_pass_vacuously(self):
        cases = {c['name']: c for c in oracle.boundary_cases('tin')}
        self.assertIn('documented_membership', oracle.boundary_check(cases['density_nine'], {'rows': []})[1])
        self.assertIn('documented_density', oracle.boundary_check(cases['density_nine'],
            {'rows': [[n, '\\x00000000', '\\x00000000'] for n in range(9)]})[1])
        self.assertIn('expected_error', oracle.boundary_check(cases['empty_phrase'], {'rows': []})[1])
        self.assertIn('unexpected_error', oracle.boundary_check(cases['empty_text'],
            {'error': 'failed', 'sqlstate': 'XX000'})[1])
        self.assertIn('unexpected_error_sqlstate', oracle.boundary_check(cases['empty_phrase'],
            {'error': 'missing function', 'sqlstate': '42883'})[1])
        self.assertEqual(oracle.boundary_check(cases['empty_phrase'],
            {'error': 'parse failed', 'sqlstate': 'XX000'})[1], [])

    def test_boundary_errors_retain_diagnostics_and_sqlstate(self):
        case = oracle.boundary_cases('tin')[0]
        with patch.object(oracle, 'psql', return_value=CompletedProcess([], 1, '',
                'ERROR:  22023: invalid argument\nDETAIL: preserved')):
            observed = oracle.boundary_observe(case, {})
        self.assertEqual(observed['sqlstate'], '22023')
        self.assertIn('DETAIL: preserved', observed['error'])

    def test_boundary_fixture_does_not_change_existing_campaigns(self):
        left, right = oracle.boundary_cases('stannum'), oracle.boundary_cases('tin')
        self.assertEqual(len(left), 24)
        self.assertEqual([x['name'] for x in left], [x['name'] for x in right])
        self.assertEqual(len(set(x['name'] for x in left)), len(left))
        self.assertEqual(len(oracle.QUERIES), 47)
        self.assertEqual(len(oracle.STATES), 5)
        self.assertIn('generate_series(1,100)', oracle.BOUNDARY_FIXTURE)
        self.assertTrue(all('stannum.' in c['sql'] for c in left))
        self.assertTrue(all('tin.' in c['sql'] for c in right))

    def test_top_level_control_preserves_scored_columns(self):
        case = next(c for c in oracle.boundary_cases('tin') if c['name'] == 'term_add_top_level')
        payload = '1|' + json.dumps('\\x3f800000') + '|' + json.dumps('\\x40000000') + '\n'
        with patch.object(oracle, 'psql', return_value=CompletedProcess([], 0, payload)):
            self.assertEqual(oracle.boundary_observe(case, {})['rows'],
                             [[1, '\\x3f800000', '\\x40000000']])


class LifecycleOracleTests(unittest.TestCase):
    def test_lifecycle_checks_keep_duplicates_and_exact_highlights(self):
        case = next(c for c in oracle.lifecycle_cases('tin') if c['name'] == 'prepared_custom')
        expected = case['expected']
        self.assertEqual(oracle.lifecycle_check(case, {'rows': list(reversed(expected))})[1], [])
        self.assertTrue(oracle.lifecycle_check(case, {'rows': expected[:-1]})[1])
        changed = [list(row) for row in expected]
        changed[0][-2] = 'alpha red'
        self.assertTrue(oracle.lifecycle_check(case, {'rows': changed})[1])
        deduplicated = [list(row) for row in dict.fromkeys(tuple(row) for row in expected)]
        self.assertTrue(oracle.lifecycle_check(case, {'rows': deduplicated})[1])

    def test_lifecycle_expected_empty_and_changed_parameters_are_executed(self):
        cases = {case['name']: case for case in oracle.lifecycle_cases('stannum')}
        for mode in ['custom', 'generic']:
            case = cases['prepared_' + mode]
            self.assertIn(f'plan_cache_mode=force_{mode}_plan', case['sql'])
            for query in ['alpha', 'beta', '', 'absenttoken', 'eclair']:
                self.assertIn(f"EXECUTE oracle_lifecycle('{query}')", case['sql'])
            self.assertEqual(case['sql'].count('EXECUTE oracle_lifecycle'), 6)
            self.assertEqual(case['expected'][-1], ['plans', 6 if mode == 'generic' else 0,
                                                   6 if mode == 'custom' else 0])
        self.assertEqual(len(cases), 14)
        self.assertTrue(all('stannum.highlight' in c['sql'] for c in cases.values()))
        self.assertTrue(all('BEGIN;' in cases[name]['sql'] and cases[name]['sql'].endswith('ROLLBACK')
                            for name in ['update_returning', 'update_cte']))

    def test_lifecycle_errors_and_equal_empty_outputs_never_silently_pass(self):
        case = oracle.lifecycle_cases('tin')[0]
        self.assertEqual(oracle.lifecycle_check(case, {'error': 'unsupported', 'sqlstate': 'XX000'})[1],
                         ['unexpected_error'])
        self.assertTrue(oracle.lifecycle_check(case, {'rows': []})[1])

    def test_failed_implicit_shapes_have_independent_explicit_controls(self):
        cases = {case['name']: case for case in oracle.lifecycle_cases('tin')}
        for name in ['cte_inline', 'cte_materialized', 'subquery_binding', 'update_cte']:
            implicit, explicit = cases[name], cases[name + '_explicit']
            self.assertIn('tin.highlight(body)', implicit['sql'])
            self.assertNotIn('tin.highlight(body)', explicit['sql'])
            self.assertEqual(implicit['expected'], explicit['expected'])


class PublishedTraceTests(unittest.TestCase):
    def test_published_prefix_preserves_raw_text_and_string_ids(self):
        import csv
        import tempfile
        from pathlib import Path
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rows = [['site/001', 'C++ & café\nMixed CASE'], ['002', ''], ['unused', 'later']]
            with (root / 'data.csv').open('w', newline='') as stream:
                writer = csv.writer(stream)
                writer.writerow(['id', 'body'])
                writer.writerows(rows)
            oracle.copy_trace_prefix(root, root / 'input.csv', 2, True)
            with (root / 'input.csv').open(newline='') as stream:
                self.assertEqual(list(csv.reader(stream)), rows[:2])
            with self.assertRaises(ValueError):
                oracle.copy_trace_prefix(root, root / 'short.csv', 4, True)

    def test_string_ids_keep_identity_in_membership_and_ranking(self):
        rows = [['site/001', '\\x3f800000'], ['001', '\\x3f800000']]
        self.assertEqual(oracle.compare_trace(rows, rows, rows), [])
        self.assertIn('membership', oracle.compare_trace(rows, rows[:1], rows))
        for invalid in [[[True, '\\x3f800000']], [[None, '\\x3f800000']], rows + rows[:1]]:
            with self.assertRaises(ValueError):
                oracle.checked_scores(invalid)

    def test_trace_identity_uses_pinned_git_objects_and_rejects_other_trace(self):
        import tempfile
        from pathlib import Path
        import published_dataset
        with tempfile.TemporaryDirectory() as directory:
            trace = Path(directory) / 'queries.json'
            trace.write_bytes(b'{"queries": []}')
            with patch.object(oracle.subprocess, 'check_output', side_effect=[trace.read_bytes(), b'{"csv": {}}']) as call:
                self.assertEqual(oracle.published_trace_identity(directory, 'stackexchange', trace), {'csv': {}})
                self.assertEqual(call.call_args_list[0].args[0][-1], published_dataset.REVISION + ':datasets/stackexchange/queries.json')
            with patch.object(oracle.subprocess, 'check_output', return_value=b'different'):
                with self.assertRaises(ValueError):
                    oracle.published_trace_identity(directory, 'stackexchange', trace)
