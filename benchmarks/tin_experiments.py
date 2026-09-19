# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Bounded remote TIN experiments; connection secrets come only from libpq env.

Requires psycopg[binary]==3.3.6 for persistent sessions and streaming COPY.
Uses only uniquely owned schemas; all recorded SQL is generated fixture SQL.
"""
import concurrent.futures
import csv
import hashlib
import json
import math
from pathlib import Path
import random
import statistics
import threading
import time
import uuid

import dataset
import tin_catalog


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def percentile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)] if values else None


def ranked(table, query, limit=10, predicate=None, offset=0, scoring='full_score'):
    result = f'SELECT id,tin.{scoring}(ctid) AS score FROM {table} WHERE body ==> {literal(query)}'
    if predicate:
        result += ' AND ' + predicate
    return result + f' ORDER BY score DESC LIMIT {limit} OFFSET {offset}'


class Experiment:
    def __init__(self, args):
        import psycopg
        self.psycopg = psycopg
        self.args = args
        self.root = args.output.resolve()
        self.root.mkdir(parents=True, exist_ok=False)
        self.deadline = time.monotonic() + args.minutes * 60
        self.schema = 'stannum_probe_' + uuid.uuid4().hex[:16]
        self.number = 0
        self.meta = dict(status='running', schema=self.schema, rows=args.rows,
                         repetitions=args.repetitions, seconds=args.seconds,
                         max_clients=args.max_clients, stages=getattr(args,'stages',None), index_segments=getattr(args,'index_segments',None), build_memory_mb=getattr(args,'build_memory_mb',None), observations=[], events=[],
                         timing_policy='instrumented server execution times; client concurrency includes network latency')
        for module in (Path(__file__), Path(tin_catalog.__file__), Path(dataset.__file__)):
            (self.root / module.name).write_bytes(module.read_bytes())
        self.meta['collector_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
        self.save()

    def connect(self):
        return self.psycopg.connect('', autocommit=True, prepare_threshold=None,
            connect_timeout=15, application_name='stannum_tin_experiment',
            options='-c statement_timeout=60000 -c lock_timeout=3000 -c idle_in_transaction_session_timeout=60000')

    def save(self):
        temp = self.root / 'experiment.tmp'
        temp.write_text(json.dumps(self.meta, indent=2) + '\n')
        temp.replace(self.root / 'experiment.json')

    def event(self, name, **details):
        self.meta['events'].append(dict(name=name, **details))
        self.save()
        print(name, details, flush=True)

    def check_deadline(self):
        if time.monotonic() >= self.deadline:
            raise TimeoutError('experiment wall-clock budget reached')

    def execute(self, conn, name, statement):
        self.check_deadline()
        self.number += 1
        (self.root / f'{self.number:05d}-{name}.sql').write_text(statement + ';\n')
        started = time.monotonic()
        conn.execute(statement)
        self.event(name, seconds=time.monotonic() - started)

    def probe(self, conn, name, statement, settings=None, serialize=False):
        self.check_deadline()
        self.number += 1
        key = f'{self.number:05d}'
        settings = settings or {}
        sql = 'EXPLAIN (ANALYZE,BUFFERS,SETTINGS,FORMAT JSON,TIMING OFF' + (',SERIALIZE TEXT' if serialize else '') + ') ' + statement
        (self.root / (key + '.sql')).write_text(
            ''.join(f'SET {k}={literal(v)};\n' for k,v in settings.items()) + sql + ';\n')
        result = dict(name=name, artifact=key, settings=settings, serialize=serialize)
        previous = {}
        try:
            for k,v in settings.items():
                previous[k] = conn.execute('SELECT current_setting(%s)', (k,)).fetchone()[0]
                conn.execute('SELECT set_config(%s,%s,false)', (k,v))
            start = time.monotonic()
            plan = conn.execute(sql).fetchone()[0][0]
            result.update(client_ms=1000*(time.monotonic()-start),
                          execution_ms=plan.get('Execution Time'), planning_ms=plan.get('Planning Time'),
                          nodes=list(tin_catalog.walk(plan['Plan'])))
            (self.root / (key + '.json')).write_text(json.dumps(plan, indent=2) + '\n')
        except self.psycopg.Error as error:
            result.update(error=type(error).__name__, sqlstate=error.sqlstate)
        finally:
            for k,v in previous.items():
                conn.execute('SELECT set_config(%s,%s,false)', (k,v))
        self.meta['observations'].append(result)
        self.save()
        return result

    def sizes(self, conn, table):
        return conn.execute("SELECT json_build_object('heap',pg_relation_size(%s),"
            "'indexes',pg_indexes_size(%s),'total',pg_total_relation_size(%s),"
            "'index_options',(SELECT json_object_agg(c.relname,c.reloptions) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE i.indrelid=%s::regclass))", (table,table,table,table)).fetchone()[0]

    def synthetic(self, conn):
        table = self.schema + '.docs'
        self.execute(conn, 'synthetic-build', tin_catalog.fixture_sql(self.schema, 100000))
        self.event('synthetic-sizes', **self.sizes(conn, table))
        for memory in (None,'2MB','64kB'):
            for fraction in (.001,.01,.05,.1,.15,.2,.25,.3,.4,.5,.75,.9):
                for limit in (1,10,100,1000):
                    self.probe(conn, f'crossover:{memory}:{fraction}:{limit}',
                        ranked(table, 'common OR rare', limit, f'id <= {int(100000*fraction)}'),
                        {'work_mem':memory} if memory else {})
        for query in ('common', 'common OR rare', 'half OR opposite', 'half AND opposite',
                      '"alpha beta"', 'alpha NEAR/3 beta', 'p*', 'raer~1',
                      'common AND NOT rare', '(half OR rare) AND p010'):
            for scoring in ('score','full_score'):
                self.probe(conn, f'synthetic:{scoring}:{query}', ranked(table,query,scoring=scoring))
        for predicate in ('id <= 10000','tenant = 3'):
            for limit in (10,100,1000):
                for i in range(3):
                    self.probe(conn,f'locality:{predicate}:{limit}:r{i}',ranked(table,'common OR rare',limit,predicate),{'work_mem':'2MB'})
        for offset in (0,100,1000,10000):
            self.probe(conn, f'offset:{offset}',ranked(table,'common OR rare',offset=offset))
        self.prepared(conn, table, ('rare','common','common OR rare','"alpha beta"'))
        self.forced_strategies(conn, table, 'common OR rare', 100000)
        self.execute(conn, 'synthetic-drop', f'DROP TABLE {table}')

    def prepared(self, conn, table, queries):
        for mode in ('auto','force_custom_plan','force_generic_plan'):
            for history in ('rare-first','common-first'):
                for dynamic_limit in (False,True):
                    conn.execute("SELECT set_config('plan_cache_mode',%s,false)",(mode,))
                    types='text,integer,integer' if dynamic_limit else 'text,integer'
                    limit='$3' if dynamic_limit else '10'
                    prepare=f'PREPARE ranked_probe({types}) AS SELECT id,tin.full_score(ctid) AS score FROM {table} WHERE body ==> $1 AND id <= $2 ORDER BY score DESC LIMIT {limit}'
                    conn.execute(prepare)
                    try:
                        for i in range(16):
                            query=queries[0 if history=='rare-first' else 1] if i<5 else queries[i%len(queries)]
                            bound=max(1,self.args.rows//100) if i>=5 and i%2 else self.args.rows
                            limit_value=100 if i%2 else 10
                            arguments=f'{literal(query)},{bound}'+(f',{limit_value}' if dynamic_limit else '')
                            statement=f'EXECUTE ranked_probe({arguments})'
                            result=self.probe(conn,f'prepared:{table.rsplit(".",1)[-1]}:{mode}:{history}:dynamic-limit={dynamic_limit}:{i}:{query}:{bound}',statement)
                            result['prepare_sql']=prepare
                            self.save()
                        counts=conn.execute("SELECT generic_plans,custom_plans FROM pg_prepared_statements WHERE name='ranked_probe'").fetchone()
                        self.event('prepared-counts',table=table.rsplit('.',1)[-1],mode=mode,history=history,dynamic_limit=dynamic_limit,generic=counts[0],custom=counts[1])
                    finally:
                        conn.execute('DEALLOCATE ranked_probe')
                        conn.execute('SET plan_cache_mode=auto')

    def forced_strategies(self, conn, table, query, rows):
        choices = [{}, {'tin.debug_force_topk':'TopK'}, {'tin.debug_force_topk':'Exhaustive'},
                   {'tin.debug_force_conjunction_mode':'Generic'},
                   {'tin.debug_force_conjunction_mode':'Pushdown'},
                   {'tin.debug_force_conjunction_mode':'NoAdaptive'},
                   {'tin.debug_disable_index_probe':'on'}]
        for fraction in (.01,.1,.25):
            for limit in (10,100):
                statement=ranked(table,query,limit,f'id <= {max(1,int(rows*fraction))}')
                baseline=conn.execute(statement).fetchall()
                expected=[r[1] for r in baseline]
                for repetition in range(3):
                    order=list(choices)
                    random.Random(410+repetition).shuffle(order)
                    for setting in order:
                        config={'work_mem':'2MB',**setting}
                        name=f'forced:{table.rsplit(".",1)[-1]}:{fraction}:{limit}:r{repetition}:{setting}'
                        result=self.probe(conn,name,statement,config)
                        if repetition==0 and not result.get('error'):
                            old={k:conn.execute('SELECT current_setting(%s)',(k,)).fetchone()[0] for k in config}
                            try:
                                for k,v in config.items(): conn.execute('SELECT set_config(%s,%s,false)',(k,v))
                                actual=conn.execute(statement).fetchall()
                                result['same_score_sequence_as_auto']=[r[1] for r in actual]==expected
                                result['returned_rows']=len(actual)
                                if not result['same_score_sequence_as_auto']:
                                    (self.root/(result['artifact']+'-result-difference.json')).write_text(json.dumps(dict(auto=baseline,forced=actual)))
                            finally:
                                for k,v in old.items(): conn.execute('SELECT set_config(%s,%s,false)',(k,v))
                            self.save()
        count=f'SELECT count(*) FROM {table} WHERE body ==> {literal(query)}'
        for config in ({},{'tin.debug_disable_count_pushdown':'on'},
                       {'tin.debug_force_visibility':'Streaming'}, {'tin.debug_force_visibility':'Sorted'},
                       {'tin.debug_force_parallel':'on'}, {'tin.track_page_reuse_stats':'on'}):
            for repetition in range(3): self.probe(conn,f'count-strategy:{table.rsplit(".",1)[-1]}:{config}:r{repetition}',count,config)

    def load_wikipedia(self, conn):
        manifest = dataset.verify(self.args.dataset)
        if self.args.rows > manifest['rows']:
            raise ValueError('requested rows exceed immutable dataset')
        self.meta['dataset'] = manifest
        self.save()
        table = self.schema + '.wiki'
        self.execute(conn,'wiki-create',f'CREATE TABLE {table}(id integer PRIMARY KEY,body text NOT NULL)')
        conn.execute("SET statement_timeout='300s'")
        start = time.monotonic()
        count = 0
        with conn.cursor() as cur, cur.copy(f'COPY {table}(id,body) FROM STDIN') as copy:
            with (self.args.dataset/'documents.csv').open(newline='') as source:
                for row in csv.reader(source):
                    if count >= self.args.rows:
                        break
                    copy.write_row((int(row[0]),row[1]))
                    count += 1
        self.event('wiki-copy',seconds=time.monotonic()-start,rows=count)
        if count != self.args.rows:
            raise ValueError('short dataset')
        memory=getattr(self.args,'build_memory_mb',None)
        if memory is not None: conn.execute("SELECT set_config('maintenance_work_mem',%s,false)",(str(memory)+'MB',))
        segments=getattr(self.args,'index_segments',None)
        options=f' WITH (initial_segment_count={segments},target_segment_count={segments})' if segments else ''
        self.execute(conn,'wiki-index',f'CREATE INDEX wiki_body_idx ON {table} USING tin(body)'+options)
        self.execute(conn,'wiki-analyze',f'ANALYZE {table}')
        conn.execute("SET statement_timeout='60s'")
        self.event('wiki-sizes',**self.sizes(conn,table))
        self.segment_snapshot(conn, table, 'built')
        return table

    def segment_snapshot(self, conn, table, phase):
        index=table+'_body_idx'
        cursor=conn.execute('SELECT * FROM tin.segment_info(%s::regclass)',(index,))
        columns=[c.name for c in cursor.description]
        rows=[dict(zip(columns,row)) for row in cursor.fetchall()]
        (self.root/f'segments-{phase}.json').write_text(json.dumps(rows,indent=2)+'\n')
        self.event('segments',phase=phase,segments=len(rows),docs=sum(r['docs'] for r in rows),dead_docs=sum(r['dead_docs'] for r in rows),pages=sum(r['total_pages'] for r in rows))

    def wiki_queries(self, table):
        queries = ['history','telescope','quasar','war AND history','telescope OR astronomy',
                   '"united states"','"computer science"','"quantum mechanics"',
                   'united NEAR/3 states','comput*','telescoep~2',
                   'history OR war OR science OR music OR art OR government',
                   '(history OR war) AND (science OR music)', 'zzleadmissingtoken']
        cases=[]
        for query in queries:
            cases.append(('count:'+query, f'SELECT count(*) FROM {table} WHERE body ==> {literal(query)}'))
            for score in ('score','full_score'):
                cases.append((score+':'+query,ranked(table,query,scoring=score)))
        for fraction in (.001,.01,.1,.25,.5,.9):
            for limit in (10,100,1000):
                cases.append((f'wiki-filter:{fraction}:{limit}',ranked(table,'history OR war',limit,f'id <= {max(1,int(self.args.rows*fraction))}')))
        for offset in (100,1000,10000):
            cases.append((f'wiki-offset:{offset}',ranked(table,'history OR war',offset=offset)))
        cases.append(('lateral-parameters',f"SELECT q.query,r.id,r.score FROM (VALUES ('quasar'),('history'),('history OR war')) q(query) CROSS JOIN LATERAL (SELECT id,tin.full_score(ctid) AS score FROM {table} WHERE body ==> q.query ORDER BY score DESC LIMIT 10) r"))
        cases.append(('secondary-order',ranked(table,'history OR war').replace('score DESC','score DESC,id ASC')))
        cases.append(('ascending-score',ranked(table,'history OR war').replace('score DESC','score ASC')))
        cases.append(("join-filter",f"SELECT w.id,tin.full_score(w.ctid) AS score FROM {table} w JOIN {self.schema}.allowed a ON w.id=a.id WHERE w.body ==> 'history OR war' ORDER BY score DESC LIMIT 10"))
        return cases

    def concurrency(self, table):
        queries = [ranked(table,'history OR war'),ranked(table,'"united states"'),
                   ranked(table,'history OR war',predicate=f'id <= {max(1,self.args.rows//100)}'),
                   f"SELECT count(*) FROM {table} WHERE body ==> 'history OR war'"]
        (self.root/'concurrency-queries.json').write_text(json.dumps(queries,indent=2)+'\n')
        for repetition in range(2):
            levels = [n for n in (1,2,4,8) if n <= self.args.max_clients]
            if repetition: levels.reverse()
            for clients in levels:
                self.check_deadline()
                barrier = threading.Barrier(clients,timeout=30)
                def worker(worker_id):
                    values=[];errors=[]
                    try:
                        with self.connect() as conn:
                            conn.execute('SELECT 1').fetchone()
                            barrier.wait()
                            until=min(time.monotonic()+self.args.seconds,self.deadline)
                            i=worker_id
                            while time.monotonic()<until:
                                start=time.monotonic()
                                try:
                                    rows=conn.execute(queries[i%len(queries)]).fetchall()
                                    values.append(dict(query=i%len(queries),ms=1000*(time.monotonic()-start),rows=len(rows)))
                                except self.psycopg.Error as error:
                                    errors.append(dict(sqlstate=error.sqlstate,error=type(error).__name__))
                                    break
                                i+=1
                    except Exception as error:
                        barrier.abort()
                        errors.append(dict(error=type(error).__name__))
                    return dict(samples=values,errors=errors)
                started=time.monotonic()
                with concurrent.futures.ThreadPoolExecutor(max_workers=clients) as pool:
                    results=list(pool.map(worker,range(clients)))
                elapsed=time.monotonic()-started
                durations=[x['ms'] for r in results for x in r['samples']]
                record=dict(repetition=repetition,clients=clients,wall_seconds=elapsed,
                            completed=len(durations),qps_including_connection_setup=len(durations)/elapsed,
                            p50_ms=percentile(durations,.5),p95_ms=percentile(durations,.95),p99_ms=percentile(durations,.99),workers=results)
                (self.root/f'concurrency-{repetition}-{clients}.json').write_text(json.dumps(record,indent=2)+'\n')
                self.event('concurrency',**{k:v for k,v in record.items() if k!='workers'},errors=sum(len(r['errors']) for r in results))

    def projection(self,conn,table):
        for query in ('history OR war','quasar','"united states"'):
            for limit in (10,1000):
                for repetition in range(3):
                    variants=[(False,False),(True,False),(False,True),(True,True)]
                    random.Random(99+repetition).shuffle(variants)
                    for body,serialize in variants:
                        statement=ranked(table,query,limit)
                        if body: statement=statement.replace('SELECT id,','SELECT id,body,',1)
                        self.probe(conn,f'projection:{query}:{limit}:body={body}:serialize={serialize}:r{repetition}',statement,serialize=serialize)

    def multi_index(self,conn,table):
        target=self.schema+'.multi'
        self.execute(conn,'multi-build',f'CREATE TABLE {target} AS SELECT id,left(body,120) AS title,body FROM {table} WHERE id <= 10000; CREATE INDEX multi_title_idx ON {target} USING tin(title); CREATE INDEX multi_body_idx ON {target} USING tin(body); ANALYZE {target}')
        predicates=["title ==> 'history' AND body ==> 'war'",
                    "title ==> 'history' OR body ==> 'war'",
                    "title ==> 'science OR computer' AND body ==> 'history OR war'",
                    "(title ==> 'science' AND body ==> 'computer') OR (title ==> 'history' AND body ==> 'war')",
                    "title ==> 'quasar' OR body ==> 'history'",
                    "title ==> 'history' AND body ==> 'war' AND id <= 1000"]
        choices=[{}, {'tin.debug_force_boolean_family':'Fused'}, {'tin.debug_force_boolean_family':'Factored'},
                 {'tin.debug_force_multi_index_drive':'Sparse'}, {'tin.debug_force_multi_index_drive':'Stripe'}]
        for number,predicate in enumerate(predicates):
            ids=[r[0] for r in conn.execute(f'SELECT id FROM {target} WHERE {predicate} ORDER BY id').fetchall()]
            expected=hashlib.sha256(json.dumps(ids).encode()).hexdigest()
            count=f'SELECT count(*) FROM {target} WHERE {predicate}'
            for setting in choices:
                for repetition in range(3):
                    result=self.probe(conn,f'multi:{number}:{setting}:r{repetition}',count,setting)
                    if repetition==0 and not result.get('error'):
                        old={k:conn.execute('SELECT current_setting(%s)',(k,)).fetchone()[0] for k in setting}
                        try:
                            for k,v in setting.items(): conn.execute('SELECT set_config(%s,%s,false)',(k,v))
                            actual=[r[0] for r in conn.execute(f'SELECT id FROM {target} WHERE {predicate} ORDER BY id').fetchall()]
                            actual_count=conn.execute(count).fetchone()[0]
                            result['same_membership_as_auto']=ids==actual
                            result['same_count_as_auto']=actual_count==len(ids)
                            result['auto_membership_sha256']=expected
                            self.save()
                        finally:
                            for k,v in old.items(): conn.execute('SELECT set_config(%s,%s,false)',(k,v))
        self.event('multi-complete',cases=len(predicates)*len(choices)*3)

    def maintenance(self,conn,table):
        query="history OR war"
        for stage in ('before','deleted','updated','vacuumed'):
            if stage=='deleted':
                self.execute(conn,'delete-ten-percent',f'DELETE FROM {table} WHERE id % 10=0')
            elif stage=='updated':
                self.execute(conn,'update-five-percent',f"UPDATE {table} SET body=body || ' stannummutatedtoken' WHERE id % 20=1")
            elif stage=='vacuumed':
                conn.execute("SET statement_timeout='300s'")
                self.execute(conn,'vacuum',f'VACUUM (ANALYZE) {table}')
                conn.execute("SET statement_timeout='60s'")
            for i in range(3):
                self.probe(conn,f'maintenance:{stage}:{i}',ranked(table,query))
            count=conn.execute(f"SELECT count(*) FROM {table} WHERE body ==> 'stannummutatedtoken'").fetchone()[0]
            self.event('maintenance-state',stage=stage,mutated_matches=count,sizes=self.sizes(conn,table))
            self.segment_snapshot(conn,table,stage)
        conn.execute("SET statement_timeout='300s'")
        self.execute(conn,'reindex',f'REINDEX INDEX {table}_body_idx')
        conn.execute("SET statement_timeout='60s'")
        self.segment_snapshot(conn,table,'reindexed')
        for i in range(3): self.probe(conn,f'maintenance:reindexed:{i}',ranked(table,query))
        started=time.monotonic()
        conn.execute("SET statement_timeout='300s'")
        checks=conn.execute('SELECT * FROM tin.fsck(%s::regclass,true)',(table+'_body_idx',)).fetchall()
        (self.root/'fsck.json').write_text(json.dumps(checks,indent=2)+'\n')
        self.event('fsck',reported_errors=len(checks),seconds=time.monotonic()-started)

    def run(self):
        owned=False
        try:
            with self.connect() as conn:
                self.meta['server']=conn.execute("SELECT json_build_object('version',version(),'extensions',(SELECT json_object_agg(extname,extversion) FROM pg_extension),'settings',(SELECT json_object_agg(name,setting) FROM pg_settings WHERE name IN ('shared_buffers','work_mem','maintenance_work_mem','effective_cache_size','random_page_cost','seq_page_cost','max_parallel_workers_per_gather','jit','plan_cache_mode','track_io_timing')))").fetchone()[0]
                if 'tin' not in self.meta['server']['extensions']:
                    raise ValueError('TIN must already be installed')
                conn.execute("SELECT tin.ql_parse('history')")
                self.meta['tin_settings']=conn.execute("SELECT name,setting,enumvals FROM pg_settings WHERE name LIKE 'tin.%' ORDER BY name").fetchall()
                conn.execute(f'CREATE SCHEMA {self.schema}')
                owned=True
                stages=set(getattr(self.args,'stages',None) or ('synthetic','queries','prepared','forced','concurrency','multi','maintenance','projection'))
                if 'synthetic' in stages and not self.args.skip_synthetic:
                    self.synthetic(conn)
                table=self.load_wikipedia(conn)
                if 'queries' in stages:
                    self.execute(conn,'allowed-build',f'CREATE TABLE {self.schema}.allowed AS SELECT id FROM {table} WHERE id % 100=1; CREATE UNIQUE INDEX ON {self.schema}.allowed(id); ANALYZE {self.schema}.allowed')
                    cases=self.wiki_queries(table)
                    for repetition in range(self.args.repetitions):
                        shuffled=list(cases)
                        random.Random(1729+repetition).shuffle(shuffled)
                        for name,statement in shuffled:
                            self.probe(conn,f'wiki:r{repetition}:{name}',statement)
                        self.event('wiki-repetition',repetition=repetition)
                if 'prepared' in stages: self.prepared(conn,table,('quasar','history','history OR war','"united states"'))
                if 'forced' in stages: self.forced_strategies(conn,table,'history OR war',self.args.rows)
                if 'projection' in stages: self.projection(conn,table)
                if 'concurrency' in stages: self.concurrency(table)
                if 'multi' in stages: self.multi_index(conn,table)
                if 'maintenance' in stages: self.maintenance(conn,table)
            failures=sum(any(o.get(k) is False for k in ('same_score_sequence_as_auto','same_membership_as_auto','same_count_as_auto')) for o in self.meta['observations'])
            failures+=sum(e.get('reported_errors',0) for e in self.meta['events'] if e['name']=='fsck')
            self.meta.update(status='validation-failed' if failures else 'complete',validation_failures=failures,query_error_count=sum('error' in o for o in self.meta['observations']))
        except BaseException as error:
            self.meta.update(status='failed',error=type(error).__name__)
            if isinstance(error,self.psycopg.Error): self.meta['sqlstate']=error.sqlstate
            # Exception strings from connection libraries may include secrets.
            self.event('experiment-failed',error=type(error).__name__)
        finally:
            if owned:
                try:
                    with self.connect() as cleanup:
                        cleanup.execute(f'DROP SCHEMA {self.schema} CASCADE')
                        self.meta['remaining_owned_schemas']=cleanup.execute('SELECT count(*) FROM pg_namespace WHERE nspname=%s',(self.schema,)).fetchone()[0]
                    self.meta['cleaned']=self.meta['remaining_owned_schemas']==0
                except Exception as error:
                    self.meta.update(cleaned=False,cleanup_error=type(error).__name__)
            self.save()
        print(self.root,flush=True)
        if self.meta['status']!='complete' or not self.meta.get('cleaned'):
            raise SystemExit(1)


def run(args):
    if not 1000 <= args.rows <= 1000000 or not 1 <= args.minutes <= 110:
        raise ValueError('rows must be 1k..1m and time budget 1..110 minutes')
    if not 1 <= args.max_clients <= 8 or not 1 <= args.seconds <= 60 or not 1 <= args.repetitions <= 5:
        raise ValueError('invalid bounded workload configuration')
    if getattr(args,'index_segments',None) not in (None,1,2,4,8):
        raise ValueError('index segments must be 1, 2, 4 or 8')
    if getattr(args,'build_memory_mb',None) not in (None,16,64,256,512):
        raise ValueError('unsupported build memory budget')
    Experiment(args).run()
