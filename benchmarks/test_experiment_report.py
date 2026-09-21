# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest
import experiment_report


class ReportTests(unittest.TestCase):
    def test_incomplete_run_withholds_metrics(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'manifest.json').write_text(json.dumps(dict(status='running')))
            self.assertEqual(experiment_report.summarize(root)['rows'],[])

    def test_complete_marker_does_not_override_missing_trials(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);(root/'manifest.json').write_text(json.dumps(dict(status='complete',rounds=[])))
            (root/'results.json').write_text('[]')
            with self.assertRaisesRegex(ValueError,'Incomplete round'):experiment_report.summarize(root)
