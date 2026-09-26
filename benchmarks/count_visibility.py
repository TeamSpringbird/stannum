#!/usr/bin/env python3
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Exercise count strategies across real snapshots in an owned temporary schema.

Requires psycopg and an installed experimental force_count_pages GUC. Uses libpq
settings; creates and drops only its uniquely named schema. Run on a test DB.
"""
import json
import uuid

QUERIES = [
    ('needle', 'needle'),
    ('needle OR red', 'needle OR red'),
    ('needle AND red', 'needle AND red'),
    ('(needle OR red) AND (blue OR needle)', '(needle OR red) AND (blue OR needle)'),
    ('needle OR (red AND blue)', 'needle OR (red AND blue)'),
    ('needle OR needle OR absent', 'needle OR needle OR absent'),
    ('absent AND (needle OR red)', 'absent AND (needle OR red)'),
]


def check(conn):
    results = []
    for query, expression in QUERIES:
        # Independent lexical predicates, not the extension's sequential operator.
        for term in ('needle', 'red', 'blue', 'absent'):
            expression = expression.replace(term, f"(body ~ '(^| ){term}( |$)')")
        expected = conn.execute('SELECT count(*) FROM docs WHERE ' + expression).fetchone()[0]
        for setting, strategy in [('off', 'scalar'), ('on', 'page bitmaps')]:
            conn.execute('SET stannum.force_count_pages=' + setting)
            count = conn.execute('SELECT count(*) FROM docs WHERE body ==> %s', (query,)).fetchone()[0]
            plan = conn.execute('EXPLAIN (ANALYZE, FORMAT JSON) SELECT count(*) FROM docs WHERE body ==> %s', (query,)).fetchone()[0][0]['Plan']
            assert count == expected, (query, setting, count, expected)
            assert plan['Custom Plan Provider'] == 'Stannum Count', plan
            assert plan['Count Strategy'] == strategy, plan
            results.append(dict(query=query, strategy=strategy, count=count, heap_fetches=plan['Heap Fetches']))
    return results


def main():
    import psycopg
    schema = 'count_visibility_' + uuid.uuid4().hex
    with psycopg.connect('', autocommit=True) as writer, psycopg.connect('', autocommit=True) as reader:
        writer.execute('CREATE SCHEMA ' + schema)
        try:
            for conn in (writer, reader):
                conn.execute('SET search_path=' + schema + ',public')
                conn.execute('SET enable_seqscan=off; SET stannum.enable_custom_scan=on; SET statement_timeout=30000')
            writer.execute('CREATE TABLE docs(id int PRIMARY KEY, body text, payload int) WITH(fillfactor=60)')
            writer.execute("INSERT INTO docs SELECT n, 'filler' || CASE WHEN n%100=0 THEN ' needle' ELSE '' END || CASE WHEN n%103=0 THEN ' red' ELSE '' END || CASE WHEN n%107=0 THEN ' blue' ELSE '' END, 0 FROM generate_series(1,1000) n")
            writer.execute('CREATE INDEX docs_idx ON docs USING stannum(body)')
            writer.execute('SET stannum.write_buffer_docs=64; SET stannum.merge_tier_factor=64')
            writer.execute("INSERT INTO docs SELECT n, 'filler' || CASE WHEN n%100=0 THEN ' needle' ELSE '' END || CASE WHEN n%103=0 THEN ' red' ELSE '' END || CASE WHEN n%107=0 THEN ' blue' ELSE '' END, 0 FROM generate_series(1001,2000) n")
            segments = writer.execute("SELECT count(*) FROM stannum.segment_info('docs_idx') WHERE kind='immutable'").fetchone()[0]
            assert segments >= 2, segments
            writer.execute('VACUUM ANALYZE docs')
            reader.execute('BEGIN ISOLATION LEVEL REPEATABLE READ')
            before = check(reader)
            writer.execute('BEGIN')
            writer.execute('UPDATE docs SET payload=payload+1 WHERE id%200=0')
            writer.execute('DELETE FROM docs WHERE id%300=0')
            writer.execute("UPDATE docs SET body='needle red blue' WHERE id%101=0")
            writer.execute("INSERT INTO docs VALUES (3001,'needle red blue',0)")
            writer.execute('COMMIT')
            retained = check(reader)
            assert [r['count'] for r in before] == [r['count'] for r in retained]
            reader.execute('COMMIT')
            fresh = check(reader)
            assert fresh[0]['count'] != before[0]['count']
            assert any(r['heap_fetches'] > 0 for r in fresh)
            writer.execute('VACUUM ANALYZE docs')
            vacuumed = check(reader)
            assert [r['count'] for r in fresh] == [r['count'] for r in vacuumed]
            print(json.dumps(dict(status='passed', immutable_segments=segments, before=before, retained=retained, fresh=fresh, vacuumed=vacuumed), indent=2))
        finally:
            reader.execute('ROLLBACK')
            writer.execute('ROLLBACK')
            writer.execute('DROP SCHEMA ' + schema + ' CASCADE')


if __name__ == '__main__':
    main()
