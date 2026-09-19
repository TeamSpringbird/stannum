"""Failure cleanup must not delete a postmaster that outlived pg_ctl startup."""
import contextlib
import io
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import paired_libraries


class StartupCleanupTests(unittest.TestCase):
    def check_startup_timeout(self, stop_fails):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cluster = root / 'cluster'
            cluster.mkdir()
            library = root / 'installed-library'
            library.write_bytes(b'original')
            for name in ('baseline', 'integrated'):
                (root / name).write_bytes(name.encode())
            stops = []

            def run(command, **kwargs):
                if command[0] == 'initdb':
                    (cluster / 'data').mkdir()
                elif command[0] == 'pg_ctl' and command[-1] == 'start':
                    (cluster / 'data/postmaster.pid').write_text('fake-test-pid')
                    raise subprocess.CalledProcessError(1, command)
                elif command[0] == 'pg_ctl' and command[-1] == 'stop':
                    stops.append(command)
                    if stop_fails:
                        raise subprocess.CalledProcessError(1, command)
                    (cluster / 'data/postmaster.pid').unlink()
                return subprocess.CompletedProcess(command, 0)

            argv = ['paired_libraries.py', '--baseline', str(root / 'baseline'),
                    '--integrated', str(root / 'integrated'), '--installed-library',
                    str(library), '--output', str(root / 'output')]
            with (patch('sys.argv', argv),
                  patch.object(paired_libraries.tempfile, 'mkdtemp', return_value=str(cluster)),
                  patch.object(paired_libraries.subprocess, 'run', side_effect=run),
                  contextlib.redirect_stdout(io.StringIO())):
                with self.assertRaises(subprocess.CalledProcessError):
                    paired_libraries.main()
            self.assertEqual(len(stops), 1)
            self.assertEqual(cluster.exists(), stop_fails)
            self.assertEqual(library.read_bytes(), b'original')

    def test_startup_timeout_stops_postmaster_before_removing_cluster(self):
        self.check_startup_timeout(False)

    def test_failed_stop_preserves_cluster(self):
        self.check_startup_timeout(True)
