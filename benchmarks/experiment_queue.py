#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Serial experiment queue; explicit prerequisites, pinned source, retained failures."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--manifest',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
    a.output.mkdir(parents=True,exist_ok=False)
    spec=json.loads(a.manifest.read_text());(a.output/'queue.json').write_text(json.dumps(spec,indent=2)+'\n')
    state=dict(status='waiting',steps=[])
    def save():(a.output/'status.json').write_text(json.dumps(state,indent=2)+'\n')
    save()
    try:
        deadline=time.monotonic()+spec.get('wait_seconds',14400)
        dependencies=[(path,False) for path in spec.get('wait_for',[])]+[(path,True) for path in spec.get('wait_for_terminal',[])]
        for path,allow_failed in dependencies:
            while True:
                try:ready=json.loads(Path(path).read_text()) if Path(path).exists() else {}
                except json.JSONDecodeError:ready={}
                if ready.get('status')=='complete' or (allow_failed and ready.get('status')=='failed'):
                    state.setdefault('prerequisites',[]).append(dict(path=path,status=ready['status']));save();break
                if ready.get('status')=='failed':raise RuntimeError('Prerequisite failed: '+path)
                if time.monotonic()>deadline:raise RuntimeError('Prerequisite deadline: '+path)
                time.sleep(15)
        env=dict(os.environ,**spec.get('env',{}))
        for i,step in enumerate(spec['steps']):
            entry=dict(name=step['name'],status='running');state['steps'].append(entry);state['status']='running';save()
            if step.get('source_commit'):
                actual=subprocess.check_output(['git','-C',step['cwd'],'rev-parse','HEAD'],text=True).strip()
                if actual!=step['source_commit']:raise RuntimeError('Source commit changed')
                if subprocess.check_output(['git','-C',step['cwd'],'status','--porcelain'],text=True).strip():raise RuntimeError('Source worktree is dirty')
            started=time.monotonic()
            with (a.output/f'{i:02d}.log').open('w') as log,open('/tmp/stannum-pgrx.lock','a') as lock:
                if step.get('lock'):fcntl.flock(lock,fcntl.LOCK_EX)
                result=subprocess.run(step['argv'],cwd=step['cwd'],env=env,stdout=log,stderr=subprocess.STDOUT)
            entry.update(status='complete' if result.returncode==0 else 'failed',exit_code=result.returncode,seconds=time.monotonic()-started);save()
            if result.returncode:raise RuntimeError('Step failed: '+step['name'])
        state['status']='complete'
    except BaseException as error:state.update(status='failed',error=repr(error));raise
    finally:save()


if __name__=='__main__':main()
