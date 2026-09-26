# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HELPER = Path(__file__).resolve().parent / 'pgrx-lock.py'


class PgrxLockTests(unittest.TestCase):
    def run_helper(self, lock, *command):
        env = dict(os.environ, STANNUM_PGRX_LOCK=str(lock))
        env.pop('STANNUM_PGRX_LOCK_HELD', None)
        return subprocess.run([sys.executable, str(HELPER), '--', *command],
                              env=env, capture_output=True, text=True, timeout=30)

    def test_runs_the_command_and_returns_its_exit_status(self):
        with tempfile.TemporaryDirectory() as directory:
            lock = Path(directory) / 'lock'
            result = self.run_helper(lock, sys.executable, '-c',
                                     'import os, sys; print(os.environ["STANNUM_PGRX_LOCK_HELD"]); sys.exit(3)')
            self.assertEqual(result.returncode, 3)
            self.assertEqual(result.stdout.strip(), str(lock))
            self.assertTrue(lock.exists())

    def test_a_nested_call_for_the_same_lock_does_not_wait_for_itself(self):
        with tempfile.TemporaryDirectory() as directory:
            lock = Path(directory) / 'lock'
            inner = f'import subprocess, sys; sys.exit(subprocess.call([sys.executable, {str(HELPER)!r}, "--", "true"]))'
            result = self.run_helper(lock, sys.executable, '-c', inner)
            self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
