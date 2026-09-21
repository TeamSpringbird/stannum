# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import unittest
from copy import deepcopy
import count_crossover as probe
import count_crossover_report as report


class CrossoverTests(unittest.TestCase):
    def row(self, part, density, width, default, forced):
        return dict(id=1,text='x',partition=part,features=dict(postings_per_heap_page=density,unique_terms=width),median_ms=dict(off=default,on=forced))

    def test_known_examples_never_leak_into_holdout(self):
        self.assertTrue(all(probe.partition({'source_id':n})=='train' for n in probe.KNOWN))
        self.assertEqual(probe.partition({'source_id':19}),probe.partition({'source_id':19}))

    def test_heldout_timings_cannot_change_fitted_rule(self):
        rows=[self.row('train',2,3,10,5),self.row('heldout',2,3,10,10000)]
        phases={'vacuumed':rows,'mutated':deepcopy(rows)}
        rule=report.fit(phases)
        self.assertIsNotNone(rule)
        for rows in phases.values(): rows[1]['median_ms']['on']=.001
        self.assertEqual(rule,report.fit(phases))

    def test_training_regressions_preserve_default(self):
        rows=[self.row('train',2,3,10,15)]
        self.assertIsNone(report.fit({'vacuumed':rows,'mutated':rows}))

    def test_material_guard_requires_absolute_and_relative_cost(self):
        self.assertFalse(report.material_regression(.01,.04))
        self.assertFalse(report.material_regression(10,10.5))
        self.assertTrue(report.material_regression(10,12))

    def test_existing_bitmap_selection_gets_no_simulated_gain(self):
        row=self.row('heldout',8,3,10,5)
        row['runs']=[dict(mode='off',strategy='page bitmaps')]
        result=report.evaluate([row],dict(density=4,width=1))
        self.assertEqual(result['forced_queries'],0)
        self.assertEqual(result['simulated_ratio'],1)
