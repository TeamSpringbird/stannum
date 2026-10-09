#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Samples a running benchmark's database over time.

    timeseries.py RUN_DIR [INTERVAL_SECONDS]

Waits for RUN_DIR/manifest.json to name the job's container, then, every
INTERVAL_SECONDS (default 60) until the container stops, appends one JSON line
to RUN_DIR/timeseries.jsonl: the index and table sizes, live and dead tuples,
(auto)vacuum counts and any VACUUM in progress, and the index's segments (count,
documents, dead documents, pages). A long run's query latencies are in the
harness's own per-second samples; this records how the index changes under it.
"""
import datetime
import json
from pathlib import Path
import subprocess
import sys
import time

QUERY = """
SELECT json_build_object(
  'index_bytes', pg_relation_size('documents_body_stannum_idx'),
  'table_bytes', pg_table_size('documents'),
  'live_tuples', s.n_live_tup, 'dead_tuples', s.n_dead_tup,
  'vacuum_count', s.vacuum_count, 'autovacuum_count', s.autovacuum_count,
  'last_autovacuum', s.last_autovacuum,
  'vacuum_running', (SELECT coalesce(json_agg(json_build_object(
      'pid', p.pid, 'phase', p.phase, 'heap_blks_scanned', p.heap_blks_scanned,
      'heap_blks_total', p.heap_blks_total, 'seconds', extract(epoch FROM now() - a.xact_start)::int)), '[]')
    FROM pg_stat_progress_vacuum p JOIN pg_stat_activity a USING (pid)),
  'segments', (SELECT json_build_object('count', count(*), 'mutable', count(*) FILTER (WHERE kind = 'mutable'),
      'docs', sum(docs), 'dead_docs', sum(dead_docs), 'pages', sum(total_pages))
    FROM stannum.segment_info('documents_body_stannum_idx')))
FROM pg_stat_user_tables s WHERE s.relname = 'documents'
"""


def container_of(run):
    manifest = run / 'manifest.json'
    if not manifest.exists():
        return None
    jobs = json.loads(manifest.read_text()).get('jobs') or []
    return jobs[-1].get('container') if jobs else None


def running(name):
    result = subprocess.run(['docker', 'inspect', '-f', '{{.State.Running}}', name],
                            capture_output=True, text=True)
    return result.returncode == 0 and result.stdout.strip() == 'true'


def sample(name):
    result = subprocess.run(['docker', 'exec', name, 'psql', '-U', 'postgres', '-d', 'benchmark',
                             '-XqAt', '-c', QUERY], capture_output=True, text=True, timeout=120)
    if result.returncode:
        return dict(error=result.stderr.strip()[-300:])
    return json.loads(result.stdout)


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    run = Path(sys.argv[1])
    interval = int(sys.argv[2]) if len(sys.argv) == 3 else 60
    name = None
    for _ in range(4 * 3600):
        name = container_of(run)
        if name and running(name):
            break
        time.sleep(1)
    else:
        sys.exit(f'no running container for {run}')
    out = run / 'timeseries.jsonl'
    while running(name):
        record = dict(at=datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds'))
        try:
            record.update(sample(name))
        except (subprocess.TimeoutExpired, json.JSONDecodeError) as error:
            record['error'] = str(error)[:300]
        with out.open('a') as handle:
            handle.write(json.dumps(record) + '\n')
        time.sleep(interval)


if __name__ == '__main__':
    main()
