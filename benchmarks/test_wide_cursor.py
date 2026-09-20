# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import importlib.util
from pathlib import Path
import random
import unittest

spec = importlib.util.spec_from_file_location('ranked_fuzz', Path(__file__).resolve().parents[1] / 'postgres/tests/ranked_fuzz.py')
fuzz = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fuzz)


class WideCursorTests(unittest.TestCase):
    def test_queries_have_distinct_nonzero_scoring_terms(self):
        for width in (31, 32, 33, 128):
            query = fuzz.Query.generate(random.Random(7), width=width)
            terms = [term.split('^')[0] for term in query.tinql.split(' OR ')]
            self.assertEqual(len(set(terms)), width)
            self.assertEqual(query.predicate.count('body ~'), width)
            self.assertTrue(all(term.isalpha() for term in terms))
            self.assertNotIn('==>', query.predicate)

    def test_wide_corpus_contains_entire_query_vocabulary(self):
        corpus = fuzz.Corpus(random.Random(7), wide=True)
        bodies = [set(corpus.body().split()) for _ in range(100)]
        self.assertTrue(any(set(fuzz.WIDE_WORDS) <= body for body in bodies))
        self.assertTrue(any(len(set(fuzz.WIDE_WORDS) & body) == 32 for body in bodies))
        ordinary = fuzz.Corpus(random.Random(7))
        self.assertFalse(set(ordinary.body().split()) & set(fuzz.WIDE_WORDS))
