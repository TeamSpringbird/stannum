#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Frozen-policy full Wikipedia diagnostics on the owned AWS container."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import boto3
import psycopg

ROOT=Path('/opt/stannum-benchmark')
OUT=Path(os.environ['OUT'])
REPO=ROOT/'repo'
sys.path.insert(0,str(REPO/'benchmarks'))
import count_probe
import tin


def main():
    state=dict(status='running',phases=[],profile_failures=[])
    bucket=os.environ['BENCH_BUCKET'];s3=boto3.client('s3'); uploaded={}
    sampler=tin.ResourceSampler('stannum-crossover',OUT/'resources.jsonl')
    sampler.thread.start()
    def checkpoint():
        (OUT/'manifest.json').write_text(json.dumps(state,indent=2)+'\n')
        for path in OUT.rglob('*'):
            if path.is_file() and path.name!='resources.jsonl' and path.suffix in ('.json','.jsonl','.log','.txt','.csv','.data'):
                stamp=(path.stat().st_size,path.stat().st_mtime_ns)
                if uploaded.get(str(path))!=stamp:
                    s3.upload_file(str(path),bucket,'checkpoint/'+str(path.relative_to(OUT)))
                    uploaded[str(path)]=stamp
        s3.put_object(Bucket=bucket,Key='campaign-status.json',Body=json.dumps(state).encode())
    with psycopg.connect('',autocommit=True) as conn:
        def sql(text):return conn.execute(text)
        def snapshot():
            return dict(rows=sql('SELECT count(*) FROM documents').fetchone()[0],
                        visibility=sql("SELECT row_to_json(v) FROM pg_visibility_map_summary('documents') v").fetchone()[0],
                        segments=sql("SELECT json_agg(s) FROM stannum.segment_info('documents_idx') s").fetchone()[0])
        state['settings']=sql('SELECT json_object_agg(name,setting) FROM pg_settings').fetchone()[0]
        try:
            for phase in ('vacuumed','vacuumed-repeat','mutated','revacuumed'):
                if float(Path('/proc/uptime').read_text().split()[0]) > 8*3600-75*60:
                    raise RuntimeError('Preserving export budget before host expiry')
                if phase in ('vacuumed','revacuumed'):sql('VACUUM ANALYZE documents')
                sampler.phase=phase+'-setup'
                state['activity']=phase+'-setup';checkpoint()
                if phase=='mutated':
                    # Preserve the original id/body heap layout. Fractions are
                    # deterministic hashes, not the local fixture's ordinal IDs.
                    state['mutation_rows']={}
                    state['mutation_rows']['id_updates']=sql('UPDATE documents SET id=id WHERE hashtextextended(id,0)%20=0').rowcount
                    state['mutation_rows']['body_updates']=sql("UPDATE documents SET body=body || ' ' WHERE (hashtextextended(id,0) & 9223372036854775807)%10=1").rowcount
                    state['mutation_rows']['deletes']=sql('DELETE FROM documents WHERE hashtextextended(id,0)%101=0').rowcount
                sql('CHECKPOINT')
                state['phases'].append(dict(name=phase,status='running',before=snapshot()))
                checkpoint()
                sampler.phase=phase+'-measurement';state['activity']=phase+'-measurement';checkpoint()
                with (OUT/(phase+'.log')).open('w') as log:
                    subprocess.run([sys.executable,str(REPO/'benchmarks/count_crossover.py'),'--queries',str(ROOT/'datasets/wikipedia/queries.json'),'--output',str(OUT/phase),'--repetitions','9'],check=True,stdout=log,stderr=subprocess.STDOUT)
                state['phases'][-1].update(status='complete',after=snapshot())
                checkpoint()
                if phase in ('vacuumed','mutated'):
                    sampler.phase=phase+'-profiles';state['activity']=phase+'-profiles';checkpoint()
                    profile_dir=OUT/(phase+'-profiles');profile_dir.mkdir()
                    queries=json.loads((ROOT/'datasets/wikipedia/queries.json').read_text())['queries']
                    for q in queries:
                        if q['source_id'] not in (302,88,139,146):continue
                        query=dict(id=str(q['source_id'])+':disjunction',tinql=q['engines']['tin']['disjunction'])
                        for mode in ('off','on'):
                            try:count_probe.profile(query,mode,profile_dir,OUT/'symfs')
                            except Exception as error:state['profile_failures'].append(dict(phase=phase,id=q['source_id'],mode=mode,error=repr(error)))
                    checkpoint()
            state['status']='complete';checkpoint()
            subprocess.run([sys.executable,str(REPO/'benchmarks/count_crossover_report.py'),str(OUT),'--fixed-rule',str(REPO/'docs/benchmarks/count-crossover/100k-selection.json'),'--output',str(OUT/'selection')],check=True)
            checkpoint()
            if state['profile_failures']:raise RuntimeError('Measurements complete but profiling failures retained')
        except BaseException as error:
            state.update(status='failed',error=repr(error));checkpoint();raise
        finally:
            sampler.close()


if __name__=='__main__':
    main()
