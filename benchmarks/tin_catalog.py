# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Controlled EXPLAIN observations on an existing TIN server (libpq environment)."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
import uuid


def cases(rows=1000):
    result = []
    queries = ['rare', 'p010', 'p050', 'p100', 'p101', 'p200', 'half', 'common',
               'common AND rare', 'common OR rare', 'half AND opposite',
               'half OR opposite', 'half AND p010', 'half OR p010',
               '"alpha beta"', '"alpha spacer beta"', 'p*', 'missing']
    for query in queries:
        result.append(dict(name='count:' + query, query=query, workload='count', limit=None, mode='literal'))
        for scoring in ('score', 'full_score'):
            result.append(dict(name=scoring + ':' + query, query=query, workload=scoring, limit=10, mode='literal'))
    for limit in (1, 100, 1000):
        result.append(dict(name=f'limit:{limit}', query='common OR rare', workload='full_score', limit=limit, mode='literal'))
    for mode in ('force_custom_plan', 'force_generic_plan', 'auto'):
        for query in ('rare', 'common OR rare', '"alpha beta"'):
            result.append(dict(name=mode + ':' + query, query=query, workload='full_score', limit=10, mode=mode))
    for predicate in ('tenant = 3', 'id > 990'):
        result.append(dict(name='filter:' + predicate, query='common OR rare', workload='full_score', limit=10, mode='literal', filter=predicate))
    for fraction in (.001, .01, .1, .5, .9):
        for limit in (10, 100):
            boundary = max(1, int(rows * fraction))
            result.append(dict(name=f'filter-sweep:{fraction}:{limit}', query='common OR rare',
                workload='full_score', limit=limit, mode='literal', filter=f'id <= {boundary}',
                filter_fraction=fraction))
    terms = ['rare','p010','p050','p100','p101','p200','half','opposite','common']
    for length in (2,4,8,9):
        result.append(dict(name=f'or-length:{length}', query=' OR '.join(terms[:length]),
            workload='full_score',limit=10,mode='literal'))
    for memory in ('64kB','2MB','8MB'):
        result.append(dict(name='work-mem:'+memory, query='common OR rare', workload='full_score',
            limit=1000,mode='literal',filter='tenant = 3',work_mem=memory))
    return result


def fixture_sql(schema, rows):
    terms = " || ".join(f"CASE WHEN n % 1000 < {cutoff} THEN 'p{cutoff:03d} ' ELSE '' END"
                        for cutoff in (10, 50, 100, 101, 200))
    return f"""CREATE TABLE {schema}.docs(id integer PRIMARY KEY, tenant integer, body text NOT NULL);
INSERT INTO {schema}.docs SELECT n, n % 10, repeat('common ', 1+n%7) ||
 CASE WHEN n%2=0 THEN 'half ' ELSE 'opposite ' END ||
 CASE WHEN n%1000=0 THEN 'rare ' ELSE '' END || {terms} ||
 CASE WHEN n%10=0 THEN 'alpha beta' ELSE 'alpha spacer beta' END
 FROM generate_series(1,{rows}) n;
CREATE INDEX docs_body_idx ON {schema}.docs USING tin(body);
CREATE INDEX docs_tenant_idx ON {schema}.docs(tenant);
ANALYZE {schema}.docs;"""


def query_sql(schema, case, parameter=False):
    argument = '$1' if parameter else "'" + case['query'].replace("'", "''") + "'"
    fields = 'count(*)' if case['workload'] == 'count' else f"id,body,tin.{case['workload']}(ctid) AS score"
    sql = f'SELECT {fields} FROM {schema}.docs WHERE body ==> {argument}'
    if case.get('filter'):
        sql += ' AND ' + case['filter']
    if case['limit'] is not None:
        sql += f" ORDER BY score DESC LIMIT {case['limit']}"
    return sql


def walk(plan):
    yield {k: v for k, v in plan.items() if k in (
        'Node Type', 'Custom Plan Provider', 'Index', 'Index Cond', 'Query', 'Scoring', 'Elided Terms',
        'Startup Cost', 'Total Cost', 'Shared Hit Blocks', 'Shared Read Blocks', 'Temp Read Blocks', 'Temp Written Blocks',
        'Top K', 'Predicted Work', 'Page Touches', 'Plan Rows', 'Actual Rows', 'Actual Loops',
        'Filter', 'Rows Removed by Filter', 'Sort Method', 'Join Type')}
    for child in plan.get('Plans', []):
        yield from walk(child)


def run(args):
    if any(n < 1000 or n > 50000 for n in args.rows) or len(set(args.rows)) != len(args.rows):
        raise ValueError('catalog row counts must be distinct and between 1000 and 50000')
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    # Never persist environment, DSN, role, hostname or database identifiers.
    env = dict(os.environ)
    env['PGOPTIONS'] = '-c statement_timeout=30000 -c lock_timeout=3000'
    def sql(text, timeout=45):
        result = subprocess.run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', text],
                                env=env, capture_output=True, text=True, timeout=timeout)
        if result.returncode:
            # libpq errors can echo connection details; do not retain stderr.
            raise RuntimeError(f'psql exited {result.returncode}; SQL retained, connection details omitted')
        return result.stdout.strip()
    def save(name, value):
        (root / name).write_text(json.dumps(value, indent=2) + '\n')
    source = Path(__file__).read_bytes()
    (root / 'tin_catalog.py').write_bytes(source)
    metadata = dict(status='running', source_sha256=hashlib.sha256(source).hexdigest(),
                    rows=args.rows, observations=[], fixture='nested-frequency-v1',
                    timing_policy='one instrumented observation per case; ordered warm-cache effects; not capacity')
    save('catalog.json', metadata)
    metadata['server'] = json.loads(sql("SELECT json_build_object('version',version(),'extensions',(SELECT json_object_agg(extname,extversion) FROM pg_extension),'settings',(SELECT json_object_agg(name,setting) FROM pg_settings WHERE name IN ('shared_buffers','work_mem','maintenance_work_mem','effective_cache_size','random_page_cost','seq_page_cost','max_parallel_workers_per_gather','max_parallel_workers','jit','plan_cache_mode','track_io_timing')));"))
    if 'tin' not in metadata['server']['extensions']:
        raise ValueError('server does not have the tin extension')
    try:
        for rows in args.rows:
            schema = 'stannum_probe_' + uuid.uuid4().hex[:16]
            owned = False
            try:
                sql(f'CREATE SCHEMA {schema};')
                owned = True
                fixture = fixture_sql(schema, rows)
                (root / f'fixture-{rows}.sql').write_text(fixture)
                started = time.monotonic()
                sql(fixture, timeout=120)
                metadata.setdefault('fixtures', []).append(dict(rows=rows, build_seconds=time.monotonic()-started,
                    sizes=json.loads(sql(f"SELECT json_build_object('index',pg_relation_size('{schema}.docs_body_idx'),'total',pg_total_relation_size('{schema}.docs'));"))))
                for i, case in enumerate(cases(rows)):
                    parameter = case['mode'] != 'literal'
                    statement = query_sql(schema, case, parameter)
                    if parameter:
                        argument = "'" + case['query'].replace("'", "''") + "'"
                        prefix = f"SET plan_cache_mode={case['mode']}; PREPARE probe(text) AS {statement};\n"
                        # Exercise auto past its first five custom executions in
                        # this same backend. Non-ANALYZE explains incur no traffic.
                        warm = ''.join(f'EXPLAIN EXECUTE probe({argument});\n' for _ in range(6)) if case['mode'] == 'auto' else ''
                        statement = prefix + warm + f'EXPLAIN (ANALYZE,BUFFERS,SETTINGS,FORMAT JSON,TIMING OFF) EXECUTE probe({argument});'
                    else:
                        statement = 'EXPLAIN (ANALYZE,BUFFERS,SETTINGS,FORMAT JSON,TIMING OFF) ' + statement
                    if case.get('work_mem'):
                        statement = f"SET work_mem='{case['work_mem']}';\n" + statement
                    name = f'{rows}-{i:03d}'
                    (root / (name + '.sql')).write_text(statement + '\n')
                    text = sql(statement)
                    # Auto's text warmups precede the final JSON document.
                    plan = json.loads(text[text.index('[\n'):])[0]
                    save(name + '.json', plan)
                    metadata['observations'].append(dict(rows=rows, case=case, artifact=name,
                        planning_ms=plan.get('Planning Time'), execution_ms=plan.get('Execution Time'),
                        nodes=list(walk(plan['Plan']))))
                    save('catalog.json', metadata)
            finally:
                if owned:
                    sql(f'DROP SCHEMA {schema} CASCADE;')
                    metadata.setdefault('cleaned_schemas', []).append(schema)
                    save('catalog.json', metadata)
        metadata['status'] = 'complete'
    except BaseException as error:
        metadata.update(status='failed', error=type(error).__name__ + ': ' + str(error))
        raise
    finally:
        save('catalog.json', metadata)
    print(root)
