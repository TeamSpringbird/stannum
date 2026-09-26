#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Per-query pages and disk reads on the local mock: mock15m-probe.py IMAGE_TAG LABEL [N_PUBLISHED]

Starts a container on the saved mock database with the AWS proportions (5g memory, 2GB shared
buffers), drops the VM page cache before each query, and reports for each query the pages the
scan touched (EXPLAIN BUFFERS), the bytes the container read from disk (cgroup io.stat) and the time.
"""
import json, subprocess, sys, time, re, shutil, os
import psycopg
IMG, LABEL = sys.argv[1], sys.argv[2]; N = int(sys.argv[3]) if len(sys.argv) > 3 else 8
DB = '/tmp/stannum-ordinal-poc/mock15m/db'; NAME = 'mock15m-probe'; PORT = 29580
run = lambda *a, **k: subprocess.run(a, capture_output=True, text=True, **k)
run('docker', 'rm', '-f', NAME)
vol = f'{NAME}-data'; run('docker', 'volume', 'rm', vol)
# Copy the saved database into a fresh volume, as the harness does for --load-database.
run('docker', 'volume', 'create', vol)
c = run('docker', 'run', '--rm', '-v', f'{DB}:/from:ro', '-v', f'{vol}:/to', 'alpine', 'sh', '-c', 'cp -a /from/. /to/ && rm -f /to/*/docker/postmaster.pid')
assert c.returncode == 0, c.stderr
run('docker', 'run', '-d', '--name', NAME, '--cpus', '8', '--device-read-iops', '/dev/vdb:20000', '--device-read-bps', '/dev/vdb:400mb', '--memory', '5g', '--memory-swap', '5g', '--shm-size', '1g',
    '-p', f'127.0.0.1:{PORT}:5432', '-v', f'{vol}:/var/lib/postgresql', '-e', 'POSTGRES_PASSWORD=postgres', '-e', 'POSTGRES_DB=benchmark',
    IMG, 'postgres', '-c', 'shared_buffers=2GB', '-c', 'work_mem=16MB', '-c', 'max_parallel_workers=8', '-c', 'jit=off', '-c', 'track_io_timing=on')
for _ in range(90):
    if run('docker', 'exec', NAME, 'pg_isready', '-U', 'postgres').returncode == 0: break
    time.sleep(2)
def rbytes():
    out = run('docker', 'exec', NAME, 'cat', '/sys/fs/cgroup/io.stat').stdout
    return sum(int(kv.split('=')[1]) for line in out.splitlines() for kv in line.split()[1:] if kv.startswith('rbytes='))
def drop_caches():
    run('docker', 'run', '--rm', '--privileged', 'alpine', 'sh', '-c', 'sync; echo 3 > /proc/sys/vm/drop_caches')
def scan(node):
    if node.get('Custom Plan Provider') == 'Stannum Text Search Scan': return node
    for ch in node.get('Plans', []):
        f = scan(ch)
        if f: return f
q = json.load(open(os.path.expanduser('~/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange/queries.json')))['queries']
Q = [("OR 5 common", 'the OR is OR to OR and OR you'), ("AND 3 common", 'to AND create AND the'), ("phrase 'to create the'", '"to create the"'), ("OR rare-ish", 'ojil OR sure OR that OR is OR possible')]
Q += [(f'{s} #{x["source_id"]}', x['engines']['tin'][s]) for x in q[:N] for s in ('conjunction', 'disjunction', 'phrase')]
tot = dict(mb=0.0, ms=0.0, pages=0, scored=0)
with psycopg.connect(host='127.0.0.1', port=PORT, user='postgres', password='postgres', dbname='benchmark', autocommit=True, prepare_threshold=None) as c:
    c.execute('SET enable_seqscan=off'); c.execute('SET statement_timeout=300000')
    for label, text in Q:
        drop_caches(); r0 = rbytes(); t = time.perf_counter()
        try:
            plan = c.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id, body, stannum.score(ctid) AS score FROM documents WHERE body ==> %s ORDER BY score DESC LIMIT 10', (text,)).fetchone()[0][0]
            node = scan(plan['Plan']) or {}; top = plan['Plan']
            pages = (top.get('Shared Hit Blocks', 0) + top.get('Shared Read Blocks', 0)); scored = node.get('Scored Candidates') or node.get('Exhaustive Score Calls') or 0
        except Exception as e:
            pages = scored = 0; print('  error', repr(e)[:80])
        ms = (time.perf_counter() - t) * 1000; mb = (rbytes() - r0) / 2**20
        tot['mb'] += mb; tot['ms'] += ms; tot['pages'] += pages; tot['scored'] += scored
        print(f'{LABEL} {label:26} pages {pages:7d} scored {scored:7d} read {mb:8.1f} MB {ms:8.0f} ms', flush=True)
print(f"{LABEL} TOTAL {len(Q)} queries: pages {tot['pages']} scored {tot['scored']} read {tot['mb']:.0f} MB {tot['ms']:.0f} ms")
run('docker', 'rm', '-f', NAME); run('docker', 'volume', 'rm', vol)
