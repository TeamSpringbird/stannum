#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Closed-loop concurrent OR-count requests on fresh native database snapshots."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import csv
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
import threading
import time



def percentile(values,p):return sorted(values)[max(0,math.ceil(len(values)*p)-1)] if values else None


def summarize(samples,start,deadline,end):
    # Fixed-window throughput counts successes completed before its boundary.
    # Latency includes drained requests too, so slow in-flight work is not hidden.
    good=[s for s in samples if s['error'] is None]
    return dict(requests=len(samples),errors=len(samples)-len(good),completed_in_window=sum(s['end']<=deadline for s in good),
                qps=sum(s['end']<=deadline for s in good)/(deadline-start),window_seconds=deadline-start,
                drain_seconds=max(0,end-deadline),p50_ms=percentile([s['ms'] for s in good],.5),
                p95_ms=percentile([s['ms'] for s in good],.95),p99_ms=percentile([s['ms'] for s in good],.99),
                distinct_queries=len({s['id'] for s in good}),latency_includes_drain=True)


def load(connect,queries,expected,clients,seconds,seed,folder):
    ready=threading.Barrier(clients+1);go=threading.Event();clock={}
    backend_cpu={}
    def worker(number):
        points=[]
        try:
            with connect() as conn:
                order=queries.copy();random.Random(seed+number).shuffle(order)
                # Connect/configure before the timing barrier.
                conn.execute('SELECT 1')
                pid=conn.execute('SELECT pg_backend_pid()').fetchone()[0]
                def cpu():
                    value=subprocess.check_output(['ps','-p',str(pid),'-o','time='],text=True).strip()
                    fields=value.split(':');return sum(float(v)*60**i for i,v in enumerate(reversed(fields)))
                before=cpu();ready.wait(timeout=120);go.wait(timeout=120)
                cursor=0
                with (folder/f'client-{number}.jsonl').open('w') as raw:
                    while time.perf_counter()<clock['deadline']:
                        q=order[cursor%len(order)];cursor+=1;begin=time.perf_counter();error=None
                        try:
                            count=conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(q['engines']['tin']['disjunction'],)).fetchone()[0]
                            if count!=expected[q['source_id']]:raise ValueError('Result count mismatch')
                        except Exception as e:error=repr(e)
                        end=time.perf_counter();point=dict(id=q['source_id'],client=number,start=begin,end=end,ms=(end-begin)*1000,error=error)
                        raw.write(json.dumps(point)+'\n');points.append(point)
                        if error:break
                backend_cpu[number]=cpu()-before
            return points
        except BaseException:
            ready.abort();raise
    with ThreadPoolExecutor(max_workers=clients) as pool:
        jobs=[pool.submit(worker,i) for i in range(clients)]
        ready.wait(timeout=120);clock['client_cpu_start']=time.process_time();clock['start']=time.perf_counter();clock['deadline']=clock['start']+seconds;go.set()
        points=[point for job in jobs for point in job.result()]
    end=time.perf_counter()
    summary=summarize(points,clock['start'],clock['deadline'],end)
    summary['query_backend_cpu_seconds']=sum(backend_cpu.values())
    summary['query_backend_average_cores']=sum(backend_cpu.values())/(end-clock['start'])
    summary['client_cpu_seconds']=time.process_time()-clock['client_cpu_start']
    summary['client_average_cores']=summary['client_cpu_seconds']/(end-clock['start'])
    summary['host_load_average']=os.getloadavg()
    return summary


def main():
    import psycopg
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('snapshot-run','queries','output'):p.add_argument('--'+name,type=Path,required=True)
    p.add_argument('--states',nargs='+',choices=('clean','mutated'),default=['clean','mutated'])
    p.add_argument('--clients',nargs='+',type=int,default=[8]);p.add_argument('--seconds',type=float,default=20)
    p.add_argument('--rounds',type=int,default=3);p.add_argument('--port',type=int,default=29438)
    p.add_argument('--control-library',type=Path)
    p.add_argument('--candidate-library',type=Path)
    p.add_argument('--allow-identical-binaries',action='store_true',help='Explicitly allow an A/A harness control; never use as optimization evidence')
    a=p.parse_args()
    if bool(a.control_library)!=bool(a.candidate_library):p.error('Both comparison libraries are required')
    if a.seconds<=0 or a.rounds<1 or any(n<1 for n in a.clients):p.error('Durations, rounds and clients must be positive')
    root=a.snapshot_run.resolve();out=a.output.resolve();out.mkdir(parents=True,exist_ok=False);shutil.copy2(__file__,out/'protocol.py')
    source=json.loads((root/'manifest.json').read_text());assert source['status']=='complete'
    assert hashlib.sha256(a.queries.read_bytes()).hexdigest()==source['queries_sha256']
    queries=json.loads(a.queries.read_text())['queries'];assert len(queries)==302
    data=out/'data';lib=out/'stannum.dylib';variants=('main','candidate-default','candidate-bitmaps')
    state=dict(status='running',source=source,trials=[],clients=a.clients,seconds=a.seconds,rounds=a.rounds,
               semantics='Closed loop: each client issues its next request after the previous completes. Latency is localhost client wall time including protocol/driver; no EXPLAIN in timed requests. Not open-loop arrival latency or server-only time.')
    comparison={}
    if a.control_library:
        variants=('control','candidate')
        for name,path in [('control',a.control_library),('candidate',a.candidate_library)]:
            frozen=out/(name+'-source.dylib');shutil.copy2(path,frozen)
            comparison[name]=dict(path=str(frozen),sha256=hashlib.sha256(frozen.read_bytes()).hexdigest())
        if comparison['control']['sha256']==comparison['candidate']['sha256'] and not a.allow_identical_binaries:
            raise ValueError('Identical comparison binaries; rebuild in isolated target directories or explicitly request an A/A control')
        state['comparison_libraries']=comparison
        state['allow_identical_binaries']=a.allow_identical_binaries
    def save():
        temp=out/'manifest.tmp';temp.write_text(json.dumps(state,indent=2)+'\n');temp.replace(out/'manifest.json')
    def cmd(*args):return subprocess.check_output(args,text=True).strip()
    def start():cmd('pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o',f'-p {a.port} -h 127.0.0.1 -c shared_buffers=256MB -c work_mem=16MB -c jit=off -c autovacuum=off','start')
    def stop():cmd('pg_ctl','-D',str(data),'-m','fast','-w','stop')
    def connect():return psycopg.connect(host='127.0.0.1',port=a.port,user=os.environ['USER'],dbname='postgres',autocommit=True,prepare_threshold=None)
    save()
    with open('/tmp/stannum-pgrx.lock','a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            for state_name in a.states:
                snapshot=root/('snapshot' if state_name=='clean' else 'snapshot-mutated')
                rows=json.loads((root/('baseline-results.json' if state_name=='clean' else 'candidate-mutated-results.json')).read_text())
                expected={r['id']:r['count'] for r in rows}
                for r in range(a.rounds):
                    order=variants[r%len(variants):]+variants[:r%len(variants)]
                    for clients in (a.clients if r%2==0 else list(reversed(a.clients))):
                        for variant in order:
                            label=f'{state_name}-r{r+1}-c{clients}-{variant}';print(label,flush=True)
                            folder=out/label;folder.mkdir()
                            if data.exists():shutil.rmtree(data)
                            shutil.copytree(snapshot,data)
                            kind='baseline' if variant=='main' else 'candidate';binary=root/'libraries'/(kind+'.dylib')
                            assert hashlib.sha256(binary.read_bytes()).hexdigest()==source[kind+'_sha256']
                            if comparison:
                                binary=Path(comparison[variant]['path'])
                                assert hashlib.sha256(binary.read_bytes()).hexdigest()==comparison[variant]['sha256']
                            shutil.copy2(binary,lib);start()
                            with connect() as conn:
                                conn.execute("UPDATE pg_proc SET probin=%s WHERE oid IN (SELECT objid FROM pg_depend WHERE refobjid=(SELECT oid FROM pg_extension WHERE extname='stannum') AND classid='pg_proc'::regclass AND deptype='e') AND prolang=(SELECT oid FROM pg_language WHERE lanname='c')",(str(lib),))
                            stop();start()
                            def configured():
                                conn=connect();conn.execute('SET enable_seqscan=off; SET statement_timeout=120000; SET plan_cache_mode=force_custom_plan')
                                if not comparison and variant!='main':conn.execute('SET stannum.force_count_pages='+('on' if variant=='candidate-bitmaps' else 'off'))
                                return conn
                            with configured() as conn,(folder/'plans.jsonl').open('w') as plans:
                                bindings=conn.execute("SELECT DISTINCT probin FROM pg_proc WHERE probin LIKE %s",(str(out)+'%',)).fetchall();assert bindings==[(str(lib),)]
                                # Coverage/correctness warmup is separate from the timed window.
                                for q in queries:
                                    query=q['engines']['tin']['disjunction'];assert conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0]==expected[q['source_id']]
                                    plan=conn.execute('EXPLAIN (FORMAT JSON) SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0][0]
                                    assert plan['Plan'].get('Custom Plan Provider')=='Stannum Count'
                                    plans.write(json.dumps(dict(id=q['source_id'],plan=plan))+'\n')
                            summary=load(configured,queries,expected,clients,a.seconds,11000+r,folder)
                            summary.update(state=state_name,round=r+1,clients=clients,variant=variant)
                            (folder/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');state['trials'].append(summary);save();stop()
                            if summary['errors']:raise RuntimeError('Request errors; comparison invalid')
            state['status']='complete'
        except BaseException as error:state.update(status='failed',error=repr(error));raise
        finally:
            if (data/'postmaster.pid').exists():stop()
            save()
    shutil.rmtree(data)
    lines=['# Concurrent local snapshot comparison','','Closed-loop localhost requests; latency includes driver/protocol and drained requests. QPS uses successful completions inside the fixed window. Ranges span repeated trials, not confidence intervals.','','| State | Clients | Variant | QPS median | Request p95 ms (range) | p99 ms median | Backend cores | Client cores | Min query coverage / 302 |','|---|---:|---|---:|---:|---:|---:|---:|---:|']
    for state_name in a.states:
        for clients in a.clients:
            for variant in variants:
                trials=[t for t in state['trials'] if t['state']==state_name and t['clients']==clients and t['variant']==variant]
                assert len(trials)==a.rounds and all(t['errors']==0 for t in trials)
                med=lambda field:statistics.median(t[field] for t in trials)
                p95=[t['p95_ms'] for t in trials]
                lines.append(f"| {state_name} | {clients} | {variant} | {med('qps'):.1f} | {med('p95_ms'):.3f} ({min(p95):.3f}–{max(p95):.3f}) | {med('p99_ms'):.3f} | {med('query_backend_average_cores'):.2f} | {med('client_average_cores'):.2f} | {min(t['distinct_queries'] for t in trials)} |")
    (out/'report.md').write_text('\n'.join(lines)+'\n')
    fields=['state','round','clients','variant','qps','p50_ms','p95_ms','p99_ms','errors','distinct_queries','drain_seconds']
    with (out/'summary.csv').open('w') as f:
        writer=csv.DictWriter(f,fieldnames=fields,extrasaction='ignore',lineterminator='\n');writer.writeheader();writer.writerows(state['trials'])


if __name__=='__main__':main()
