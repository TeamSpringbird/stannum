# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from aws import database_snapshot as snapshot


class SnapshotTests(unittest.TestCase):
    def test_corruption_and_unverified_manifest_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            folder=Path(tmp)
            manifest=dict(format=1,state='restore-verified',files={})
            for name in ('database.tar.gz','image.tar.gz'):
                path=folder/name;path.write_bytes(b'original')
                manifest['files'][name]=dict(bytes=path.stat().st_size,sha256=snapshot.digest(path))
            snapshot.verify_files(folder,manifest)
            (folder/'database.tar.gz').write_bytes(b'changed!')
            with self.assertRaisesRegex(ValueError,'checksum'):snapshot.verify_files(folder,manifest)
            manifest['state']='pending'
            with self.assertRaisesRegex(ValueError,'verified'):snapshot.verify_files(folder,manifest)

    def test_never_replaces_existing_volume(self):
        with patch.object(snapshot.subprocess,'run') as command, patch.object(snapshot,'run') as mutation:
            command.return_value.returncode=0
            with self.assertRaisesRegex(ValueError,'new volume'):snapshot.empty_volume('existing')
            mutation.assert_not_called()

    def test_manifest_cannot_add_arbitrary_paths(self):
        with self.assertRaisesRegex(ValueError,'Unexpected'):
            snapshot.verify_files(Path('/unused'),dict(format=1,state='restore-verified',files={'../outside':{}}))
