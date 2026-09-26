#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Deterministic query-shape suite using the existing server-side timer.

Serial mode uses session-local tables; concurrent mode uses a private schema.
Requires an installed stannum or tin extension.
The synthetic corpus tests query complexity, not production capacity.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import run as recorder

import server_times

WORDS = [f'word{chr(97 + i // 26)}{chr(97 + i % 26)}' for i in range(128)]


def literal(value):
    return "'" + value.replace("'", "''") + "'"


def has(word):
    return f"strpos(' ' || d.body || ' ', {literal(' ' + word + ' ')}) > 0"


def expressions():
    result = []
    for size in (2, 8, 32, 128):
        for op in ('AND', 'OR'):
            words = WORDS[:size]
            result.append((f'{op.lower()}_{size}', f' {op} '.join(words),
                           '(' + f' {op} '.join(has(w) for w in words) + ')'))
    a, b, c, d = WORDS[:4]
    result += [
        ('nested', f'({a} OR {b}) AND ({c} OR {d})',
         f'({has(a)} OR {has(b)}) AND ({has(c)} OR {has(d)})'),
        ('exclude', f'({a} OR {b}) AND NOT ({c} OR {d})',
         f'({has(a)} OR {has(b)}) AND NOT ({has(c)} OR {has(d)})'),
        ('redundant', f'{a} OR ({a} AND {b})', has(a)),
        ('contradiction', f'{a} AND NOT {a}', 'false'),
        ('phrase', f'"{a} {b}"', has(a + ' ' + b)),
        ('phrase_or', f'"{a} {b}" OR {c}', f'{has(a + " " + b)} OR {has(c)}'),
        ('prefix', 'worda*', '(' + ' OR '.join(has(w) for w in WORDS[:26]) + ')'),
        ('rare', 'raretoken', has('raretoken')),
        ('common_rare', f'{a} OR raretoken', f'{has(a)} OR {has("raretoken")}'),
        ('absent', 'missingtoken', 'false'),
    ]
    return result


def fixture(rows, engine, temporary=True):
    if engine not in ('stannum', 'tin') or rows < 128:
        raise ValueError('engine must be stannum/tin and rows >= 128')
    table = 'TEMP TABLE' if temporary else 'TABLE'
    vocabulary = ','.join(literal(w) for w in WORDS)
    return [
        'SET statement_timeout=120000', 'SET jit=off',
        f"CREATE {table} shape_documents(id bigint PRIMARY KEY, body text NOT NULL)",
        f"INSERT INTO shape_documents SELECT n, string_agg(w, ' ' ORDER BY j) || "
        "CASE WHEN n % 97 = 0 THEN ' raretoken' ELSE '' END "
        f"FROM generate_series(1,{rows}) n CROSS JOIN unnest(ARRAY[{vocabulary}]) "
        "WITH ORDINALITY words(w,j) WHERE n % 17 = 0 OR (n*31+j*7)%19 < 5 GROUP BY n",
        f'CREATE INDEX shape_search ON shape_documents USING {engine}(body)',
        f'CREATE {table} shape_allowed(id bigint PRIMARY KEY)',
        f'INSERT INTO shape_allowed SELECT n FROM generate_series(1,{rows}) n WHERE n % 7 = 0',
        'ANALYZE shape_documents', 'ANALYZE shape_allowed',
    ]


def catalog(engine):
    result = []
    for name, query, predicate in expressions():
        match = f'd.body ==> {literal(query)}'
        score = f'{engine}.full_score(d.ctid)'
        oracle = (f'WITH reference AS MATERIALIZED (SELECT d.id, d.body, {score} AS score '
                  f'FROM shape_documents d WHERE {match}) ')
        membership = (f"SELECT id FROM shape_documents d WHERE {match}",
                      f"SELECT id FROM shape_documents d WHERE {predicate}")
        for mode in ('count', 'ranked'):
            projection = 'count(*) AS n' if mode == 'count' else f'd.id, {score} AS score'
            suffix = '' if mode == 'count' else ' ORDER BY score DESC LIMIT 10'
            sql = f'SELECT {projection} FROM shape_documents d WHERE {match}{suffix}'
            reference = (f'SELECT count(*) AS n FROM shape_documents d WHERE {predicate}' if mode == 'count'
                         else oracle + 'SELECT id,score FROM reference ORDER BY score DESC LIMIT 10')
            result.append(dict(name=f'{name}_{mode}', family=name, mode=mode, query=query,
                               sql=sql, reference=reference, membership=membership))
        if name == 'or_8':
            filters = {
                'exists': 'EXISTS (SELECT 1 FROM shape_allowed a WHERE a.id=d.id)',
                'not_exists': 'NOT EXISTS (SELECT 1 FROM shape_allowed a WHERE a.id=d.id)',
                'correlated': 'd.id <= (SELECT min(a.id)+20 FROM shape_allowed a WHERE a.id >= d.id)',
                'sparse': 'd.id % 101 = 0',
            }
            for family, condition in filters.items():
                result.append(dict(name=f'{family}_ranked', family=family, mode='ranked', query=query,
                    sql=f'SELECT d.id,{score} AS score FROM shape_documents d WHERE ({match}) AND ({condition}) ORDER BY score DESC LIMIT 10',
                    reference=oracle + f'SELECT d.id,d.score FROM reference d WHERE {condition} ORDER BY score DESC LIMIT 10', membership=membership))
            result.append(dict(name='join_ranked', family='join', mode='ranked', query=query,
                sql=f'SELECT d.id,{score} AS score FROM shape_documents d JOIN shape_allowed a USING(id) WHERE {match} ORDER BY score DESC LIMIT 10',
                reference=oracle + 'SELECT d.id,d.score FROM reference d JOIN shape_allowed a USING(id) ORDER BY score DESC LIMIT 10', membership=membership))
            result.append(dict(name='materialized_ranked', family='materialized', mode='ranked', query=query,
                sql=oracle + 'SELECT id,score FROM reference WHERE id % 7 = 0 ORDER BY score DESC LIMIT 10',
                reference=oracle + 'SELECT id,score FROM reference WHERE id % 7 = 0 ORDER BY score DESC LIMIT 10', membership=membership))
            for kind in ('lateral', 'union'):
                if kind == 'lateral':
                    form = "SELECT sum(n) AS n FROM generate_series(0,6) g CROSS JOIN LATERAL (SELECT count(*) n FROM shape_documents d WHERE ({condition}) AND d.id % 7 = g) s"
                else:
                    form = "SELECT sum(n) AS n FROM (SELECT count(*) n FROM shape_documents d WHERE ({condition}) AND id % 2=0 UNION ALL SELECT count(*) n FROM shape_documents d WHERE ({condition}) AND id % 2=1) s"
                result.append(dict(name=f'{kind}_count',family=kind,mode='count',query=query,
                    sql=form.format(condition=match), reference=form.format(condition=predicate),membership=membership))
    return result


def difference(left, right):
    return f'(({left}) EXCEPT ALL ({right})) UNION ALL (({right}) EXCEPT ALL ({left}))'


def checks(case, compare_scores=True):
    # Materialize each original statement separately: wrapping a ranked query
    # inside EXCEPT/PLpgSQL expression planning can change scorer binding.
    statements = [f"CREATE TEMP TABLE shape_actual AS {case['sql']}"]
    if compare_scores:
        statements.append(f"CREATE TEMP TABLE shape_reference AS {case['reference']}")
    left, right = case['membership']
    statements.append(f"IF EXISTS ({difference(left, right)}) THEN RAISE EXCEPTION 'membership mismatch'; END IF")
    if case['mode'] == 'ranked':
        left = "SELECT encode(float4send(score),'hex') FROM shape_actual"
        right = "SELECT encode(float4send(score),'hex') FROM shape_reference"
        statements.append("IF EXISTS (SELECT id FROM shape_actual GROUP BY id HAVING count(*)>1) THEN RAISE EXCEPTION 'duplicate ranked IDs'; END IF")
        statements.append(f"IF EXISTS ((SELECT id FROM shape_actual) EXCEPT ({case['membership'][1]})) THEN RAISE EXCEPTION 'ranked membership mismatch'; END IF")
    else:
        left, right = 'TABLE shape_actual', 'TABLE shape_reference'
    if compare_scores:
        statements.append(f"IF EXISTS ({difference(left, right)}) THEN RAISE EXCEPTION 'result mismatch'; END IF")
        statements.append('DROP TABLE shape_reference')
    else:
        if case['mode'] != 'ranked':
            raise ValueError('live membership-only validation requires ranked cases')
        statements.append(f"IF (SELECT count(*) FROM shape_actual) <> "
                          f"(SELECT least(10,count(*)) FROM ({case['membership'][1]}) expected) "
                          "THEN RAISE EXCEPTION 'ranked cardinality mismatch'; END IF")
        statements.append("IF EXISTS (SELECT 1 FROM shape_actual WHERE score IS NULL OR "
                          "score::text IN ('NaN','Infinity','-Infinity')) "
                          "THEN RAISE EXCEPTION 'invalid score'; END IF")
    statements.append('DROP TABLE shape_actual')
    return '; '.join(statements) + ';'


def validate(cases, setup, env, compare_scores=True):
    script = setup + ["""SELECT jsonb_build_object('metadata',jsonb_build_object(
 'version',version(),
 'extensions',(SELECT jsonb_object_agg(extname,extversion) FROM pg_extension),
 'settings',(SELECT jsonb_object_agg(name,setting) FROM pg_settings WHERE name IN
 ('shared_buffers','work_mem','jit','max_parallel_workers_per_gather','block_size')),
 'fixture_md5',(SELECT md5(string_agg(id::text || ':' || body,E'\\n' ORDER BY id)) FROM shape_documents)))""", """CREATE FUNCTION pg_temp.shape_check(name text, statements text)
RETURNS jsonb LANGUAGE plpgsql AS $$ BEGIN
 EXECUTE statements;
 RETURN jsonb_build_object('case',name,'status','passed');
EXCEPTION WHEN OTHERS THEN
 RETURN jsonb_build_object('case',name,'status','failed','sqlstate',SQLSTATE,'error',SQLERRM);
END $$"""]
    for case in cases:
        block = 'DO $verify$ BEGIN ' + checks(case, compare_scores=compare_scores) + ' END $verify$'
        script.append(f"SELECT pg_temp.shape_check({literal(case['name'])},{literal(block)})")
    response = subprocess.run(['psql','-XqAt','-v','ON_ERROR_STOP=1'],
                              input=';\n'.join(script)+';',env=env,text=True,capture_output=True)
    if response.returncode:
        raise RuntimeError(response.stderr.strip())
    results = [json.loads(line) for line in response.stdout.splitlines() if line.startswith('{')]
    metadata = results.pop(0)['metadata']
    if [r.get('case') for r in results] != [c['name'] for c in cases]:
        raise RuntimeError('incomplete correctness results')
    return results, metadata


def nodes(node):
    yield node
    for child in node.get('Plans', []):
        yield from nodes(child)


def concurrent(args, cases):
    """Use the existing pgbench recorder against a private, cross-session schema."""
    schema = 'shape_' + uuid.uuid4().hex
    env = dict(os.environ)
    env['PGOPTIONS'] = (env.get('PGOPTIONS', '') +
                        f' -c search_path={schema},public -c jit=off -c statement_timeout=120000')
    selected = [c for c in cases if c['name'] in
                ('or_2_ranked', 'or_32_ranked', 'or_128_ranked')]
    setup = fixture(args.rows, args.engine, temporary=False)
    processes = []
    handles = []
    results = {'sources': {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                           for p in (Path(__file__), Path(recorder.__file__))},
               'rows': args.rows, 'readers': args.readers, 'write_rate': args.write_rate,
               'seconds': args.seconds, 'queries': selected,
               'caveat': 'Local pgbench client latency includes transport. Closed-loop readers; '
                         'one rate-limited match-changing UPDATE writer. Concurrent membership '
                         'oracle adds load. Synthetic fixture; not a production capacity claim.'}
    def save():
        (args.output/'results.json').write_text(json.dumps(results, indent=2)+'\n')
    def check(label):
        started = time.monotonic()
        checks, metadata = validate(selected, ['BEGIN ISOLATION LEVEL REPEATABLE READ'], env,
                                    compare_scores=label != 'during')
        # Connection close rolls back the verification transaction and its temporary tables.
        report = dict(checks=checks, metadata=metadata,
                      comparison='membership-cardinality-finite-scores' if label == 'during' else 'exact-score-multiset',
                      wall_seconds=time.monotonic()-started)
        (args.output/(label+'-correctness.json')).write_text(json.dumps(report, indent=2)+'\n')
        return report
    def launch(role, clients, scripts, rate=0):
        command = ['pgbench', '-n', '-M', 'prepared', '-c', str(clients), '-j', str(clients),
                   '-T', str(args.seconds), '-l', '--log-prefix', str(args.output/role)]
        if rate:
            command += ['-R', str(rate)]
        for script in scripts:
            command += ['-f', str(script)]
        log = (args.output/(role+'.txt')).open('w')
        handles.append(log)
        proc = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        processes.append(proc)
        return proc
    recorder.psql(f'CREATE SCHEMA {schema}', env)
    try:
        recorder.psql(';\n'.join(setup)+';', env)
        results['before'] = check('before')
        if any(c['status'] != 'passed' for c in results['before']['checks']):
            raise RuntimeError('before correctness failed')
        plans = server_times.explain_all(args.engine,
                    [(c['name'], c['sql']) for c in selected], 1, False, env, [], True)
        (args.output/'plans.json').write_text(json.dumps(plans)+'\n')
        if len(plans) != len(selected) or not all(
                any(n.get('Pruning') == 'block-max' for n in nodes(p[0]['Plan'])) for p in plans):
            raise RuntimeError('Expected ranked custom scan missing; inspect plans.json')
        scripts = []
        for c in selected:
            script = args.output/(c['name']+'.sql')
            script.write_text(c['sql']+';\n')
            scripts.append(script)
        writer = args.output/'update.sql'
        writer.write_text(f"\\set row random(1, {args.rows})\n"
            "UPDATE shape_documents SET body = CASE WHEN body LIKE 'wordaa %' "
            "THEN substr(body,8) ELSE 'wordaa ' || body END WHERE id=:row;\n")
        started = time.monotonic()
        reader = launch('reader', args.readers, scripts)
        write = launch('writer', 1, [writer], args.write_rate) if args.write_rate else None
        # Check live membership on one snapshot; index scoring statistics are not MVCC-frozen.
        with ThreadPoolExecutor(max_workers=1) as pool:
            def during():
                time.sleep(args.seconds/3)
                return check('during')
            future = pool.submit(during)
            reader_status = reader.wait()
            elapsed = time.monotonic()-started
            writer_status = write.wait() if write else None
            writer_elapsed = time.monotonic()-started
            results['during'] = future.result()
        results['reader'] = recorder.summarize_logs(
            sorted(args.output.glob('reader.*[0-9]')), [c['name'] for c in selected], elapsed)
        if write:
            results['writer'] = recorder.summarize_logs(
                sorted(args.output.glob('writer.*[0-9]')), ['update'], writer_elapsed)
        results['exit_codes'] = dict(reader=reader_status, writer=writer_status)
        results['after'] = check('after')
        save()
        if any(c['status'] != 'passed' for phase in ('before', 'during', 'after')
               for c in results[phase]['checks']):
            raise RuntimeError('correctness failed; see results.json')
        for role in ('reader', 'writer'):
            if role not in results:
                continue
            if results['exit_codes'][role] or results[role]['failures'] or not all(
                    q['completed'] for q in results[role]['queries'].values()):
                raise RuntimeError(f'{role} traffic failed; see results.json')
    finally:
        for proc in processes:
            if proc.poll() is None:
                proc.terminate()
                proc.wait()
        for handle in handles:
            handle.close()
        save()
        recorder.psql(f'DROP SCHEMA {schema} CASCADE', env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', choices=['stannum', 'tin'], default='stannum')
    parser.add_argument('--rows', type=int, default=4096)
    parser.add_argument('--repetitions', type=int, default=7)
    parser.add_argument('--discard', type=int, default=2)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--generate-only', action='store_true')
    parser.add_argument('--readers', type=int, default=0, help='Run concurrent pgbench instead of serial timing')
    parser.add_argument('--seconds', type=int, default=30)
    parser.add_argument('--write-rate', type=int, default=0, help='Target UPDATE transactions/second')
    args = parser.parse_args()
    if args.readers < 0 or args.write_rate < 0 or args.seconds < 5:
        parser.error('readers/write-rate must be nonnegative; seconds >= 5')
    if args.readers and (args.engine != 'stannum' or args.generate_only):
        parser.error('concurrent mode requires stannum and cannot use generate-only')
    if args.write_rate and not args.readers:
        parser.error('write-rate requires readers')
    if not 0 <= args.discard < args.repetitions:
        parser.error('require 0 <= discard < repetitions')
    setup = fixture(args.rows, args.engine)
    cases = catalog(args.engine)
    args.output.mkdir(parents=True, exist_ok=False)
    payload = dict(engine=args.engine, rows=args.rows, setup=setup, cases=cases)
    encoded = json.dumps(payload, indent=2) + '\n'
    (args.output / 'catalog.json').write_text(encoded)
    timing = dict(setup=setup, queries=[[c['name'], c['sql']] for c in cases])
    (args.output / 'server-cases.json').write_text(json.dumps(timing, indent=2)+'\n')
    if args.generate_only:
        return
    if args.readers:
        concurrent(args, cases)
        return
    validation, metadata = validate(cases, setup, dict(os.environ))
    (args.output/'server.json').write_text(json.dumps(metadata,indent=2)+'\n')
    for case, result in zip(cases, validation):
        if result['status'] != 'passed':
            (args.output/(case['name']+'-repro.sql')).write_text(';\n'.join(setup+[case['sql']])+';\n')
    (args.output/'correctness.json').write_text(json.dumps(validation,indent=2)+'\n')
    passed = {r['case'] for r in validation if r['status'] == 'passed'}
    timing['queries'] = [q for q in timing['queries'] if q[0] in passed]
    if not passed:
        raise RuntimeError('all correctness checks failed; see correctness.json')
    plans = server_times.explain_all(args.engine, timing['queries'], args.repetitions,
                                    False, dict(os.environ), timing['setup'], True)
    rows = server_times.summarize(timing['queries'], plans, args.repetitions, args.discard)
    for i, row in enumerate(rows):
        retained = plans[i*args.repetitions:(i+1)*args.repetitions][args.discard:]
        row['planning_median_ms'] = statistics.median(p[0]['Planning Time'] for p in retained)
        row['plan_nodes'] = sorted({n.get('Custom Plan Provider', n['Node Type'])
                                   for p in retained for n in nodes(p[0]['Plan'])})
        row['strategies'] = sorted({n['Candidate Strategy'] for p in retained
                                   for n in nodes(p[0]['Plan']) if 'Candidate Strategy' in n})
    report = dict(engine=args.engine, rows=args.rows, cases=rows,
                  correctness=validation, repetitions=args.repetitions, discard=args.discard,
                  catalog_sha256=hashlib.sha256(encoded.encode()).hexdigest(),
                  sources={p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                           for p in (Path(__file__), Path(server_times.__file__))},
                  caveat='Synthetic temporary-table fixture; warm single-client diagnostics, not capacity or LED equivalence.')
    (args.output/'results.json').write_text(json.dumps(report,indent=2)+'\n')
    (args.output/'plans.json').write_text(json.dumps(plans)+'\n')
    print(f'{len(passed)}/{len(cases)} cases passed correctness; results at {args.output}')
    if len(passed) != len(cases):
        raise SystemExit(1)


if __name__ == '__main__':
    main()
