# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest

import notify


class NotificationTests(unittest.TestCase):
    def test_pending_trials_do_not_trigger_completion(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'wikipedia-100000').mkdir()
            p = root / 'wikipedia-100000/campaign.json'
            p.write_text(json.dumps({'status': 'running', 'jobs': [{'status': 'complete'}]}))
            self.assertEqual(notify.milestones(root), {})
            p.write_text(json.dumps({'status': 'incomplete', 'jobs': [
                {'status': 'complete'}, {'status': 'failed'}]}))
            result = notify.milestones(root)
            self.assertIn('1/2 trials passed', result['100k'][1])
            self.assertIn('incomplete', result['100k'][1])
            self.assertNotIn('series', result)
            (root / 'status.json').write_text(json.dumps({'status': 'incomplete'}))
            self.assertIn('series', notify.milestones(root))


if __name__ == '__main__':
    unittest.main()
