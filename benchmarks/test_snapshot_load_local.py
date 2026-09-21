# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import unittest
from snapshot_load_local import summarize


class WindowTests(unittest.TestCase):
    def test_late_requests_remain_in_latency_not_fixed_window_throughput(self):
        rows=[dict(id=1,end=1.5,ms=10,error=None),dict(id=2,end=2.5,ms=1000,error=None)]
        result=summarize(rows,1,2,2.5)
        self.assertEqual(result['qps'],1)
        self.assertEqual(result['p95_ms'],1000)
        self.assertEqual(result['drain_seconds'],.5)
        self.assertEqual(result['distinct_queries'],2)

    def test_errors_cannot_be_counted_as_successes(self):
        result=summarize([dict(id=1,end=1.5,ms=10,error='wrong result')],1,2,2)
        self.assertEqual(result['errors'],1)
        self.assertEqual(result['qps'],0)
        self.assertIsNone(result['p95_ms'])
