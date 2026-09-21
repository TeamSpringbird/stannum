#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Matched off/on filtered-ranking experiment for PR45 on a native snapshot."""
import argparse
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import random
import shutil
import statistics
import subprocess

import psycopg


def nodes(node):
    yield node
    for child in node.get('Plans',[]):yield from nodes(child)


def valid_topk(actual, exhaustive, limit):
    expected=sorted((score for _,score in exhaustive),reverse=True)[:limit]
    scores=[score for _,score in actual];lookup=dict(exhaustive)
    if len(set(key for key,_ in actual))!=len(actual) or len(scores)!=len(expected):raise ValueError('Ranked cardinality/duplicate mismatch')
    for (key,score),wanted in zip(actual,expected):
        if not math.isfinite(score) or key not in lookup or not math.isclose(score,lookup[key],rel_tol=1e-6,abs_tol=1e-6) or not math.isclose(score,wanted,rel_tol=1e-6,abs_tol=1e-6):
            raise ValueError('Ranked membership/score/order mismatch')


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for arg in ('snapshot-run','library','output'):p.add_argument('--'+arg,type=Path,required=True)
    p.add_argument('--commit',required=True);a=p.parse_args();out=a.output.resolve();out.mkdir(parents=True,exist_ok=False)
    root=a.snapshot_run.resolve();assert json.loads((root/'manifest.json').read_text())['status']=='complete'
    shutil.copy2(__file__,out/'protocol.py');lib=out/'stannum.dylib';shutil.copy2(a.library,lib);data=out/'data'
    state=dict(status='running',commit=a.commit,library_sha256=hashlib.sha256(lib.read_bytes()).hexdigest(),rounds=[],rows=100000,scope='PR45 same-binary flag off/on; clean native snapshot; exhaustive same-engine ranking oracle, not Lead')
    def save():(out/'manifest.json').write_text(json.dumps(state,indent=2)+'\n')
    def cmd(*xs):return subprocess.check_output(xs,text=True).strip()
    def start():cmd('pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o','-p 29437 -h 127.0.0.1 -c shared_buffers=256MB -c work_mem=16MB -c jit=off -c autovacuum=off','start')
    def stop():cmd('pg_ctl','-D',str(data),'-m','fast','-w','stop')
    def connect():return psycopg.connect(host='127.0.0.1',port=29437,user=os.environ['USER'],dbname='postgres',autocommit=True,prepare_threshold=None)
    cases=[dict(id=f'{term}/{selectivity}/{k}',term=term,selectivity=selectivity,k=k) for term in ('database','united OR states','new OR york') for selectivity in (1,10,50) for k in (10,100)]
    results=[];save()
    with open('/tmp/stannum-pgrx.lock','a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            # Four rounds balance off/on order exactly.
            for round_id in range(4):
                shuffled=cases.copy();random.Random(45100+round_id).shuffle(shuffled)
                for mode in (('off','on') if round_id%2==0 else ('on','off')):
                    print(f'Frontier round {round_id+1}: {mode}',flush=True)
                    if data.exists():shutil.rmtree(data)
                    shutil.copytree(root/'snapshot',data);start()
                    with connect() as conn:
                        conn.execute("UPDATE pg_proc SET probin=%s WHERE oid IN (SELECT objid FROM pg_depend WHERE refobjid=(SELECT oid FROM pg_extension WHERE extname='stannum') AND classid='pg_proc'::regclass AND deptype='e') AND prolang=(SELECT oid FROM pg_language WHERE lanname='c')",(str(lib),))
                    stop();start()
                    with connect() as conn,(out/f'round-{round_id+1}-{mode}.jsonl').open('w') as raw:
                        conn.execute('SET enable_seqscan=off; SET statement_timeout=120000; SET plan_cache_mode=force_custom_plan')
                        for case in shuffled:
                            predicate=f'(hashtextextended(id,0) & 9223372036854775807)%100 < {case["selectivity"]}'
                            base='SELECT id,stannum.full_score(ctid) AS score FROM documents WHERE body ==> %s AND ('+predicate+')'
                            # Escape literal modulus for psycopg's parameter parser.
                            base=base.replace('%100','%%100')
                            conn.execute('SET stannum.enable_custom_scan=off; SET enable_bitmapscan=on')
                            exhaustive=conn.execute(base,(case['term'],)).fetchall()
                            conn.execute('SET stannum.enable_custom_scan=on; SET enable_bitmapscan=off; SET stannum.experimental_ranked_frontier='+mode)
                            query=base+' ORDER BY score DESC LIMIT '+str(case['k'])
                            actual=conn.execute(query,(case['term'],)).fetchall();valid_topk(actual,exhaustive,case['k'])
                            samples=[]
                            for repetition in range(9):
                                plan=conn.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) '+query,(case['term'],)).fetchone()[0][0]
                                strategies=[n.get('Candidate Strategy') for n in nodes(plan['Plan'])]
                                if mode=='on' and 'resumable ranked frontier (experimental)' not in strategies:raise ValueError(('Frontier not exercised',case,strategies))
                                samples.append(plan['Execution Time']);raw.write(json.dumps(dict(case=case,repetition=repetition,plan=plan))+'\n')
                            results.append(dict(round=round_id+1,mode=mode,**case,median_ms=statistics.median(samples)))
                    stop();state['rounds'].append(dict(round=round_id+1,mode=mode,queries=len(cases)));save()
                    (out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
            state['status']='complete'
            lines=['# PR45 filtered ranked frontier','','Same binary, experimental flag off/on. Four alternating rounds; nine server-timed executions per case. Exhaustive same-engine score/order validation; not an independent Lead oracle.','','| Query / qualifying hash % / K | Off median ms | On median ms | On/off |','|---|---:|---:|---:|']
            for case in cases:
                med={mode:statistics.median(r['median_ms'] for r in results if r['id']==case['id'] and r['mode']==mode) for mode in ('off','on')}
                lines.append(f"| {case['id']} | {med['off']:.3f} | {med['on']:.3f} | {med['on']/med['off']:.2f} |")
            (out/'report.md').write_text('\n'.join(lines)+'\n')
        except BaseException as error:state.update(status='failed',error=repr(error));raise
        finally:
            if (data/'postmaster.pid').exists():stop()
            save()
    shutil.rmtree(data)


if __name__=='__main__':main()
