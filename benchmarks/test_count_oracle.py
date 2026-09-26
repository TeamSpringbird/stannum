# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import unittest
import count_oracle


class OraclePlanTests(unittest.TestCase):
    def test_requires_real_expected_index(self):
        count_oracle.require_index({'Node Type':'Bitmap Heap Scan','Plans':[{'Node Type':'Bitmap Index Scan','Index Name':'documents_idx'}]},'documents_idx')
        for plan in [{'Node Type':'Seq Scan'},{'Node Type':'Tid Scan'}, {'Node Type':'Bitmap Heap Scan','Plans':[{'Node Type':'Bitmap Index Scan','Index Name':'other'}]}]:
            with self.assertRaises(ValueError):count_oracle.require_index(plan,'documents_idx')
