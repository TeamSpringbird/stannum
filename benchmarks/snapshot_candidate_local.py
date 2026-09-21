#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Native PostgreSQL physical-snapshot/binary compatibility rehearsal.

Uses private library copies and an owned cluster; never replaces installed libs.
The baseline and candidate must have identical extension SQL interfaces.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

import psycopg


def sha(path):
    with path.open('rb') as stream:return hashlib.file_digest(stream,'sha256').hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__)
    for name in ('input','queries','output','baseline_library','candidate_library'):
        p.add_argument('--'+name.replace('_','-'),type=Path,required=True)
    for name in ('baseline_commit','candidate_commit'):p.add_argument('--'+name.replace('_','-'),required=True)
    p.add_argument('--rows',type=int,default=100000)
    p.add_argument('--aws-mutations',action='store_true',help='Use the AWS hash-selected 5%% id updates, 10%% whitespace updates, 1/101 deletes')
    p.add_argument('--port',type=int,default=29435)
    args=p.parse_args();out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
    shutil.copy2(__file__,out/'protocol.py')
    libs=out/'libraries';libs.mkdir()
    for mode in ('baseline','candidate'):shutil.copy2(getattr(args,mode+'_library'),libs/(mode+'.dylib'))
    runtime=libs/'stannum.dylib';shutil.copy2(libs/'baseline.dylib',runtime)
    data=out/'data';snapshot=out/'snapshot'
    queries=json.loads(args.queries.read_text())['queries'];assert len(queries)==302
    env=dict(os.environ,PGHOST='127.0.0.1',PGPORT=str(args.port),PGDATABASE='postgres',PGUSER=os.environ['USER'])
    for key in ('PGOPTIONS','PGSERVICE','PGSERVICEFILE','PGPASSWORD'):env.pop(key,None)
    state=dict(status='starting',expected_rows=args.rows,aws_mutations=args.aws_mutations,phases=[],baseline_commit=args.baseline_commit,candidate_commit=args.candidate_commit,
               baseline_sha256=sha(libs/'baseline.dylib'),candidate_sha256=sha(libs/'candidate.dylib'),
               input_sha256=sha(args.input),queries_sha256=sha(args.queries))
    def save(): (out/'manifest.json').write_text(json.dumps(state,indent=2)+'\n')
    def command(*cmd):return subprocess.check_output(cmd,env=env,text=True).strip()
    def sql(query):return command('psql','-XqAt','-v','ON_ERROR_STOP=1','-c',query)
    def start():
        command('pg_ctl','-D',str(data),'-l',str(out/'server.log'),'-o',f'-p {args.port} -h 127.0.0.1 -c shared_buffers=256MB -c maintenance_work_mem=256MB -c work_mem=16MB -c jit=off -c autovacuum=off','start')
    def stop():command('pg_ctl','-D',str(data),'-m','fast','-w','stop')
    def fingerprint():
        return json.loads(sql("SELECT json_build_object('rows',(SELECT count(*) FROM documents),'visibility',(SELECT row_to_json(v) FROM pg_visibility_map_summary('documents') v),'segments',(SELECT json_agg(s) FROM stannum.segment_info('documents_idx') s))"))
    def connect():return psycopg.connect(host='127.0.0.1',port=args.port,user=env['PGUSER'],dbname='postgres',autocommit=True)
    def check(phase,candidate):
        print('Checking '+phase,flush=True);started=time.monotonic()
        expected_hash=state['candidate_sha256' if candidate else 'baseline_sha256']
        if sha(runtime)!=expected_hash:raise ValueError('Wrong runtime library bytes')
        with connect() as conn:
            bindings=conn.execute("SELECT DISTINCT probin FROM pg_proc WHERE oid IN (SELECT objid FROM pg_depend WHERE refobjid=(SELECT oid FROM pg_extension WHERE extname='stannum') AND classid='pg_proc'::regclass AND deptype='e') AND prolang=(SELECT oid FROM pg_language WHERE lanname='c')").fetchall()
            if bindings!=[(str(runtime),)]:raise ValueError(('Unexpected library bindings',bindings))
            # Independent full-corpus lexical oracle for these normalized OR queries.
            terms={t for q in queries for t in q['text'].split()}
            postings={t:set() for t in terms}
            with conn.transaction(), conn.cursor(name='oracle_documents') as documents:
                documents.itersize=2000
                documents.execute('SELECT body FROM documents')
                for n,(body,) in enumerate(documents):
                    for term in set(body.split()) & terms:postings[term].add(n)
            expected=[len(set().union(*(postings[t] for t in q['text'].split()))) for q in queries]
            conn.execute('SET enable_seqscan=off; SET statement_timeout=120000; SET plan_cache_mode=force_custom_plan')
            actual=[]
            with (out/(phase+'-plans.jsonl')).open('w') as plans:
                for i,q in enumerate(queries):
                    modes=('off','on') if candidate else ('off',)
                    for mode in modes:
                        if candidate:conn.execute('SET stannum.force_count_pages='+mode)
                        query=q['engines']['tin']['disjunction']
                        count=conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0]
                        if count!=expected[i]:raise ValueError((phase,q['source_id'],mode,count,expected[i]))
                        plan=conn.execute('EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0][0]
                        node=plan['Plan']
                        if node.get('Custom Plan Provider')!='Stannum Count':raise ValueError('Unexpected count plan')
                        if candidate and mode=='on' and node.get('Count Strategy')!='page bitmaps':raise ValueError('Forced bitmap path not exercised')
                        result=dict(id=q['source_id'],mode=mode,count=count,plan=plan)
                        plans.write(json.dumps(result)+'\n');actual.append(dict(id=q['source_id'],mode=mode,count=count,execution_ms=plan['Execution Time']))
            (out/(phase+'-results.json')).write_text(json.dumps(actual,indent=2)+'\n')
        state['phases'].append(dict(name=phase,checks=len(actual),library_sha256=expected_hash,seconds=time.monotonic()-started,fingerprint=fingerprint()))
        save();print('Completed '+phase,flush=True)
        return expected
    save()
    with open('/tmp/stannum-pgrx.lock','a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        try:
            command('initdb','-D',str(data),'-A','trust')
            start()
            sql('CREATE EXTENSION stannum; CREATE EXTENSION pg_visibility;')
            # Update only extension-owned C functions in this disposable database.
            # Restart before invoking the baseline, so no backend has both libraries loaded.
            sql("UPDATE pg_proc SET probin='"+str(runtime).replace("'","''")+"' WHERE oid IN (SELECT objid FROM pg_depend WHERE refobjid=(SELECT oid FROM pg_extension WHERE extname='stannum') AND classid='pg_proc'::regclass AND deptype='e') AND prolang=(SELECT oid FROM pg_language WHERE lanname='c');")
            stop();start()
            sql('CREATE TABLE documents(id text NOT NULL,body text NOT NULL) WITH(autovacuum_enabled=false)')
            with args.input.open('rb') as stream:
                subprocess.run(['psql','-Xq','-v','ON_ERROR_STOP=1','-c','COPY documents FROM STDIN WITH(FORMAT csv)'],stdin=stream,env=env,check=True)
            assert int(sql('SELECT count(*) FROM documents'))==args.rows
            print('Building baseline index',flush=True)
            sql('CREATE INDEX documents_idx ON documents USING stannum(body)');sql('VACUUM ANALYZE documents')
            before=check('baseline',False)
            sql('CHECKPOINT');stop()
            control=command('pg_controldata',str(data))
            if 'shut down' not in control:raise ValueError('Not a clean shutdown')
            (out/'pg_controldata.txt').write_text(control+'\n')
            shutil.copytree(data,snapshot)
            # Restore physical files rather than just restart the original directory.
            shutil.rmtree(data);shutil.copytree(snapshot,data)
            shutil.copy2(libs/'candidate.dylib',runtime);start()
            restored=check('candidate-restored',True)
            if state['phases'][0]['fingerprint']!=state['phases'][1]['fingerprint']:raise ValueError('Physical baseline state changed on restore')
            if restored!=before:raise ValueError('Candidate changed baseline answers')
            with connect() as conn:
                if args.aws_mutations:
                    conn.execute("UPDATE documents SET id=id WHERE hashtextextended(id,0)%20=0")
                    conn.execute("UPDATE documents SET body=body || ' ' WHERE (hashtextextended(id,0) & 9223372036854775807)%10=1")
                    conn.execute('DELETE FROM documents WHERE hashtextextended(id,0)%101=0')
                else:
                    conn.execute('SET stannum.write_buffer_docs=64; SET stannum.merge_tier_factor=2')
                    conn.execute("UPDATE documents SET id=id WHERE hashtextextended(id,0)%20=0")
                    conn.execute("UPDATE documents SET body=body || ' database' WHERE hashtextextended(id,0)%100=1")
                    conn.execute('DELETE FROM documents WHERE hashtextextended(id,0)%101=0')
                    conn.execute("INSERT INTO documents SELECT 'snapshot-check-'||n, 'database postgres snapshot' FROM generate_series(1,1024) n")
            check('candidate-mutated',True)
            sql('CHECKPOINT');stop()
            shutil.copytree(data,out/'snapshot-mutated');start()
            sql('VACUUM ANALYZE documents');check('candidate-vacuumed',True)
            stop();start();check('candidate-restarted',True)
            # Existing focused suite asserts both strategies across two-session snapshots.
            with (out/'visibility.log').open('w') as log:
                subprocess.run([sys.executable,str(Path(__file__).with_name('count_visibility.py'))],env=env,check=True,stdout=log,stderr=subprocess.STDOUT)
            state['status']='complete'
        except BaseException as error:
            state.update(status='failed',error=repr(error));raise
        finally:
            if (data/'postmaster.pid').exists():stop()
            save()
    print('Complete: '+str(out),flush=True)


if __name__=='__main__':main()
