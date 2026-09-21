# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

"""Bounded result oracle that still requires the real full-corpus index path."""


def capture(conn, limit=1000):
    rows = conn.execute('SELECT ctid::text,id,body FROM documents LIMIT %s', (limit,)).fetchall()
    if not rows:
        raise ValueError('empty reference sample')
    return [dict(tid=tid,id=key,body=body) for tid,key,body in rows]


def require_index(plan, index):
    def nodes(node):
        yield node
        for child in node.get('Plans',[]):
            yield from nodes(child)
    found=list(nodes(plan))
    if any(n['Node Type'] in ('Seq Scan','Tid Scan','Tid Range Scan') for n in found):
        raise ValueError('oracle bypassed the search index')
    if not any(n['Node Type']=='Bitmap Index Scan' and n.get('Index Name')==index for n in found):
        raise ValueError('oracle did not use the expected search index')


def check(conn, sample, query, terms, index='documents_idx'):
    expected=sorted(row['tid'] for row in sample if set(terms) & set((row['body'] or '').split()))
    tids=[row['tid'] for row in sample]
    statement='SELECT ctid::text FROM documents WHERE body ==> %s AND ctid=ANY(%s::tid[])'
    with conn.transaction():
        # TID/heap-only evaluation would test the operator, not index membership.
        # The bitmap scan may still visit many pages, but returns <=sample rows
        # and never materializes millions of text IDs or a repeated CTE join.
        conn.execute("SET LOCAL work_mem='256MB'; SET LOCAL enable_seqscan=off; SET LOCAL enable_tidscan=off; SET LOCAL enable_indexscan=off; SET LOCAL stannum.enable_custom_scan=off")
        plan=conn.execute('EXPLAIN (FORMAT JSON) '+statement,(query,tids)).fetchone()[0][0]['Plan']
        require_index(plan,index)
        actual=sorted(row[0] for row in conn.execute(statement,(query,tids)))
    if actual!=expected:
        raise ValueError('indexed sample differs from independent token reference')
    return plan
