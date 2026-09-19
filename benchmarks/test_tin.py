# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest

import tin


class TraceCorrectnessTests(unittest.TestCase):
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

    def test_sql_keeps_query_text_inside_literal(self):
        text = "'quoted' ; DROP TABLE documents; --"
        self.assertEqual(tin.literal(text), "'''quoted'' ; DROP TABLE documents; --'")


if __name__ == '__main__':
    unittest.main()
