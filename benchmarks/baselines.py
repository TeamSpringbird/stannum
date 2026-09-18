#!/usr/bin/env python3
"""Run both real-corpus baselines sequentially, retaining a frozen harness and logs."""
import argparse
import datetime as dt
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

import run as bench


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--datasets', required=True)
    parser.add_argument('--output', required=True)
    parser.add_argument('--image', default='lead-bench:local')
    args = parser.parse_args()
    root = Path(args.output).resolve()
    # Verify both before starting any measured work.
    for rows in (100000, 1000000):
        bench.dataset.verify(Path(args.datasets) / f'wikipedia-{rows}', rows)
    root.mkdir(parents=True, exist_ok=False)
    source = bench.provenance(root)
    bench.save(root / 'source.json', source)
    image = bench.command(['docker', 'image', 'inspect', args.image, '--format', '{{.Id}}'])
    protocol = root / 'protocol'
    protocol.mkdir()
    for name in ('run.py', 'campaign.py', 'dataset.py', 'baselines.py', 'Dockerfile', 'Dockerfile.dockerignore'):
        shutil.copy2(Path(__file__).resolve().parent / name, protocol / name)
    state = {'status': 'running', 'started_at': dt.datetime.now(dt.timezone.utc).isoformat(),
             'pid': os.getpid(), 'image_id': image, 'campaigns': {}}
    bench.save(root / 'status.json', state)
    env = dict(os.environ, LEAD_BENCH_ROOT=str(bench.ROOT))
    try:
        for rows in (100000, 1000000):
            name = f'wikipedia-{rows}'
            state['active'] = name
            bench.save(root / 'status.json', state)
            cmd = [sys.executable, str(protocol / 'campaign.py'), '--dataset',
                   str(Path(args.datasets).resolve() / name), '--output', str(root / name),
                   '--rows', str(rows), '--image', image, '--source-manifest', str(root / 'source.json'),
                   '--repetitions', '5', '--seconds', str(300 if rows == 100000 else 1800), '--warmup', '30',
                   '--clients', '2', '--write-rate', '20', '--statement-timeout-ms', '1800000',
                   '--label', name + '-v1']
            print('Starting ' + name, flush=True)
            with (root / (name + '.log')).open('w') as log:
                result = subprocess.run(cmd, env=env, stdout=log, stderr=subprocess.STDOUT)
            manifest = root / name / 'campaign.json'
            status = json.loads(manifest.read_text())['status'] if manifest.exists() else 'failed'
            state['campaigns'][name] = {'exit_code': result.returncode, 'status': status}
            if (root / name / 'aggregate.json').exists():
                with (root / name / 'history.csv').open('w') as history:
                    subprocess.run([sys.executable, str(protocol / 'run.py'), 'history', str(root / name)],
                                   env=env, stdout=history, check=True)
            print(f'Finished {name}: {status}', flush=True)
        state['status'] = 'complete' if all(v['status'] == 'complete' for v in state['campaigns'].values()) else 'incomplete'
    except BaseException as error:
        state['status'] = 'failed'
        state['error'] = str(error)
        raise
    finally:
        state.pop('active', None)
        state['finished_at'] = dt.datetime.now(dt.timezone.utc).isoformat()
        bench.save(root / 'status.json', state)
    if state['status'] != 'complete':
        raise SystemExit(2)


if __name__ == '__main__':
    main()
