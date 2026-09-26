# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import unittest
import gzip
import json
from pathlib import Path
import tempfile
from unittest.mock import patch
import query_comparison as comparison


class QueryComparisonTests(unittest.TestCase):
    def test_weights_use_baseline_mix_not_faster_candidate_sample_frequency(self):
        rows = comparison.compare({'slow':[100,100], 'fast':[1,1]}, {'slow':[50], 'fast':[2]*20})
        self.assertEqual([r['query_id'] for r in rows], ['fast','slow'])
        self.assertEqual(rows[0]['baseline_mix_delta_ms'], .5)
        self.assertEqual(rows[1]['baseline_mix_delta_ms'], -25)
        self.assertEqual(rows[1]['mean_change_percent'], -50)
        self.assertAlmostEqual(sum(r['baseline']['query_time_share'] for r in rows), 1)

    def test_missing_query_coverage_is_not_silently_dropped(self):
        with self.assertRaises(ValueError):
            comparison.compare({'a':[1], 'b':[2]}, {'a':[1]})

    def test_identical_runs_have_no_change(self):
        rows = comparison.compare({'a':[1,2,100]}, {'a':[1,2,100]})
        self.assertEqual(rows[0]['mean_change_percent'], 0)
        self.assertEqual(rows[0]['baseline']['p95_ms'], 100)

    def test_invalid_samples_rejected(self):
        for samples in ([], [float('nan')], [-1]):
            with self.assertRaises(ValueError):
                comparison.compare({'a':[1]}, {'a':samples})

    def test_reader_rejects_truncated_export_and_query_errors(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root/'stannum').mkdir()
            (root/'manifest.json').write_text('{}')
            export = root/'stannum/result_test.json'
            export.write_text(json.dumps({'runs': {'stannum': {'startTime':1000, 'endTime':1268}}}))
            point = dict(type='Point', metric='query_duration', data=dict(time='1970-01-01T00:10:00.950999Z', value=10, tags={'backend':'stannum','query_id':'1:disjunction'}))
            def samples(points):
                with gzip.open(root/'stannum/samples.json.gz','wt') as out:
                    for p in points: out.write(json.dumps(p)+'\n')
            samples([point])
            with patch('query_comparison.tin.comparison_contract', return_value={}):
                with self.assertRaisesRegex(ValueError, 'outside'):
                    comparison.read(root)
                export.write_text(json.dumps({'runs': {'stannum': {'startTime':1000, 'endTime':600950}}}))
                _, groups = comparison.read(root)
                self.assertEqual(groups, {'1:disjunction':[10]})
                samples([point, dict(type='Point', metric='benchmark_query_errors', data={'value':1})])
                with self.assertRaisesRegex(ValueError, 'query errors'):
                    comparison.read(root)
