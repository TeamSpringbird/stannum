#!/usr/bin/env python3
"""Native PG18 lifecycle checks in a disposable private cluster; install stannum first."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time


def main():
    root = Path(tempfile.mkdtemp(prefix='stannum-postings-'))
    data = root / 'data'
    standby = root / 'standby'
    # A private Unix socket directory avoids touching any existing server/port.
    env = dict(os.environ, PGHOST=str(root), PGPORT='28928', PGUSER='postgres', PGDATABASE='postgres',
               PGOPTIONS='-c enable_seqscan=off -c statement_timeout=60000')
    for name in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
        env.pop(name, None)
    def command(args, **kw):
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, **kw)
    def sql(text):
        return command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=env).strip()
    def start():
        command(['pg_ctl', '-D', str(data), '-l', str(root/'server.log'), '-w', 'start'])
    def stop(mode='fast'):
        command(['pg_ctl', '-D', str(data), '-m', mode, '-w', 'stop'])
    verified = 0
    def verify(index='docs_search', heap_check=True, env=env):
        # An empty result is the contract: every finding names a location.
        nonlocal verified
        findings = command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], env=env,
            input=f"SELECT severity || ': ' || location || ': ' || message FROM stannum.verify_index('{index}', {str(heap_check).lower()});").strip()
        assert findings == '', (index, findings)
        verified += 1
    def check():
        verify()
        for term in ('needle', 'common', 'fresh', 'missing'):
            differences = sql(f"""WITH actual AS MATERIALIZED (SELECT id FROM docs WHERE body ==> '{term}'),
                expected AS MATERIALIZED (SELECT id FROM docs WHERE body ~ '\\m{term}\\M'),
                delta AS ((SELECT * FROM actual EXCEPT SELECT * FROM expected)
                          UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM actual))
                SELECT count(*) FROM delta;""")
            assert differences == '0', (term, differences)
    try:
        command(['initdb', '-D', str(data), '-U', 'postgres', '-A', 'trust', '--no-locale', '--encoding=UTF8', '--data-checksums'])
        with (data/'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport=28928\nunix_socket_directories='{root}'\nshared_buffers='64MB'\n")
        start()
        sql("""CREATE EXTENSION stannum;
          CREATE TABLE docs(id int PRIMARY KEY, body text, revision int DEFAULT 0) WITH(fillfactor=60);
          INSERT INTO docs SELECT n, 'common ' || CASE WHEN n%100=0 THEN 'needle' ELSE 'other' END, 0
            FROM generate_series(1,5000) n;
          CREATE INDEX docs_search ON docs USING stannum(body);
          CREATE UNLOGGED TABLE volatile_docs(body text);
          INSERT INTO volatile_docs VALUES ('needle');
          CREATE INDEX volatile_search ON volatile_docs USING stannum(body);
        """)
        check()
        verify('volatile_search')
        # Cancel after scan initialization in a single reusable backend, then
        # prove that transaction/error cleanup leaves subsequent scans usable.
        cancelled = command(['psql','-X','-qAt'], input="""SET statement_timeout='50ms';
            SELECT pg_sleep(1) FROM docs WHERE body ==> 'needle' LIMIT 1;
            SET statement_timeout='60000ms';
            SELECT count(*) FROM docs WHERE body ==> 'needle';""", env=env)
        assert 'canceling statement due to statement timeout' in cancelled, cancelled
        assert cancelled.strip().endswith('50'), cancelled
        # Scorer state lives for one statement: a document inserted between
        # two statements in one backend is scored by the second.
        ranked = command(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'], input="""SET enable_seqscan=off;
            SELECT id FROM docs WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC LIMIT 1;
            INSERT INTO docs VALUES(99998,'needle needle needle needle',0);
            SELECT id, stannum.full_score(ctid) > 0 FROM docs WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC LIMIT 1;
            SELECT count(*) FROM docs WHERE body ==> 'needle' AND stannum.full_score(ctid) > 0;
            DELETE FROM docs WHERE id=99998;""", env=env).split()
        assert ranked[-2] == '99998|t' and ranked[-1] == '51', ranked
        sql("BEGIN; INSERT INTO docs VALUES(99999,'needle',0); ROLLBACK;")
        sql("UPDATE docs SET revision=revision+1; DELETE FROM docs WHERE id%2=0;")
        verify()
        sql('VACUUM (INDEX_CLEANUP ON) docs;')
        verify()
        sql("INSERT INTO docs SELECT n, 'fresh',0 FROM generate_series(6000,7500) n;")
        check()
        # A writer and a differential reader use independent sessions/snapshots.
        statements = []
        for round_ in range(30):
            statements.append(f"BEGIN; UPDATE docs SET body=CASE WHEN body='needle' THEN 'fresh' ELSE 'needle' END WHERE id BETWEEN 6000 AND 6100; INSERT INTO docs VALUES({8000+round_},'needle',0); COMMIT; SELECT pg_sleep(0.02);")
        writer = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'], stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL, stderr=(root/'writer-errors.log').open('w'), text=True, env=env)
        writer.stdin.write('\n'.join(statements)); writer.stdin.close()
        # Validate a concurrently built index, then make it the only search index.
        check()
        sql('CREATE INDEX CONCURRENTLY docs_search_new ON docs USING stannum(body);')
        sql('DROP INDEX docs_search; ALTER INDEX docs_search_new RENAME TO docs_search;')
        checks = 0
        while writer.poll() is None:
            check(); checks += 1
        assert writer.returncode == 0
        check()
        # A repeatable-read transaction keeps the old snapshot through a concurrent update and vacuum.
        snapshot_sql = "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT count(*) FROM docs WHERE body ==> 'needle'; SELECT pg_sleep(2); SELECT count(*) FROM docs WHERE body ==> 'needle'; COMMIT;"
        snapshot = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=dict(env,PGAPPNAME='stannum-snapshot-check'))
        snapshot.stdin.write(snapshot_sql); snapshot.stdin.close(); snapshot.stdin=None
        deadline=time.monotonic()+10
        while time.monotonic()<deadline:
            if sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='stannum-snapshot-check' AND wait_event='PgSleep';") == '1': break
            time.sleep(.02)
        else: raise AssertionError('snapshot session did not enter wait')
        sql("UPDATE docs SET body='fresh' WHERE body='needle';")
        sql('VACUUM (INDEX_CLEANUP ON) docs;')
        out, err = snapshot.communicate()
        assert snapshot.returncode == 0, err
        counts=[x for x in out.splitlines() if x.isdigit()]
        assert len(counts)==2 and counts[0]==counts[1], counts
        check()
        sql('REINDEX INDEX docs_search;')
        check()
        # Small thresholds drive folds, merges, dead lists, segment rewrites
        # and page reclamation through the FSM. Results must stay exact and
        # the index must stop growing once freed pages are reused.
        tuned = 'SET stannum.write_buffer_docs=4; SET stannum.max_segments=3; SET stannum.merge_tier_factor=2;'
        def check_folded():
            verify('folded_search')
            for term in ('needle', 'common', 'missing'):
                differences = sql(f"""WITH actual AS MATERIALIZED (SELECT id FROM folded WHERE body ==> '{term}'),
                    expected AS MATERIALIZED (SELECT id FROM folded WHERE body ~ '\\m{term}\\M'),
                    delta AS ((SELECT * FROM actual EXCEPT SELECT * FROM expected)
                              UNION ALL (SELECT * FROM expected EXCEPT SELECT * FROM actual))
                    SELECT count(*) FROM delta;""")
                assert differences == '0', ('folded', term, differences)
        sql(tuned + """CREATE TABLE folded(id int PRIMARY KEY, body text);
            CREATE INDEX folded_search ON folded USING stannum(body);
            INSERT INTO folded SELECT n, CASE WHEN n%10=0 THEN 'needle' ELSE 'common' END
              FROM generate_series(1,400) n;""")
        check_folded()
        sql('DELETE FROM folded WHERE id%2=0; VACUUM (INDEX_CLEANUP ON) folded;')
        check_folded()
        sql('VACUUM (INDEX_CLEANUP ON) folded;')
        check_folded()
        size_after_first_cycle = int(sql("SELECT pg_relation_size('folded_search');"))
        for cycle in range(4):
            sql(tuned + f"""DELETE FROM folded WHERE id%4={cycle};
                INSERT INTO folded SELECT n, CASE WHEN n%10=0 THEN 'needle' ELSE 'common' END
                  FROM generate_series({1000*(cycle+1)}, {1000*(cycle+1)+200}) n
                  ON CONFLICT DO NOTHING;""")
            sql('VACUUM (INDEX_CLEANUP ON) folded; VACUUM (INDEX_CLEANUP ON) folded;')
            check_folded()
            segments = int(sql("SELECT count(*) FROM stannum.segment_info('folded_search') WHERE kind='immutable';"))
            assert 1 <= segments <= 3, segments
        size_after_cycles = int(sql("SELECT pg_relation_size('folded_search');"))
        assert size_after_cycles <= 3 * size_after_first_cycle, (size_after_first_cycle, size_after_cycles)

        # No manual checkpoint before immediate shutdown: replay must recover WAL.
        sql("INSERT INTO docs VALUES(99998,'needle',0); DELETE FROM docs WHERE id=1;")
        stop('immediate'); start()
        check()
        verify('volatile_search')
        assert sql("SELECT count(*) FROM docs WHERE id=99998 AND body ==> 'needle';") == '1'
        assert sql('SELECT count(*) FROM volatile_docs;') == '0'
        assert sql("SELECT pg_relation_size('volatile_search', 'init');") == str(2 * 8192)
        assert sql("SELECT count(*) FROM stannum.segment_info('volatile_search');") == '0'
        assert sql("SELECT pg_relation_size('volatile_search');") == str(2 * 8192)
        assert sql("SELECT count(*) FROM volatile_docs WHERE body ==> 'needle';") == '0'
        sql("INSERT INTO volatile_docs VALUES('needle');")
        assert sql("SELECT count(*) FROM volatile_docs WHERE body ==> 'needle';") == '1'
        sql("REINDEX INDEX volatile_search;")
        verify('volatile_search')
        unlogged_plan = json.loads(sql("EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM volatile_docs WHERE body ==> 'needle';"))
        assert unlogged_plan[0]['Plan']['Custom Plan Provider'] == 'Stannum Text Search Scan', unlogged_plan
        assert unlogged_plan[0]['Plan']['Actual Rows'] == 1, unlogged_plan
        sql("TRUNCATE docs; INSERT INTO docs VALUES(1,'needle',0);")
        check()
        assert sql("SELECT count(*) FROM docs WHERE body ==> 'needle';") == '1'
        stop(); start(); check()
        # Committed relations let real workers execute the custom search/count
        # plans; SPI pg_tests alone cannot establish worker launch behavior.
        sql("CREATE TABLE worker_docs(id int, body text); INSERT INTO worker_docs SELECT n, CASE WHEN n%10=0 THEN 'needle' ELSE 'common' END FROM generate_series(1,10000) n; CREATE INDEX worker_search ON worker_docs USING stannum(body); ANALYZE worker_docs;")
        def nodes(plan):
            yield plan
            for child in plan.get('Plans', []):
                yield from nodes(child)
        parallel_settings = "SET debug_parallel_query=on; SET max_parallel_workers_per_gather=2; SET min_parallel_table_scan_size=0; SET parallel_setup_cost=0; SET parallel_tuple_cost=0; SET enable_bitmapscan=off;"
        worker_plans = []
        for query, rows in [("SELECT * FROM worker_docs WHERE body ==> 'needle'", 1000), ("SELECT count(*) FROM worker_docs WHERE body ==> 'needle'", 1)]:
            plan = json.loads(sql(parallel_settings + 'EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON) ' + query))[0]['Plan']
            assert plan['Actual Rows'] == rows, plan
            worker_plans.append(plan)
        # debug_parallel_query wraps safe non-partial paths in a single-copy
        # Gather. Candidate cursors remain worker-local, never DSM-shared.
        for plan, provider in zip(worker_plans, ('Stannum Text Search Scan', 'Stannum Count')):
            assert any(node.get('Workers Launched', 0) > 0 for node in nodes(plan)), plan
            custom_nodes = [node for node in nodes(plan) if node.get('Custom Plan Provider') == provider]
            assert len(custom_nodes) == 1, plan
            assert custom_nodes[0].get('Execution Counters') == 'Unavailable from parallel workers', custom_nodes[0]
            assert 'Heap Fetches' not in custom_nodes[0] and 'Candidates' not in custom_nodes[0], custom_nodes[0]
        assert sql(parallel_settings + "SELECT count(*) FROM worker_docs WHERE body ==> 'needle';") == '1000'
        sql(tuned + "CREATE TABLE standby_churn(id int, body text); INSERT INTO standby_churn SELECT n, 'needle common' FROM generate_series(1,200) n; CREATE INDEX standby_churn_idx ON standby_churn USING stannum(body);")
        # Verify the conservative read path on an actual streaming standby.
        command(['pg_basebackup','-h',str(root),'-p','28928','-U','postgres','-D',str(standby),'-X','stream','-R','--checkpoint=fast'])
        with (standby/'postgresql.conf').open('a') as f:
            f.write("\nport=28929\nhot_standby=on\nmax_standby_streaming_delay=-1\nwal_receiver_status_interval=1s\n")
        command(['pg_ctl','-D',str(standby),'-l',str(root/'standby.log'),'-w','start'])
        standby_env=dict(env,PGPORT='28929')
        recovery=command(['psql','-X','-qAt','-c','SELECT pg_is_in_recovery();'],env=standby_env).strip()
        assert recovery == 't'
        # Verification takes only AccessShareLock during recovery.
        verify(env=standby_env)
        verify('folded_search', env=standby_env)
        plan=json.loads(command(['psql','-X','-qAt','-c',"EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert plan[0]['Plan']['Lossy Heap Blocks'] == 1, plan
        assert plan[0]['Plan']['Actual Rows'] == 1, plan
        standby_snapshot_checks = 0
        def standby_sql(text):
            return command(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'], input=text, env=standby_env).strip()
        for feedback in ('off', 'on'):
            standby_sql(f"ALTER SYSTEM SET hot_standby_feedback={feedback}; SELECT pg_reload_conf();")
            # Wait for replay of the preceding scenario before acquiring a new
            # snapshot, then record its expected answer independently of Stannum.
            target = sql('SELECT pg_current_wal_flush_lsn();')
            deadline = time.monotonic() + 20
            while standby_sql(f"SELECT pg_last_wal_replay_lsn() >= '{target}'::pg_lsn;") != 't':
                assert time.monotonic() < deadline, 'standby failed to catch up'
                time.sleep(.02)
            expected = standby_sql("SELECT count(*) FROM standby_churn WHERE body LIKE '%needle%';")
            reader_env = dict(standby_env, PGAPPNAME='stannum-standby-snapshot')
            statements = ['BEGIN ISOLATION LEVEL REPEATABLE READ;']
            for iteration in range(160):
                statements.append(f"SET LOCAL stannum.enable_custom_scan={'on' if iteration%2 else 'off'}; SELECT count(*) FROM standby_churn WHERE body ==> 'needle'; SELECT pg_sleep(0.03);")
            statements.append('COMMIT;')
            reader_script = root / f'standby-reader-{feedback}.sql'
            reader_script.write_text('\n'.join(statements))
            reader = subprocess.Popen(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1', '-f', str(reader_script)],
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=reader_env)
            deadline = time.monotonic() + 10
            while standby_sql("SELECT count(*) FROM pg_stat_activity WHERE application_name='stannum-standby-snapshot' AND wait_event='PgSleep';") != '1':
                assert time.monotonic() < deadline, 'standby snapshot did not start'
                time.sleep(.02)
            if feedback == 'on':
                deadline = time.monotonic() + 3
                while sql('SELECT count(*) FROM pg_stat_replication WHERE backend_xmin IS NOT NULL;') == '0':
                    assert time.monotonic() < deadline, 'standby feedback xmin not received'
                    time.sleep(.02)
            # Inserts force repeated folds, merges and reclamation. Updates and
            # VACUUM also test heap recovery conflicts: with feedback off, replay
            # may wait for this reader, rather than cancelling its old snapshot.
            base = 10000 if feedback == 'off' else 20000
            for iteration in range(16):
                first = base + iteration * 30
                sql(tuned + f"INSERT INTO standby_churn SELECT n, 'needle common' FROM generate_series({first}, {first+29}) n;")
            sql("UPDATE standby_churn SET body='changed' WHERE id<=100;")
            sql('VACUUM (INDEX_CLEANUP ON) standby_churn;')
            out, err = reader.communicate(timeout=30)
            assert reader.returncode == 0, (feedback, err)
            counts = [line for line in out.splitlines() if line.isdigit()]
            assert len(counts) == 160 and set(counts) == {expected}, (feedback, expected, counts)
            standby_snapshot_checks += len(counts)
            verify('standby_churn_idx')
        ranked_plan = json.loads(standby_sql("EXPLAIN (ANALYZE, VERBOSE, FORMAT JSON) SELECT stannum.full_score(ctid) FROM docs WHERE body ==> 'needle' ORDER BY stannum.full_score(ctid) DESC;"))
        assert 'score_bound_indexed' not in json.dumps(ranked_plan), ranked_plan
        assert 'score_bound' in json.dumps(ranked_plan), ranked_plan
        assert standby_sql("SELECT stannum.full_score(ctid) > 0 FROM docs WHERE body ==> 'needle';") == 't'
        # Direct SQL calls bypass planner routing, including privileged callers.
        direct_indexed_score = "SELECT stannum.score_bound_indexed('(0,1)'::tid, 'needle', 'docs'::regclass::oid::int, 'docs_search'::regclass::oid::int, 1, NULL, NULL, NULL, NULL, NULL);"
        recovery_score_error = 'indexed scoring is unavailable for recovery snapshots'
        rejected = subprocess.run(['psql', '-XqAt', '-v', 'ON_ERROR_STOP=1'],
                                  input=direct_indexed_score, text=True, capture_output=True,
                                  env=standby_env)
        assert rejected.returncode == 3 and recovery_score_error in rejected.stderr, rejected
        # A snapshot acquired during recovery retains the fallback after promotion.
        promotion_env = dict(standby_env, PGAPPNAME='stannum-promotion-check')
        promotion = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=promotion_env)
        promotion.stdin.write("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL stannum.enable_custom_scan=off; SELECT count(*) FROM docs; SELECT pg_sleep(3); SELECT 'promoted:' || (NOT pg_is_in_recovery())::text; EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle'; SET LOCAL stannum.enable_custom_scan=on; SELECT 'custom:' || count(*) FROM docs WHERE body ==> 'needle'; SELECT 'score:' || (stannum.full_score(ctid)>0)::text FROM docs WHERE body ==> 'needle'; SAVEPOINT direct_score;\n"
                              + "\\set ON_ERROR_STOP off\n" + direct_indexed_score + "\n"
                              + "\\echo direct-indexed-error :ERROR\n\\set ON_ERROR_STOP on\n"
                              + "ROLLBACK TO SAVEPOINT direct_score; SELECT 'snapshot-survived:' || count(*) FROM docs; COMMIT;")
        promotion.stdin.close(); promotion.stdin=None
        deadline=time.monotonic()+10
        while time.monotonic()<deadline:
            waiting=command(['psql','-X','-qAt','-c',"SELECT count(*) FROM pg_stat_activity WHERE application_name='stannum-promotion-check' AND wait_event='PgSleep';"],env=standby_env).strip()
            if waiting == '1': break
            time.sleep(.02)
        else: raise AssertionError('promotion session did not enter wait')
        command(['pg_ctl','-D',str(standby),'-w','promote'])
        out, err = promotion.communicate()
        assert promotion.returncode == 0, err
        old_plan=json.loads(out[out.index('['):out.rindex(']')+1])
        assert old_plan[0]['Plan']['Lossy Heap Blocks'] == 1, old_plan
        # The same recovery snapshot retains heap fallback with custom scans enabled.
        assert 'custom:1' in out, out
        assert 'score:true' in out, out
        assert 'promoted:true' in out, out
        assert 'direct-indexed-error true' in out and recovery_score_error in err, (out, err)
        assert 'snapshot-survived:1' in out, out
        # New primary snapshots can call the indexed scorer again.
        assert standby_sql(direct_indexed_score), 'indexed scorer returned no value after promotion'
        new_plan=json.loads(command(['psql','-X','-qAt','-c',"SET stannum.enable_custom_scan=off; EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert new_plan[0]['Plan']['Exact Heap Blocks'] == 1, new_plan
        assert new_plan[0]['Plan']['Lossy Heap Blocks'] == 0, new_plan
        custom_plan=json.loads(command(['psql','-X','-qAt','-c',"EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert custom_plan[0]['Plan']['Custom Plan Provider'] == 'Stannum Text Search Scan', custom_plan
        assert custom_plan[0]['Plan']['Actual Rows'] == 1, custom_plan
        verify(env=standby_env)
        result={'status':'passed', 'concurrent_reader_checks':checks, 'verify_index_calls':verified, 'standby_snapshot_checks':standby_snapshot_checks, 'checks':['build','overflow','rollback','HOT-eligible updates','vacuum','tuple reuse','concurrent index build','concurrent writer/readers','repeatable-read snapshot with vacuum','reindex','fold/merge/rewrite/reclaim cycles','immediate shutdown and WAL recovery','unlogged reset and indexed REINDEX','parallel worker execution','standby feedback on/off during folds and vacuum','truncate','clean restart','streaming standby fallback','cancellation and backend reuse','snapshot-origin fallback after promotion','direct indexed scoring rejects recovery snapshots','per-statement scorer state','verify_index after every phase']}
        (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps(result)); print('Artifacts:',root)
    finally:
        if (standby/'postmaster.pid').exists():
            command(['pg_ctl','-D',str(standby),'-m','fast','-w','stop'])
        if (data/'postmaster.pid').exists(): stop()


if __name__ == '__main__':
    main()
