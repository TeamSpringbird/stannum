# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""The crash driver must not signal a mismatched or privileged process."""
import unittest

from spill_crash_recovery import checked_pid, literal


class CrashTargetTests(unittest.TestCase):
    def test_rejects_unverified_targets(self):
        for marker, active in [(123, ''), (123, '456'), (123, '123\n456'), (1, '1'), (0, '0')]:
            with self.subTest(marker=marker, active=active):
                with self.assertRaises(RuntimeError):
                    checked_pid(marker, active)

    def test_accepts_exact_scratch_backend(self):
        self.assertEqual(checked_pid(123, '123\n'), 123)

    def test_sql_literal_quotes_paths_and_names(self):
        self.assertEqual(literal("/tmp/a'b"), "'/tmp/a''b'")


if __name__ == '__main__':
    unittest.main()
