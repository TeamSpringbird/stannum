#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Download and checksum the exact prepared PlanetScale benchmark corpus."""
import argparse
import csv
import gzip
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import urllib.request

REVISION = 'f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86'
BASE = 'https://raw.githubusercontent.com/planetscale/paradedb-benchmarker/' + REVISION
MEDIA = 'https://media.githubusercontent.com/media/planetscale/paradedb-benchmarker/' + REVISION


def verify(path, expected):
    if not path.exists() or path.stat().st_size != expected['bytes']:
        return False
    h = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(8 * 1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest() == expected['sha256']


def acquire(corpus, root):
    root.mkdir(parents=True, exist_ok=True)
    for name in ('data-manifest.json', 'source.json', 'queries.json'):
        with urllib.request.urlopen(f'{BASE}/datasets/{corpus}/{name}', timeout=60) as response:
            data = response.read()
        json.loads(data)
        (root / name).write_bytes(data)
    manifest = json.loads((root / 'data-manifest.json').read_text())
    csv = root / manifest['csv']['file']
    if verify(csv, manifest['csv']):
        print(f'{corpus}: CSV already verified', flush=True)
        return
    for part in manifest['gzip']['parts']:
        target = root / part['file']
        if verify(target, part):
            continue
        pending = root / (part['file'] + '.partial')
        subprocess.run(['curl', '--fail', '--location', '--retry', '5', '--continue-at', '-',
                        '--output', str(pending), f'{MEDIA}/datasets/{corpus}/{part["file"]}'], check=True)
        if not verify(pending, part):
            raise ValueError(f'Checksum/size mismatch: {pending}; remove it before retrying')
        pending.replace(target)
        print(f'{corpus}: verified {part["file"]}', flush=True)
    joined = root / 'joined.csv.gz.partial'
    try:
        with joined.open('wb') as target:
            for part in manifest['gzip']['parts']:
                with (root / part['file']).open('rb') as source:
                    shutil.copyfileobj(source, target, 8 * 1024 * 1024)
        if not verify(joined, manifest['gzip']):
            raise ValueError('Combined gzip checksum mismatch')
        pending_csv = root / 'data.csv.partial'
        with gzip.open(joined, 'rb') as source, pending_csv.open('wb') as target:
            shutil.copyfileobj(source, target, 8 * 1024 * 1024)
        if not verify(pending_csv, manifest['csv']):
            raise ValueError('Decompressed CSV checksum mismatch')
        pending_csv.replace(csv)
        (root / 'verification.json').write_text(json.dumps(dict(
            revision=REVISION, corpus=corpus, csv=manifest['csv'], status='verified'), indent=2)+'\n')
        print(f'{corpus}: verified full CSV ({csv.stat().st_size} bytes)', flush=True)
    finally:
        joined.unlink(missing_ok=True)


def inspect(root, corpus, expected_manifest):
    """Verify CSV identity before loading; never trust a stale receipt alone."""
    root = Path(root)
    manifest = json.loads((root / 'data-manifest.json').read_text())
    if manifest != expected_manifest:
        raise ValueError('dataset manifest differs from pinned upstream driver')
    receipt = json.loads((root / 'verification.json').read_text())
    if receipt.get('revision') != REVISION or receipt.get('corpus') != corpus or receipt.get('csv') != manifest['csv']:
        raise ValueError('published dataset identity mismatch')
    if not verify(root / 'data.csv', manifest['csv']):
        raise ValueError('published CSV checksum mismatch')
    return dict(format='planetscale-prepared-v1', revision=REVISION, corpus=corpus,
                rows={'wikipedia':5032104, 'stackexchange':150000000}[corpus], csv=manifest['csv'])


def prefix(root, target, rows):
    """Preserve field contents, order and empty strings in a bounded COPY file."""
    if rows < 1:
        raise ValueError('rows must be positive')
    # Some published documents exceed Python's default 128 KiB field limit.
    csv.field_size_limit(1024 * 1024 * 1024)
    with (Path(root) / 'data.csv').open(encoding='utf-8', newline='') as source, Path(target).open('w', encoding='utf-8', newline='') as output:
        reader = csv.reader(source)
        if next(reader, None) != ['id', 'body']:
            raise ValueError('expected id,body CSV header')
        writer = csv.writer(output, quoting=csv.QUOTE_ALL)
        for _ in range(rows):
            row = next(reader, None)
            if row is None or len(row) != 2:
                raise ValueError('incomplete or malformed published CSV')
            writer.writerow(row)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--corpus', choices=['wikipedia', 'stackexchange'], required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    acquire(args.corpus, args.output)


if __name__ == '__main__':
    main()
