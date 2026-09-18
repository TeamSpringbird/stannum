import csv
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import dataset
import run


class DatasetTests(unittest.TestCase):
    def test_normalization_and_exact_phrase_boundaries(self):
        body = dataset.normalize('Computer Science', 'HISTORY, computer-sciences; quantum mechanics!')
        self.assertEqual(body, 'computer science history computer sciences quantum mechanics')
        by_name = {c[0]: c for c in dataset.CASES}
        self.assertTrue(dataset.matches(body, by_name['phrase_medium']))
        self.assertTrue(dataset.matches(body, by_name['phrase_rare']))
        self.assertFalse(dataset.matches('computer sciences', by_name['phrase_medium']))
        self.assertFalse(dataset.matches('prehistory', by_name['common']))
        self.assertFalse(dataset.matches(body, by_name['miss']))
        self.assertLessEqual(len(dataset.normalize('', 'word ' * 20000).split()), 8192)

    def test_checksum_and_cardinality_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            data = root / 'documents.csv'
            data.write_text('1,history\n')
            (root / 'manifest.json').write_text(json.dumps({'status': 'complete', 'rows': 1,
                'files': {'documents.csv': dataset.sha256(data)}}))
            self.assertEqual(dataset.verify(root, 1)['rows'], 1)
            with self.assertRaises(ValueError):
                dataset.verify(root, 2)
            data.write_text('1,changed\n')
            with self.assertRaises(ValueError):
                dataset.verify(root, 1)

    def test_external_oracle_rejects_incorrect_match_sets(self):
        corpus = {'cases': [dataset.CASES[1]], 'match_counts': {'common': 12}}
        with patch.object(run, 'sql_json', return_value={'count': 12, 'differences': 2}):
            with self.assertRaises(ValueError):
                run.validate('lead', 1000, {}, corpus)

    def test_external_ranked_oracle_checks_membership(self):
        corpus = {'cases': [dataset.CASES[1]], 'match_counts': {'common': 1}}
        with patch.object(run, 'sql_json', side_effect=[[{'id': 3, 'score': 1.0}], [3]]):
            run.validate_ranked('lead', 1000, {}, corpus)
        with patch.object(run, 'sql_json', side_effect=[[{'id': 3, 'score': 1.0}], []]):
            with self.assertRaises(ValueError):
                run.validate_ranked('lead', 1000, {}, corpus)


if __name__ == '__main__':
    unittest.main()
