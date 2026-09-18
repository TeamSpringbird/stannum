#!/usr/bin/env python3
"""Watch a local baseline series and request macOS completion notifications."""
import argparse
import datetime as dt
import json
import os
from pathlib import Path
import subprocess
import time


def read(path):
    try:
        return json.loads(path.read_text())
    except (OSError, ValueError):
        return {}


def milestones(root):
    events = {}
    small = read(root / 'wikipedia-100000/campaign.json')
    if small.get('status') in ('complete', 'incomplete', 'failed'):
        jobs = small.get('jobs', [])
        passed = sum(j.get('status') == 'complete' for j in jobs)
        events['100k'] = ('Lead benchmarks: 100k finished',
                         f'{passed}/{len(jobs)} trials passed. Campaign {small["status"]}. Results: {root}/wikipedia-100000/report.md')
    series = read(root / 'status.json')
    if series.get('status') in ('complete', 'incomplete', 'failed'):
        events['series'] = ('Lead benchmark series finished',
                            f'Status: {series["status"]}. Results: {root}')
    elif series.get('pid'):
        try:
            os.kill(series['pid'], 0)
        except ProcessLookupError:
            events['stopped'] = ('Lead benchmark process stopped',
                                 f'No completion recorded. Inspect logs in {root}.')
    return events


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    args = parser.parse_args()
    root = args.directory.resolve()
    record = root / 'notifications.json'
    state = read(record)
    while True:
        events = milestones(root)
        for key, (title, message) in events.items():
            if key in state:
                continue
            # Pass strings as argv, never interpolate them into AppleScript source.
            result = subprocess.run(['osascript', '-e',
                'on run argv\n display notification (item 2 of argv) with title (item 1 of argv) sound name "Glass"\nend run',
                title, message], capture_output=True, text=True)
            if result.returncode:
                print(f'Notification request failed: {result.stderr}', flush=True)
                continue
            state[key] = {'requested_at': dt.datetime.now(dt.timezone.utc).isoformat(),
                          'title': title, 'message': message,
                          'delivery': 'Requested from macOS; display depends on notification settings'}
            temporary = record.with_suffix('.tmp')
            temporary.write_text(json.dumps(state, indent=2) + '\n')
            temporary.replace(record)
            print(f'Notification requested: {title}', flush=True)
        if any(key in state for key in ('series', 'stopped')):
            return
        time.sleep(30)


if __name__ == '__main__':
    main()
