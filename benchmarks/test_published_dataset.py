# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

import gzip
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import published_dataset as data


def identity(blob):
    return dict(bytes=len(blob), sha256=hashlib.sha256(blob).hexdigest())


class PublishedDatasetTests(unittest.TestCase):
    def test_split_gzip_is_one_stream_and_preserves_exact_csv(self):
        csv = b'id,body\nhttps://example.test/a,"alpha beta"\n'
        packed = gzip.compress(csv)
        chunks = [packed[:len(packed)//2], packed[len(packed)//2:]]
        parts = [dict(file=f'data.csv.gz.part{i:04}', **identity(b)) for i,b in enumerate(chunks)]
        manifest = dict(csv=dict(file='data.csv', **identity(csv)),
                        gzip=dict(parts=parts, **identity(packed)))
        metadata = {'data-manifest.json':manifest, 'source.json':{}, 'queries.json':[]}
        def fetch(url, timeout):
            return io.BytesIO(json.dumps(metadata[url.rsplit('/',1)[-1]]).encode())
        def download(command, check):
            target = Path(command[command.index('--output')+1])
            name = command[-1].rsplit('/',1)[-1]
            target.write_bytes(chunks[[p['file'] for p in parts].index(name)])
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with patch.object(data.urllib.request,'urlopen',side_effect=fetch), patch.object(data.subprocess,'run',side_effect=download) as curl:
                data.acquire('wikipedia',root)
                self.assertEqual((root/'data.csv').read_bytes(),csv)
                self.assertEqual(curl.call_count,2)
                data.acquire('wikipedia',root)
                self.assertEqual(curl.call_count,2)
            self.assertFalse((root/'joined.csv.gz.partial').exists())
            self.assertEqual(json.loads((root/'verification.json').read_text())['status'],'verified')

    def test_same_size_corruption_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/'part'
            path.write_bytes(b'bad')
            self.assertFalse(data.verify(path,identity(b'yes')))
            self.assertFalse(data.verify(path,identity(b'longer')))
            self.assertTrue(data.verify(path,identity(b'bad')))
