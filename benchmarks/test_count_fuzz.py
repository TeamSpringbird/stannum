# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import unittest
import count_fuzz as fuzz


class CountFuzzTests(unittest.TestCase):
    def test_seed_reproduces_fixture_queries_and_mutations(self):
        a = fuzz.fixture(1,512,20,3)
        self.assertEqual(a, fuzz.fixture(1,512,20,3))
        self.assertNotEqual(a, fuzz.fixture(2,512,20,3))
        self.assertEqual([b['rollback'] for b in a['mutations']], [False,False,True])

    def test_independent_nested_boolean_truth_table(self):
        ast = ('AND', ('OR', 'alpha','beta'), ('OR', 'rare','alpha'))
        for tokens, expected in [(set(),False), ({'alpha'},True), ({'beta'},False), ({'beta','rare'},True), ({'rare'},False)]:
            self.assertEqual(fuzz.matches(ast,tokens),expected)
        self.assertEqual(fuzz.render(ast),'((alpha OR beta) AND (rare OR alpha))')
