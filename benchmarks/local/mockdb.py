# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""A throwaway server on the saved mock database, shared by the local probes.

The container gets the AWS proportions (8 CPUs, the NVMe read caps, 5g of
memory, 2GB of shared buffers) and a fresh copy of the database that
mock-build.sh saved under STANNUM_MOCK. Its postgres password is PGPASSWORD
when set, otherwise a random one; it reaches docker through the environment
and psycopg in-process, never a command line.
"""
import json
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import time

READ_CAPS = '--device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb'


def required_path(variable, meaning):
    value = os.environ.get(variable)
    if not value:
        sys.exit(f'set {variable} to {meaning}')
    return Path(value)


def published_queries():
    """The published StackExchange queries under STANNUM_DATASET."""
    path = required_path('STANNUM_DATASET', 'the published StackExchange dataset directory') / 'queries.json'
    return json.loads(path.read_text())['queries']


def run(*args, **kwargs):
    return subprocess.run(args, capture_output=True, text=True, **kwargs)


def drop_caches():
    """Drop the Docker VM's page cache, which the container's cgroup does not charge."""
    run('docker', 'run', '--rm', '--privileged', 'alpine', 'sh', '-c', 'sync; echo 3 > /proc/sys/vm/drop_caches')


class Server:
    """`with Server(image, name, port) as server:` starts it; leaving removes it."""

    def __init__(self, image, name, port, settings=()):
        self.image, self.name, self.port, self.settings = image, name, port, list(settings)
        self.volume = f'{name}-data'
        self.database = required_path('STANNUM_MOCK', 'the directory holding the saved database (db/)') / 'db'
        if not self.database.is_dir():
            sys.exit(f'no saved database at {self.database}; run mock-build.sh first')
        self.password = os.environ.get('PGPASSWORD') or secrets.token_urlsafe(18)

    def __enter__(self):
        self.remove()
        run('docker', 'volume', 'create', self.volume)
        # Copy the saved database into a fresh volume, as the harness does for --load-database.
        copied = run('docker', 'run', '--rm', '-v', f'{self.database}:/from:ro', '-v', f'{self.volume}:/to', 'alpine',
                     'sh', '-c', 'cp -a /from/. /to/ && rm -f /to/*/docker/postmaster.pid')
        if copied.returncode:
            sys.exit(copied.stderr)
        settings = [a for s in ['shared_buffers=2GB', 'work_mem=16MB', 'max_parallel_workers=8', 'jit=off',
                                'track_io_timing=on', *self.settings] for a in ('-c', s)]
        started = run('docker', 'run', '-d', '--name', self.name, '--cpus', '8',
                      *shlex.split(os.environ.get('STANNUM_DOCKER_RUN_ARGS', READ_CAPS)),
                      '--memory', '5g', '--memory-swap', '5g', '--shm-size', '1g',
                      '-p', f'127.0.0.1:{self.port}:5432', '-v', f'{self.volume}:/var/lib/postgresql',
                      '-e', 'POSTGRES_PASSWORD', '-e', 'POSTGRES_DB=benchmark', self.image, 'postgres', *settings,
                      env=dict(os.environ, POSTGRES_PASSWORD=self.password))
        if started.returncode:
            self.remove()
            sys.exit(started.stderr)
        for _ in range(90):
            if run('docker', 'exec', self.name, 'pg_isready', '-U', 'postgres').returncode == 0:
                return self
            time.sleep(2)
        self.remove()
        sys.exit(f'{self.name} did not become ready')

    def __exit__(self, *_):
        self.remove()

    def remove(self):
        run('docker', 'rm', '-f', self.name)
        run('docker', 'volume', 'rm', self.volume)

    def connect(self):
        import psycopg
        return psycopg.connect(host='127.0.0.1', port=self.port, user='postgres', password=self.password,
                               dbname='benchmark', autocommit=True, prepare_threshold=None)

    def read_bytes(self):
        """Bytes the container has read from disk (cgroup io.stat rbytes)."""
        out = run('docker', 'exec', self.name, 'cat', '/sys/fs/cgroup/io.stat').stdout
        return sum(int(kv.split('=')[1]) for line in out.splitlines() for kv in line.split()[1:]
                   if kv.startswith('rbytes='))


def scan_node(node):
    """The Stannum custom scan node of an EXPLAIN plan, or None."""
    if node.get('Custom Plan Provider') == 'Stannum Text Search Scan':
        return node
    for child in node.get('Plans', []):
        found = scan_node(child)
        if found:
            return found
    return None
