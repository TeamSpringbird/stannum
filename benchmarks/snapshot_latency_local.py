#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Balanced single-client latency comparison from a validated native snapshot."""
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
import time

import psycopg


def digest(path):return hashlib.sha256(path.read_bytes()).hexdigest()

def percentile(values,p):
    return sorted(values)[max(0,math.ceil(len(values)*p)-1)]


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot-run',type=Path,required=True)
    parser.add_argument('--queries',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--port',type=int,default=29436)
    args=parser.parse_args();root=args.snapshot_run.resolve();out=args.output.resolve()
    manifest=json.loads((root/'manifest.json').read_text());assert manifest['status']=='complete'
    out.mkdir(parents=True,exist_ok=False);shutil.copy2(__file__,out/'protocol.py')
    lib=out/'stannum.dylib';data=out/'data'
    expected={r['id']:r['count'] for r in json.loads((root/'baseline-results.json').read_text())}
    queries=json.loads(args.queries.read_text())['queries'];assert len(queries)==302
    assert digest(args.queries)==manifest['queries_sha256']
    state=dict(status='running',source_manifest=manifest,rounds=[],repetitions=9,
               metric='Server EXPLAIN ANALYZE Execution Time; single client; one warmup per query; fresh physical snapshot per variant',
               percentile='nearest rank of the 302 per-query medians, equal weight per query; not concurrent-load or request-weighted p95')
    def save():(out/'manifest.json').write_text(json.dumps(state,indent=2)+'\n')
    def cmd(*args):return subprocess.check_output(args,text=True).strip()
    def start():cmd('pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o',f'-p {args.port} -h 127.0.0.1 -c shared_buffers=256MB -c work_mem=16MB -c jit=off -c autovacuum=off','start')
    def stop():cmd('pg_ctl','-D',str(data),'-m','fast','-w','stop')
    def connect():return psycopg.connect(host='127.0.0.1',port=args.port,user=os.environ['USER'],dbname='postgres',autocommit=True,prepare_threshold=None)
    variants=('main','candidate-default','candidate-bitmaps')
    # Latin-square order balances each variant's position across three rounds.
    orders=[variants,variants[1:]+variants[:1],variants[2:]+variants[:2]]
    results=[];save()
    with open('/tmp/stannum-pgrx.lock','a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            for round_id,order in enumerate(orders):
                shuffled=queries.copy();random.Random(91000+round_id).shuffle(shuffled)
                for variant in order:
                    print(f'Round {round_id+1}: {variant}',flush=True)
                    if data.exists():shutil.rmtree(data)
                    shutil.copytree(root/'snapshot',data)
                    kind='baseline' if variant=='main' else 'candidate'
                    source=root/'libraries'/(kind+'.dylib')
                    assert digest(source)==manifest[kind+'_sha256']
                    shutil.copy2(source,lib);start()
                    # The snapshot points to its original private library. Redirect
                    # only extension C functions, then restart before invoking any.
                    with connect() as conn:
                        conn.execute("UPDATE pg_proc SET probin=%s WHERE oid IN (SELECT objid FROM pg_depend WHERE refobjid=(SELECT oid FROM pg_extension WHERE extname='stannum') AND classid='pg_proc'::regclass AND deptype='e') AND prolang=(SELECT oid FROM pg_language WHERE lanname='c')",(str(lib),))
                    stop();start()
                    with connect() as conn, (out/f'round-{round_id+1}-{variant}.jsonl').open('w') as raw:
                        bindings=conn.execute("SELECT DISTINCT probin FROM pg_proc WHERE probin LIKE %s",(str(out)+'%',)).fetchall();assert bindings==[(str(lib),)]
                        conn.execute('SET enable_seqscan=off; SET plan_cache_mode=force_custom_plan; SET statement_timeout=120000')
                        if variant!='main':conn.execute('SET stannum.force_count_pages='+('on' if variant=='candidate-bitmaps' else 'off'))
                        for q in shuffled:
                            query=q['engines']['tin']['disjunction']
                            count=conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0]
                            assert count==expected[q['source_id']],(variant,q['source_id'],count)
                            samples=[]
                            for repetition in range(9):
                                plan=conn.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0][0]
                                node=plan['Plan'];assert node.get('Custom Plan Provider')=='Stannum Count'
                                if variant=='candidate-bitmaps':assert node['Count Strategy']=='page bitmaps'
                                samples.append(plan['Execution Time']);raw.write(json.dumps(dict(id=q['source_id'],repetition=repetition,plan=plan))+'\n')
                            results.append(dict(round=round_id+1,variant=variant,id=q['source_id'],text=q['text'],median_ms=statistics.median(samples)))
                    stop()
                    times=[r['median_ms'] for r in results if r['round']==round_id+1 and r['variant']==variant]
                    state['rounds'].append(dict(round=round_id+1,variant=variant,p50_ms=percentile(times,.5),p95_ms=percentile(times,.95),p99_ms=percentile(times,.99),sum_ms=sum(times)))
                    save();(out/'results.json').write_text(json.dumps(results,indent=2)+'\n')
            state['status']='complete'
        except BaseException as error:state.update(status='failed',error=repr(error));raise
        finally:
            if (data/'postmaster.pid').exists():stop()
            save()
    shutil.rmtree(data)
    print(json.dumps(state['rounds'],indent=2),flush=True)


if __name__=='__main__':main()
