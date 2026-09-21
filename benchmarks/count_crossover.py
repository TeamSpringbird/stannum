#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Measure default versus forced-page counting over the published OR trace.

Uses persistent sessions and warm execution; times are server EXPLAIN timings.
Features come from untimed planner estimates, never actual matching counts.
"""
import argparse
import hashlib
import json
from pathlib import Path
import random
import re
import statistics

KNOWN = {302,88,301,222,197,251}


def partition(query):
    # Already inspected examples must not be advertised as held-out evidence.
    key = str(query['source_id'])
    return 'train' if query['source_id'] in KNOWN or int(hashlib.sha256(('count-v1:'+key).encode()).hexdigest()[:8],16)%10 < 7 else 'heldout'


def main():
    import psycopg
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--queries',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--repetitions',type=int,default=9)
    args=parser.parse_args()
    if args.repetitions < 3: parser.error('at least three repetitions required')
    args.output.mkdir(parents=True,exist_ok=False)
    queries=json.loads(args.queries.read_text())['queries']
    assert len(queries)==302 and len({q['source_id'] for q in queries})==302
    assert all(re.fullmatch('[a-z0-9 ]+',q['text']) for q in queries)
    protocol=dict(query_file_sha256=hashlib.sha256(args.queries.read_bytes()).hexdigest(),
                  partitions={str(q['source_id']):partition(q) for q in queries}, repetitions=args.repetitions,
                  semantics='Published OR COUNT; default selector versus forced pages, persistent session, warm cache')
    (args.output/'protocol.json').write_text(json.dumps(protocol,indent=2)+'\n')
    with psycopg.connect('',autocommit=True,prepare_threshold=None) as conn:
        conn.execute('SET jit=off; SET statement_timeout=120000; SET plan_cache_mode=force_custom_plan; SET enable_seqscan=off')
        relation=conn.execute("SELECT relpages,reltuples FROM pg_class WHERE oid='documents'::regclass").fetchone()
        segments=conn.execute("SELECT count(*) FROM stannum.segment_info('documents_idx') WHERE kind='immutable'").fetchone()[0]
        reference={n:set(body.split()) for n,body in conn.execute('SELECT n,body FROM documents ORDER BY n LIMIT 1000')}
        def estimate(text):
            return conn.execute('EXPLAIN (FORMAT JSON) SELECT id FROM documents WHERE body ==> %s',(text,)).fetchone()[0][0]['Plan']['Plan Rows']
        terms={t for q in queries for t in q['text'].split()}
        estimates={t:estimate(t) for t in sorted(terms)}
        protocol.update(heap_pages=relation[0],estimated_rows=relation[1],immutable_segments=segments,term_estimates=estimates)
        (args.output/'protocol.json').write_text(json.dumps(protocol,indent=2)+'\n')
        random.Random(20260921).shuffle(queries)
        results=[]
        with (args.output/'plans.jsonl').open('w') as plans:
            for number,q in enumerate(queries):
                query=q['engines']['tin']['disjunction']
                unique=set(q['text'].split())
                expected_ids=sorted(n for n,tokens in reference.items() if unique & tokens)
                counts=[]
                for mode in ('off','on'):
                    conn.execute('SET stannum.force_count_pages='+mode)
                    counts.append(conn.execute('SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0])
                    actual=sorted(r[0] for r in conn.execute('SELECT n FROM documents WHERE n=ANY(%s) AND body ==> %s',(list(reference),query)))
                    assert actual==expected_ids,(q['source_id'],mode,'membership mismatch')
                assert counts[0]==counts[1],(q['source_id'],'count mismatch',counts)
                runs=[]
                for repetition in range(args.repetitions):
                    for mode in (('off','on') if repetition%2==0 else ('on','off')):
                        conn.execute('SET stannum.force_count_pages='+mode)
                        plan=conn.execute('EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON) SELECT count(*) FROM documents WHERE body ==> %s',(query,)).fetchone()[0][0]
                        node=plan['Plan']
                        assert node['Custom Plan Provider']=='Stannum Count',node
                        if mode=='on': assert node['Count Strategy']=='page bitmaps',node
                        runs.append(dict(mode=mode,repetition=repetition,execution_ms=plan['Execution Time'],planning_ms=plan['Planning Time'],strategy=node['Count Strategy'],heap_fetches=node['Heap Fetches']))
                        plans.write(json.dumps(dict(id=q['source_id'],mode=mode,repetition=repetition,plan=plan))+'\n')
                total=sum(estimates[t] for t in unique)
                result=dict(id=q['source_id'],text=q['text'],partition=partition(q),exact_count=counts[0],
                            features=dict(unique_terms=len(unique),estimated_postings=total,estimated_query_rows=estimate(query),
                                          postings_per_heap_page=total/max(1,relation[0]),heap_pages=relation[0],immutable_segments=segments),
                            runs=runs,median_ms={mode:statistics.median(r['execution_ms'] for r in runs if r['mode']==mode) for mode in ('off','on')})
                results.append(result)
                (args.output/'comparison.json').write_text(json.dumps(results,indent=2)+'\n')
                if (number+1)%25==0: print(f'{number+1}/302 queries complete',flush=True)
        (args.output/'status.json').write_text(json.dumps(dict(status='complete',queries=len(results),reference_rows=len(reference),correctness_mismatches=0))+'\n')


if __name__=='__main__':
    main()
