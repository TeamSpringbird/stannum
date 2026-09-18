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
    def check():
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
        sql('VACUUM (INDEX_CLEANUP ON) docs;')
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
        assert sql("SELECT count(*) FROM docs WHERE id=99998 AND body ==> 'needle';") == '1'
        assert sql('SELECT count(*) FROM volatile_docs;') == '0'
        sql("INSERT INTO volatile_docs VALUES('needle');")
        assert sql("SELECT count(*) FROM volatile_docs WHERE body ==> 'needle';") == '1'
        sql("TRUNCATE docs; INSERT INTO docs VALUES(1,'needle',0);")
        check()
        assert sql("SELECT count(*) FROM docs WHERE body ==> 'needle';") == '1'
        stop(); start(); check()
        # Verify the conservative read path on an actual streaming standby.
        command(['pg_basebackup','-h',str(root),'-p','28928','-U','postgres','-D',str(standby),'-X','stream','-R','--checkpoint=fast'])
        with (standby/'postgresql.conf').open('a') as f:
            f.write("\nport=28929\nhot_standby=on\n")
        command(['pg_ctl','-D',str(standby),'-l',str(root/'standby.log'),'-w','start'])
        standby_env=dict(env,PGPORT='28929')
        recovery=command(['psql','-X','-qAt','-c','SELECT pg_is_in_recovery();'],env=standby_env).strip()
        assert recovery == 't'
        plan=json.loads(command(['psql','-X','-qAt','-c',"EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert plan[0]['Plan']['Lossy Heap Blocks'] == 1, plan
        assert plan[0]['Plan']['Actual Rows'] == 1, plan
        # A snapshot acquired during recovery retains the fallback after promotion.
        promotion_env = dict(standby_env, PGAPPNAME='stannum-promotion-check')
        promotion = subprocess.Popen(['psql','-X','-qAt','-v','ON_ERROR_STOP=1'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=promotion_env)
        promotion.stdin.write("BEGIN ISOLATION LEVEL REPEATABLE READ; SET LOCAL stannum.enable_custom_scan=off; SELECT count(*) FROM docs; SELECT pg_sleep(3); EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle'; SET LOCAL stannum.enable_custom_scan=on; SELECT 'custom:' || count(*) FROM docs WHERE body ==> 'needle'; COMMIT;")
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
        # The custom scan falls back to a heap scan for the same recovery snapshot.
        assert 'custom:1' in out, out
        new_plan=json.loads(command(['psql','-X','-qAt','-c',"SET stannum.enable_custom_scan=off; EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert new_plan[0]['Plan']['Exact Heap Blocks'] == 1, new_plan
        assert new_plan[0]['Plan']['Lossy Heap Blocks'] == 0, new_plan
        custom_plan=json.loads(command(['psql','-X','-qAt','-c',"EXPLAIN (ANALYZE, FORMAT JSON) SELECT * FROM docs WHERE body ==> 'needle';"],env=standby_env))
        assert custom_plan[0]['Plan']['Custom Plan Provider'] == 'Stannum Text Search Scan', custom_plan
        assert custom_plan[0]['Plan']['Actual Rows'] == 1, custom_plan
        result={'status':'passed', 'concurrent_reader_checks':checks, 'checks':['build','overflow','rollback','HOT-eligible updates','vacuum','tuple reuse','concurrent index build','concurrent writer/readers','repeatable-read snapshot with vacuum','reindex','fold/merge/rewrite/reclaim cycles','immediate shutdown and WAL recovery','unlogged reset','truncate','clean restart','streaming standby fallback','cancellation and backend reuse','snapshot-origin fallback after promotion','per-statement scorer state']}
        (root/'result.json').write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps(result)); print('Artifacts:',root)
    finally:
        if (standby/'postmaster.pid').exists():
            command(['pg_ctl','-D',str(standby),'-m','fast','-w','stop'])
        if (data/'postmaster.pid').exists(): stop()


if __name__ == '__main__':
    main()
