# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import copy
import unittest

from merge_costs import schedule, summarize, window_metrics


def result(budget=0):
    return {
        'status': 'passed', 'artifact_sha256': 'same-build', 'server': {'version': 'test'},
        'config': {'docs': 2, 'repeat': 20, 'write_buffer_docs': 1, 'max_merge_docs': budget},
        'correctness': dict(differences=0, heap_docs=2, index_docs=2, verify_errors=0),
        'samples': [
            dict(id=1, kind='buffered', execution_ms=1, wal_bytes=20,
                 shared_read_blocks=0, segments_after=0, retired_existing_docs=0),
            dict(id=2, kind='fold_and_merge', execution_ms=9, wal_bytes=180,
                 shared_read_blocks=2, segments_after=1, retired_existing_docs=32),
        ],
    }


class MergeCampaignTests(unittest.TestCase):
    def test_rotation_balances_each_budget_position(self):
        order = schedule([0, 256, 1024], 3)
        for position in range(3):
            self.assertEqual({order[r * 3 + position][1] for r in range(3)}, {0, 256, 1024})
        for budgets, rounds in [([], 1), ([0, 0], 1), ([0], 0)]:
            with self.assertRaises(ValueError):
                schedule(budgets, rounds)

    def test_costs_do_not_invent_cpu_phase_times_or_missing_folds(self):
        metrics = window_metrics(result())
        self.assertEqual(metrics['execution_ms_total'], 10)
        self.assertEqual(metrics['wal_bytes_total'], 200)
        self.assertEqual(metrics['merge_insert_ids'], [2])
        self.assertEqual(metrics['groups']['fold']['samples'], 0)
        self.assertIsNone(metrics['groups']['fold']['p50_ms'])
        self.assertEqual(metrics['retired_existing_docs_total'], 32)

    def test_failed_or_partial_measurements_are_rejected(self):
        for mutate in [lambda r: r.update(status='failed'),
                       lambda r: r['samples'].pop(),
                       lambda r: r['samples'][1].update(id=1),
                       lambda r: r['correctness'].update(differences=1)]:
            data = result()
            mutate(data)
            with self.assertRaises(ValueError):
                window_metrics(data)

    def test_campaign_medians_use_windows_without_pooling_samples(self):
        first, second = result(), result()
        second['samples'][1]['execution_ms'] = 29
        summary = summarize([{'round': 1, 'result': first}, {'round': 2, 'result': second}])
        self.assertEqual(summary['median_window_totals'][0]['execution_ms_total'], 20)
        self.assertEqual(summary['median_window_totals'][0]['windows'], 2)

    def test_build_fixture_server_and_round_mismatch_fail_closed(self):
        windows = [{'round': 1, 'result': result(0)}, {'round': 1, 'result': result(256)}]
        summarize(windows)
        for mutate in [lambda r: r.update(artifact_sha256='different'),
                       lambda r: r['config'].update(repeat=200),
                       lambda r: r['server'].update(version='different')]:
            bad = copy.deepcopy(windows)
            mutate(bad[1]['result'])
            with self.assertRaises(ValueError):
                summarize(bad)
        with self.assertRaises(ValueError):
            summarize(windows + [windows[0]])
        with self.assertRaises(ValueError):
            summarize(windows + [{'round': 2, 'result': result(0)}])


if __name__ == '__main__':
    unittest.main()
