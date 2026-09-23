#!/usr/bin/env python3
"""Attribute every page a ranked query reads from storage.

Per query, three independent views of the same reads: pg_statio deltas by
relation (index, heap, toast, toast index, everything else), the EXPLAIN node
counters (top node and scan node), and the extension's own phase and area
counters. Warm up on one slice of the published queries, then measure on a
disjoint slice, so the steady state is what is reported.
"""
import subprocess, json, time, sys, os
import psycopg
IMG=sys.argv[1]; WARM=int(sys.argv[2]) if len(sys.argv)>2 else 240; STEADY=int(sys.argv[3]) if len(sys.argv)>3 else 360
NAME='attrib'; PORT=29588; DB=os.environ.get('STANNUM_MOCK','/tmp/stannum-ordinal-poc/mock15m')+'/db'
run=lambda *a: subprocess.run(a,capture_output=True,text=True)
run('docker','rm','-f',NAME); vol=f'{NAME}-data'; run('docker','volume','rm',vol); run('docker','volume','create',vol)
assert run('docker','run','--rm','-v',f'{DB}:/from:ro','-v',f'{vol}:/to','alpine','sh','-c','cp -a /from/. /to/ && rm -f /to/*/docker/postmaster.pid').returncode==0
run('docker','run','-d','--name',NAME,'--cpus','8','--device-read-iops','/dev/vdb:20000','--device-read-bps','/dev/vdb:400mb',
    '--memory','5g','--memory-swap','5g','--shm-size','1g','-p',f'127.0.0.1:{PORT}:5432','-v',f'{vol}:/var/lib/postgresql',
    '-e','POSTGRES_PASSWORD=postgres','-e','POSTGRES_DB=benchmark',IMG,'postgres','-c','shared_buffers=2GB','-c','work_mem=16MB',
    '-c','max_parallel_workers=8','-c','jit=off','-c','track_io_timing=on','-c','autovacuum=off')
for _ in range(90):
    if run('docker','exec',NAME,'pg_isready','-U','postgres').returncode==0: break
    time.sleep(2)
run('docker','run','--rm','--privileged','alpine','sh','-c','sync; echo 3 > /proc/sys/vm/drop_caches')
q=json.load(open(os.path.expanduser('~/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange/queries.json')))['queries']
forms=[(s, x['engines']['tin'][s]) for x in q[:200] for s in ('conjunction','disjunction','phrase')]
SQL='EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id, body, stannum.score(ctid) s FROM documents WHERE body ==> %s ORDER BY s DESC LIMIT 10'
STATIO='''select coalesce(sum(heap_blks_read),0), coalesce(sum(idx_blks_read),0), coalesce(sum(toast_blks_read),0), coalesce(sum(tidx_blks_read),0)
          from pg_statio_all_tables where relname = 'documents' '''
STATIO_ALL='''select coalesce(sum(heap_blks_read+idx_blks_read+coalesce(toast_blks_read,0)+coalesce(tidx_blks_read,0)),0) from pg_statio_all_tables'''
def find(n):
    if n.get('Custom Plan Provider')=='Stannum Text Search Scan': return n
    for ch in n.get('Plans',[]):
        f=find(ch)
        if f: return f
def statio(c):
    c.execute('select pg_stat_force_next_flush()')
    c.execute('select pg_stat_clear_snapshot()')
    h,i,t,ti=c.execute(STATIO).fetchone()
    a=c.execute(STATIO_ALL).fetchone()[0]
    return dict(heap=int(h), index=int(i), toast=int(t), tidx=int(ti), other=int(a)-int(h+i+t+ti))
def parse(field):
    out={}
    for part in (field or '').split(', '):
        if ' ' in part:
            k,v=part.rsplit(' ',1); out[k]=int(v)
    return out
with psycopg.connect(host='127.0.0.1',port=PORT,user='postgres',password='postgres',dbname='benchmark',autocommit=True,prepare_threshold=None) as c:
    c.execute('SET enable_seqscan=off'); c.execute('SET statement_timeout=300000')
    shown=False
    for phase, sl in (('warm-up', slice(0, WARM)), ('steady', slice(WARM, WARM+STEADY))):
        acc={}; n=0; per_style={}
        def add(k,v): acc[k]=acc.get(k,0)+v
        for style, text in forms[sl]:
            s0=statio(c); t=time.perf_counter()
            whole=c.execute(SQL,(text,)).fetchone()[0][0]
            ms=(time.perf_counter()-t)*1000; s1=statio(c); n+=1
            top=whole['Plan']; node=find(top) or {}
            if not shown:
                def shape(p,d=0):
                    print('  '*d + p.get('Node Type','?'), p.get('Custom Plan Provider',''), 'read', p.get('Shared Read Blocks'), 'hit', p.get('Shared Hit Blocks'), 'time', p.get('Actual Total Time'))
                    for ch in p.get('Plans',[]): shape(ch,d+1)
                shape(top); print('  execution ms', whole.get('Execution Time'), 'planning read', (whole.get('Planning') or {}).get('Shared Read Blocks')); shown=True
            for k in ('heap','index','toast','tidx','other'): add('statio '+k, s1[k]-s0[k])
            add('planning read', (whole.get('Planning') or {}).get('Shared Read Blocks',0))
            add('top node read', top.get('Shared Read Blocks',0)); add('scan node read', node.get('Shared Read Blocks',0))
            add('scan node hit', node.get('Shared Hit Blocks',0))
            add('exec ms', whole.get('Execution Time',0)); add('top node ms', top.get('Actual Total Time',0)); add('wall ms', ms)
            add('visibility checks', node.get('Visibility Checks',0)); add('heap fetches', node.get('Heap Fetches',0))
            for k,v in parse(node.get('Disk Pages By Area')).items(): add('area '+k, v)
            for k,v in parse(node.get('Disk Pages By Phase')).items(): add('phase '+k, v)
            st=per_style.setdefault(style, {})
            def sadd(k,v): st[k]=st.get(k,0)+v
            sadd('n',1); sadd('statio index', s1['index']-s0['index']); sadd('statio heap', s1['heap']-s0['heap']); sadd('scan node read', node.get('Shared Read Blocks',0))
            sadd('exec ms', whole.get('Execution Time',0)); sadd('visibility checks', node.get('Visibility Checks',0))
            for k,v in parse(node.get('Disk Pages By Area')).items(): sadd('area '+k, v)
            for k,v in parse(node.get('Disk Pages By Phase')).items(): sadd('phase '+k, v)
        print(f'== {phase}: {n} queries')
        for k in sorted(acc): print(f'   {k:28} {acc[k]:12.0f}  {acc[k]/n:9.1f}/query')
        for s,st in per_style.items():
            n_=st.pop('n'); print(f'   -- style {s} ({n_} queries), per query:')
            for k in sorted(st): print(f'      {k:28} {st[k]/n_:9.1f}')
        sys.stdout.flush()
run('docker','rm','-f',NAME); run('docker','volume','rm',vol)
