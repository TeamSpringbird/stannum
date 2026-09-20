#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Paired count-strategy diagnostics on an existing, exclusively owned database.

Requires libpq environment, the experimental force_count_pages GUC, and a loaded
`documents` table with its Stannum index. Run outside timed benchmark traffic.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import time


def literal(text):
    return "'" + text.replace("'", "''") + "'"


def sql(statement, mode='off', application='count-probe'):
    env = dict(os.environ, PGAPPNAME=application,
               PGOPTIONS=f'-c statement_timeout=120000 -c jit=off -c plan_cache_mode=force_custom_plan -c stannum.force_count_pages={mode}')
    return subprocess.check_output(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-c', statement], env=env, text=True).strip()


def paired(queries, root, repetitions):
    results = []
    sql('SELECT 1 FROM documents LIMIT 1;')
    for query in queries:
        predicate = 'body ==> ' + literal(query['tinql'])
        count = 'SELECT count(*) FROM documents WHERE ' + predicate
        expected = int(sql(count))
        assert int(sql(count, 'on')) == expected, query['id']
        # Independent lexical check on a bounded reference set, plus full-count
        # agreement across the two strategies. No claim of exhaustive oracle coverage.
        lexical = ' OR '.join('body ~ ' + literal('(^| )' + term + '( |$)') for term in query['text'].split())
        reference = ('WITH reference AS MATERIALIZED (SELECT id,body FROM documents ORDER BY id LIMIT 1000), '
                     'indexed AS MATERIALIZED (SELECT id FROM documents WHERE ' + predicate + ') '
                     'SELECT count(*) FROM ((SELECT id FROM indexed JOIN reference USING(id) '
                     'EXCEPT ALL SELECT id FROM reference WHERE ' + lexical + ') UNION ALL '
                     '(SELECT id FROM reference WHERE ' + lexical + ' EXCEPT ALL '
                     'SELECT id FROM indexed JOIN reference USING(id))) differences;')
        for mode in ('off', 'on'):
            assert int(sql(reference, mode)) == 0, query['id']
        runs = []
        for repetition in range(repetitions):
            for mode in (('off', 'on') if repetition % 2 == 0 else ('on', 'off')):
                statement = ('PREPARE count_probe AS SELECT count(*) FROM documents WHERE body ==> $1; '
                             'EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) EXECUTE count_probe(' + literal(query['tinql']) + ');')
                plan = json.loads(sql(statement, mode))[0]
                node = plan['Plan']
                assert node.get('Custom Plan Provider') == 'Stannum Count', node
                if mode == 'on':
                    assert node.get('Count Strategy') == 'page bitmaps', node
                name = f"{query['id'].replace(':', '-')}-{mode}-{repetition}.json"
                (root / name).write_text(json.dumps(plan, indent=2)+'\n')
                runs.append(dict(mode=mode, repetition=repetition, execution_ms=plan['Execution Time'],
                                 strategy=node['Count Strategy'], candidates=node.get('Candidates'),
                                 heap_fetches=node.get('Heap Fetches'), plan=name))
        medians = {mode: statistics.median(r['execution_ms'] for r in runs if r['mode']==mode) for mode in ('off','on')}
        results.append(dict(query=query, exact_count=expected, sampled_reference_rows=1000,
                            median_ms=medians, default_over_forced=medians['off']/medians['on'], runs=runs))
        (root/'comparison.json').write_text(json.dumps(results,indent=2)+'\n')
    return results


def profile(query, mode, root, symfs):
    """Sample one backend for ten seconds; no profile timing is a benchmark result."""
    label=f"count-profile-{query['id'].split(':')[0]}-{mode}"
    count='SELECT count(*) FROM documents WHERE body ==> ' + literal(query['tinql'])
    statement=('SELECT pg_sleep(3); DO $profile$ DECLARE deadline timestamptz := clock_timestamp() + interval \'12 seconds\'; n bigint; '
               'BEGIN WHILE clock_timestamp() < deadline LOOP EXECUTE '+literal(count)+' INTO n; END LOOP; END $profile$;')
    env=dict(os.environ,PGAPPNAME=label,PGOPTIONS=f'-c statement_timeout=120000 -c jit=off -c plan_cache_mode=force_custom_plan -c stannum.force_count_pages={mode}')
    with (root/(label+'.log')).open('w') as log:
        process=subprocess.Popen(['psql','-XqAt','-v','ON_ERROR_STOP=1','-c',statement],env=env,stdout=log,stderr=subprocess.STDOUT)
        try:
            for _ in range(30):
                pid=sql('SELECT pid FROM pg_stat_activity WHERE application_name='+literal(label)+';')
                if pid:break
                time.sleep(.1)
            else:raise RuntimeError('Profile backend did not appear')
            # The experiment container uses host PID namespace; pg_backend_pid is
            # therefore the host PID perf attaches to. Never assume this otherwise.
            data=root/(label+'.perf.data')
            with (root/(label+'.perf.log')).open('w') as perf_log:
                subprocess.run(['perf','record','-F','99','-g','--call-graph','dwarf','-p',pid,'-o',str(data),'--','sleep','10'],stdout=perf_log,stderr=subprocess.STDOUT,check=True)
            process.wait(timeout=120)
            if process.returncode:raise RuntimeError('Profile query failed')
            with (root/(label+'.perf.txt')).open('w') as report:
                subprocess.run(['perf','report','--stdio','--no-children','--symfs',str(symfs),'-i',str(data)],stdout=report,stderr=subprocess.STDOUT,check=True)
        finally:
            if process.poll() is None:
                process.terminate();process.wait(timeout=10)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--queries',type=Path,default=Path('benchmarks/count-probe/queries.json'))
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--repetitions',type=int,default=7)
    parser.add_argument('--profile',action='store_true')
    parser.add_argument('--symfs',type=Path,help='Host copy of container binaries for perf symbol resolution')
    args=parser.parse_args()
    assert args.repetitions>=3
    args.output.mkdir(parents=True,exist_ok=False)
    queries=json.loads(args.queries.read_text())
    (args.output/'queries.json').write_text(json.dumps(queries,indent=2)+'\n')
    result=paired(queries,args.output,args.repetitions)
    failures=[]
    if args.profile:
        if args.symfs is None:
            raise SystemExit('--profile requires --symfs')
        for query in queries[:2]:
            for mode in ('off','on'):
                try:profile(query,mode,args.output,args.symfs)
                except Exception as error:failures.append(dict(query=query['id'],mode=mode,error=str(error)))
    (args.output/'status.json').write_text(json.dumps({'paired_complete':True,'profiles_requested':args.profile,'profile_failures':failures},indent=2)+'\n')
    print(json.dumps([{k:r[k] for k in ('query','median_ms','default_over_forced')} for r in result],indent=2))
    if failures:raise SystemExit('Paired timings saved, but one or more CPU profiles failed')


if __name__=='__main__':
    main()
