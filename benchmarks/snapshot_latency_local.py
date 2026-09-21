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
import sys
import time

import psycopg


def digest(path):return hashlib.sha256(path.read_bytes()).hexdigest()

def percentile(values,p):
    return sorted(values)[max(0,math.ceil(len(values)*p)-1)]



def selector_report(output):
    """Summarize only complete campaigns; estimation is part of total selection."""
    output=Path(output)
    manifest=json.loads((output/'manifest.json').read_text())
    if manifest['status']!='complete':raise ValueError('Incomplete selector campaign')
    records=json.loads((output/'results.json').read_text())
    if len(records)!=302*len(manifest['rounds']):raise ValueError('Incomplete query coverage')
    for trial in manifest['rounds']:
        ids=[r['id'] for r in records if r['round']==trial['round'] and r['variant']==trial['variant']]
        if len(ids)!=302 or len(set(ids))!=302:raise ValueError('Duplicate or missing query results')
    variants=sorted({r['variant'] for r in records})
    summary=[]
    strategies={}
    for path in output.glob('round-*-candidate-default.jsonl'):
        round_id=int(path.name.split('-')[1])
        for line in path.open():
            row=json.loads(line)
            strategies[(round_id,row['id'])]=row['plan']['Plan']['Count Strategy']
    for path in output.glob('round-*.jsonl'):
        round_id=int(path.name.split('-')[1])
        variant=path.name.split('-',2)[2].removesuffix('.jsonl')
        if variant not in ('instrumented','shadow','selected'):continue
        for line in path.open():
            row=json.loads(line);node=row['plan']['Plan']
            expected_strategy=strategies[(round_id,row['id'])]
            if (variant=='selected' and node['Count Estimate Supported (Last)']
                    and node['Count Estimate Postings (Last)']>=manifest['selector_threshold']):
                expected_strategy='page bitmaps'
            if node['Count Strategy']!=expected_strategy:
                raise ValueError('Unexpected selector decision')
    for variant in variants:
        rounds=[r for r in manifest['rounds'] if r['variant']==variant]
        plans=[json.loads(line)['plan'] for path in output.glob(f'round-*-{variant}.jsonl') for line in path.open()]
        expected=len([r for r in records if r['variant']==variant])*manifest['repetitions']
        if len(plans)!=expected:raise ValueError('Incomplete raw plan coverage')
        item=dict(variant=variant,samples=len(plans),
                  median_round_p50_ms=statistics.median(r['p50_ms'] for r in rounds),
                  median_round_p95_ms=statistics.median(r['p95_ms'] for r in rounds),
                  median_round_sum_ms=statistics.median(r['sum_ms'] for r in rounds))
        if variant in ('instrumented','shadow','selected','selector-bitmaps'):
            estimates=[r['Plan']['Count Estimation Time'] for r in plans]
            item.update(estimation_median_us=1000*statistics.median(estimates),
                        estimation_p95_us=1000*percentile(estimates,.95),
                        estimation_percent_total=100*sum(estimates)/sum(r['Execution Time'] for r in plans),
                        page_selections=sum(r['Plan']['Count Strategy']=='page bitmaps' for r in plans))
        summary.append(item)
    (output/'selector-summary.json').write_text(json.dumps(summary,indent=2)+'\n')
    lines=['# Selector experiment', '', 'Serial server timings; p95 is across query medians, not concurrent request latency. Estimation is included in execution time. Round medians are descriptive, not confidence intervals.', '',
           '| Variant | p50 ms | p95 ms | Sum of query medians ms | Estimation median / p95 us | Estimation % of summed execution |',
           '|---|---:|---:|---:|---:|---:|']
    for r in summary:
        estimation=f"{r['estimation_median_us']:.3f} / {r['estimation_p95_us']:.3f}" if 'estimation_median_us' in r else 'unavailable'
        fraction=f"{r['estimation_percent_total']:.4f}%" if 'estimation_percent_total' in r else 'unavailable'
        lines.append(f"| {r['variant']} | {r['median_round_p50_ms']:.3f} | {r['median_round_p95_ms']:.3f} | {r['median_round_sum_ms']:.3f} | {estimation} | {fraction} |")
    (output/'selector-report.md').write_text('\n'.join(lines)+'\n')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--snapshot-run',type=Path,required=True)
    parser.add_argument('--queries',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--state',choices=('clean','mutated'),default='clean')
    parser.add_argument('--profile-ids',type=int,nargs='*',default=[],help='macOS sample profiles, collected after the final timed trial of each variant')
    parser.add_argument('--port',type=int,default=29436)
    parser.add_argument('--control-library',type=Path,help='Override archived candidate control with a pinned compatible binary')
    parser.add_argument('--selector-library',type=Path,help='Compare original candidate, instrumentation only, and shadow estimation')
    parser.add_argument('--selector-threshold',type=int,default=0,help='Optional experimental threshold arm; zero omits it')
    parser.add_argument('--rounds',type=int,default=3)
    parser.add_argument('--repetitions',type=int,default=9)
    args=parser.parse_args();
    if args.rounds < 1 or args.repetitions < 1 or args.selector_threshold < 0:parser.error('Invalid repetition count or threshold')
    if args.selector_threshold and not args.selector_library:parser.error('Threshold requires selector library')
    if args.profile_ids and sys.platform!='darwin':parser.error('Native sampling currently requires macOS')
    root=args.snapshot_run.resolve();out=args.output.resolve()
    manifest=json.loads((root/'manifest.json').read_text());assert manifest['status']=='complete'
    out.mkdir(parents=True,exist_ok=False);shutil.copy2(__file__,out/'protocol.py')
    lib=out/'stannum.dylib';data=out/'data'
    control_source=None
    if args.control_library:
        control_source=out/'control-source.dylib'
        shutil.copy2(args.control_library,control_source)
    selector_source=None
    if args.selector_library:
        selector_source=out/'selector-source.dylib'
        shutil.copy2(args.selector_library,selector_source)
    expected_file='baseline-results.json' if args.state=='clean' else 'candidate-mutated-results.json'
    snapshot_name='snapshot' if args.state=='clean' else 'snapshot-mutated'
    expected={r['id']:r['count'] for r in json.loads((root/expected_file).read_text())}
    queries=json.loads(args.queries.read_text())['queries'];assert len(queries)==302
    assert digest(args.queries)==manifest['queries_sha256']
    assert set(args.profile_ids) <= {q['source_id'] for q in queries}
    state=dict(status='running',snapshot_state=args.state,source_manifest=manifest,rounds=[],repetitions=args.repetitions,
               control_sha256=digest(control_source) if control_source else None,
               selector_sha256=digest(selector_source) if selector_source else None,selector_threshold=args.selector_threshold,
               metric='Server EXPLAIN ANALYZE Execution Time; single client; one warmup per query; fresh physical snapshot per variant',
               percentile='nearest rank of the 302 per-query medians, equal weight per query; not concurrent-load or request-weighted p95')
    def save():(out/'manifest.json').write_text(json.dumps(state,indent=2)+'\n')
    def cmd(*args):return subprocess.check_output(args,text=True).strip()
    def start():cmd('pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o',f'-p {args.port} -h 127.0.0.1 -c shared_buffers=256MB -c work_mem=16MB -c jit=off -c autovacuum=off','start')
    def stop():cmd('pg_ctl','-D',str(data),'-m','fast','-w','stop')
    def connect():return psycopg.connect(host='127.0.0.1',port=args.port,user=os.environ['USER'],dbname='postgres',autocommit=True,prepare_threshold=None)
    variants=('main','candidate-default','candidate-bitmaps')
    # Latin-square order balances each variant's position across three rounds.
    if selector_source:
        variants=('candidate-default','instrumented','shadow')
        if args.selector_threshold:variants+=('selected','selector-bitmaps')
    orders=[variants[i%len(variants):]+variants[:i%len(variants)] for i in range(args.rounds)]
    results=[];save()
    with open('/tmp/stannum-pgrx.lock','a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            for round_id,order in enumerate(orders):
                shuffled=queries.copy();random.Random(91000+round_id).shuffle(shuffled)
                for variant in order:
                    print(f'Round {round_id+1}: {variant}',flush=True)
                    if data.exists():shutil.rmtree(data)
                    shutil.copytree(root/snapshot_name,data)
                    kind='baseline' if variant=='main' else 'candidate'
                    source=root/'libraries'/(kind+'.dylib')
                    assert digest(source)==manifest[kind+'_sha256']
                    if variant=='candidate-default' and control_source:
                        source=control_source
                        assert digest(source)==state['control_sha256']
                    if variant in ('instrumented','shadow','selected','selector-bitmaps'):
                        source=selector_source
                        assert digest(source)==state['selector_sha256']
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
                        if variant in ('instrumented','shadow','selected','selector-bitmaps'):
                            conn.execute('SET stannum.profile_count_selection='+('on' if variant=='shadow' else 'off'))
                            conn.execute('SET stannum.count_page_threshold='+str(args.selector_threshold if variant=='selected' else 0))
                            conn.execute('SET stannum.force_count_pages='+('on' if variant=='selector-bitmaps' else 'off'))
                        for q in shuffled:
                            query=q['engines']['tin']['disjunction']
                            count=conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0]
                            assert count==expected[q['source_id']],(variant,q['source_id'],count)
                            samples=[]
                            for repetition in range(args.repetitions):
                                plan=conn.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0][0]
                                node=plan['Plan'];assert node.get('Custom Plan Provider')=='Stannum Count'
                                if variant in ('candidate-bitmaps','selector-bitmaps'):assert node['Count Strategy']=='page bitmaps'
                                if variant in ('instrumented','shadow','selected','selector-bitmaps'):
                                    assert node['Count Selection Calls']==1
                                    assert node['Count Selection Time']>=node['Count Estimation Time']>=0
                                    assert node['Count Estimation Calls']==(1 if variant in ('shadow','selected') else 0)
                                samples.append(plan['Execution Time']);raw.write(json.dumps(dict(id=q['source_id'],repetition=repetition,plan=plan))+'\n')
                            results.append(dict(round=round_id+1,variant=variant,id=q['source_id'],text=q['text'],median_ms=statistics.median(samples)))
                        if args.profile_ids and round_id==len(orders)-1:
                            for q in queries:
                                if q['source_id'] not in args.profile_ids:continue
                                path=out/f"profile-{variant}-{q['source_id']}.txt"
                                pid=conn.execute('SELECT pg_backend_pid()').fetchone()[0]
                                with (out/f"profile-{variant}-{q['source_id']}.log").open('w') as log:
                                    sampler=subprocess.Popen(['sample',str(pid),'5','1','-file',str(path)],stdout=log,stderr=subprocess.STDOUT)
                                    deadline=time.monotonic()+20
                                    try:
                                        while sampler.poll() is None:
                                            if time.monotonic()>deadline:raise TimeoutError('Sampler exceeded budget')
                                            count=conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(q['engines']['tin']['disjunction'],)).fetchone()[0]
                                            assert count==expected[q['source_id']]
                                        if sampler.returncode!=0 or not path.exists():raise RuntimeError('Native profile failed; inspect sampler log')
                                    finally:
                                        if sampler.poll() is None:sampler.kill()
                                        sampler.wait()
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
    if selector_source:selector_report(out)
    print(json.dumps(state['rounds'],indent=2),flush=True)


if __name__=='__main__':main()
