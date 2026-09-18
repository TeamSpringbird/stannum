#!/usr/bin/env python3
"""Build immutable, nested Wikipedia search corpora; pyarrow needed only to build."""
import argparse
import concurrent.futures
import csv
import hashlib
import json
from pathlib import Path
import random
import re
import subprocess
import urllib.request

REVISION = 'b04c8d1ceb2f5cd4588862100d08de323dccfbaa'
REPO = 'https://huggingface.co/datasets/wikimedia/wikipedia'
# The absent-term sentinel is frozen corpus data from the original Lead baseline.
# Keep it stable across product renames so checksums and query identities stay valid.
CASES = [
    ('miss', 'zzleadmissingtoken', 'zzleadmissingtoken', 'zzleadmissingtoken', 'term', 0),
    ('common', 'history', 'history', 'history', 'term', 0),
    ('medium', 'telescope', 'telescope', 'telescope', 'term', 0),
    ('rare', 'quasar', 'quasar', 'quasar', 'term', 0),
    ('and', 'war AND history', 'war & history', 'war history', 'and', 0),
    ('or', 'telescope OR astronomy', 'telescope | astronomy', 'telescope astronomy', 'or', 0),
    ('phrase_common', '"united states"', 'united <-> states', 'united states', 'phrase', 0),
    ('phrase_medium', '"computer science"', 'computer <-> science', 'computer science', 'phrase', 0),
    ('phrase_rare', '"quantum mechanics"', 'quantum <-> mechanics', 'quantum mechanics', 'phrase', 0),
    ('phrase_miss', '"zzleadmissingtoken telescope"', 'zzleadmissingtoken <-> telescope', 'zzleadmissingtoken telescope', 'phrase', 0),
]


def sha256(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as f:
        for block in iter(lambda: f.read(8 * 1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def normalize(title, text):
    # Equal lexical input for all engines; positions stay below PostgreSQL's limit.
    return ' '.join(re.findall('[a-z]+', (title + ' ' + text).lower())[:8192])


def matches(body, case):
    terms = case[3].split()
    words = set(body.split())
    if case[4] == 'phrase':
        return ' ' + case[3] + ' ' in ' ' + body + ' '
    return any(t in words for t in terms) if case[4] == 'or' else all(t in words for t in terms)


def verify(root, rows=None):
    root = Path(root)
    m = json.loads((root / 'manifest.json').read_text())
    if m['status'] != 'complete' or (rows is not None and m['rows'] != rows):
        raise ValueError('Dataset incomplete or row count differs')
    for name, expected in m['files'].items():
        if sha256(root / name) != expected:
            raise ValueError('Dataset checksum mismatch: ' + name)
    return m


def download(item, cache):
    path = cache / Path(item['path']).name
    expected = item['lfs']['oid']
    if path.exists() and sha256(path) == expected:
        return path
    part = path.with_suffix('.partial')
    url = f"{REPO}/resolve/{REVISION}/{item['path']}?download=true"
    subprocess.run(['curl', '-fL', '--retry', '8', '--retry-delay', '3', '--continue-at', '-',
                    '--output', str(part), url], check=True, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL)
    if sha256(part) != expected:
        raise ValueError('Source checksum mismatch: ' + str(part))
    part.rename(path)
    print('Downloaded and verified ' + path.name, flush=True)
    return path


def build(args):
    import pyarrow.parquet as pq
    root = Path(args.output).resolve()
    root.mkdir(parents=True, exist_ok=True)
    targets = [root / 'wikipedia-100000', root / 'wikipedia-1000000']
    if all((p / 'manifest.json').exists() for p in targets):
        for p in targets:
            verify(p)
        print('Both datasets already complete and verified', flush=True)
        return
    cache = root / 'source'
    cache.mkdir(exist_ok=True)
    url = f'https://huggingface.co/api/datasets/wikimedia/wikipedia/tree/{REVISION}/20231101.en?limit=100'
    items = json.load(urllib.request.urlopen(url))
    items = sorted((i for i in items if i['path'].endswith('.parquet')), key=lambda i: i['path'])
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        paths = list(pool.map(lambda i: download(i, cache), items))
    total = sum(pq.ParquetFile(p).metadata.num_rows for p in paths)
    # Sample across ALL source shards. Rank defines nested membership, not source order.
    selected = {row: rank + 1 for rank, row in enumerate(random.Random(1729).sample(range(total), 1000000))}
    outputs = []
    for size, target in zip((100000, 1000000), targets):
        if target.exists():
            raise ValueError(f'Refusing partial/existing dataset: {target}; inspect it before retrying')
        target.mkdir()
        handles = [(target / name).open('w', newline='', encoding='utf-8') for name in ('documents.csv', 'attribution.csv', 'matches.csv')]
        outputs.append((size, target, handles, [csv.writer(h, lineterminator='\n') for h in handles], [], {c[0]: 0 for c in CASES}))
    row = 0
    try:
        for path in paths:
            for batch in pq.ParquetFile(path).iter_batches(batch_size=1024):
                for doc in batch.to_pylist():
                    identifier = selected.get(row)
                    row += 1
                    if identifier is None:
                        continue
                    body = normalize(doc['title'], doc['text'])
                    if not body or any(t in body.split() for t in ('mutablea', 'mutableb', 'zzleadmissingtoken')):
                        raise ValueError('Empty document or reserved token collision')
                    hits = [case[0] for case in CASES if matches(body, case)]
                    for size, target, handles, writers, lengths, counts in outputs:
                        if identifier > size:
                            continue
                        writers[0].writerow((identifier, body + ' mutablea'))
                        writers[1].writerow((identifier, doc['id'], doc['url'], doc['title']))
                        lengths.append(len(body.encode()))
                        for hit in hits:
                            writers[2].writerow((hit, identifier))
                            counts[hit] += 1
            print(f'Processed {path.name}: {row}/{total} source documents', flush=True)
    finally:
        for _, _, handles, _, _, _ in outputs:
            for h in handles:
                h.close()
    for size, target, _, _, lengths, counts in outputs:
        if len(lengths) != size:
            raise ValueError('Incorrect output cardinality')
        lengths.sort()
        m = {'status': 'complete', 'schema_version': 1, 'rows': size, 'source': REPO,
             'revision': REVISION, 'subset': '20231101.en', 'source_rows': total,
             'source_files': {i['path']: i['lfs']['oid'] for i in items},
             'license': 'CC BY-SA 3.0 / GFDL; attribution.csv retains article IDs, URLs and titles',
             'sampling': 'Python random.Random(1729).sample across all source rows; rank <= size',
             'normalization': 'title + text; lowercase ASCII letter runs; first 8192 tokens; mutable suffix',
             'builder_sha256': sha256(__file__), 'cases': CASES, 'match_counts': counts,
             'body_bytes_without_suffix': {'total': sum(lengths), 'median': lengths[len(lengths)//2],
                 'p95': lengths[int(len(lengths)*.95)], 'max': lengths[-1]},
             'files': {n: sha256(target / n) for n in ('documents.csv', 'attribution.csv', 'matches.csv')}}
        (target / 'manifest.json').write_text(json.dumps(m, indent=2, sort_keys=True) + '\n')
        print(f'Complete: {target} ({sum(lengths):,} normalized text bytes)', flush=True)


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output', required=True)
    build(p.parse_args())
