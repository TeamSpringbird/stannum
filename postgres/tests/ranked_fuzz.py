#!/usr/bin/env python3
"""Randomized concurrency fuzzer for Stannum's ranked (top-k) scan.

Starts a throwaway cluster (install stannum first), builds a small corpus with
heavy score ties and reused heap space, then drives several writer sessions
(inserts, deletes, HOT and non-HOT updates, VACUUM, rollbacks, folding and
merge tunables) while reader sessions run random ranked queries: single terms,
OR, AND, boosts, phrases, `AND NOT`, `AT LEAST`, `full_score` and `score`,
LIMIT/OFFSET around posting-block boundaries and above the pruning cap,
cursors fetched partially with writes in between, and joins or filters that
read past k.

Each reader compares, inside one snapshot, the custom-scan output (ids and
float4 score bits) with the same query forced through the unpruned path
(`stannum.enable_custom_scan = off`, `ORDER BY score DESC, ctid`) and with a
regex sequential-scan membership check, and also checks uniqueness, descending
order, finite scores and the tie order the scan promises. On any mismatch it
writes a reproduction script (seed, schema, statements in order) and stops.

    python3 postgres/tests/ranked_fuzz.py --seconds 600 --seed 7
    python3 postgres/tests/ranked_fuzz.py --smoke      # fixed seeds, < 2 minutes

Everything random derives from `--seed`; the statement sequence is
reproducible, real timing between overlapping sessions is not.
"""
import argparse
import json
import os
from pathlib import Path
import queue
import random
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

PORT = '28938'
STATEMENT_TIMEOUT_MS = 120_000

VOCABULARY = ['alpha', 'beta', 'gamma', 'delta', 'echo', 'fox', 'golf', 'hotel', 'india', 'juliet']
RARE = ['zed', 'quux']
FILLER = 'pad'

# Scenarios that once failed or that pin a bug class. Each is run by `--smoke`
# in addition to the fixed smoke seed; keep them short.
REGRESSIONS = [
    # Two open cursors in one session (seed 1 failed at 3 seconds before the
    # per-scan scorer registry): the projected score of one cursor's rows must
    # not come from a scorer another scan rebuilt over newer statistics.
    dict(seed=1, seconds=10, writers=3, readers=2, corpus=800),
    dict(seed=101, seconds=8, writers=2, readers=1, corpus=300, twin_weight=100),
    # Rows deleted after the snapshot plus high-scoring inserts, so the pruned
    # top k is exhausted and the completed ordering must skip emitted rows.
    dict(seed=102, seconds=8, writers=2, readers=2, corpus=600, cursor_weight=100),
    # Tiny write buffers with reused heap space: retained scorers must keep
    # each document's own length after the buffer index is refreshed.
    dict(seed=103, seconds=8, writers=3, readers=2, corpus=400, fold_bias=True),
]


class FuzzFailure(Exception):
    pass


class Session:
    """One psql session driven asynchronously through its pipes.

    Statements are sent as batches; every statement is followed by a marker
    that reports psql's `ERROR` flag so a batch's output can be split per
    statement and errors detected without the session dying.
    """

    def __init__(self, name, env, trace):
        self.name = name
        self.trace = trace
        self.proc = subprocess.Popen(
            ['psql', '-X', '-q', '-A', '-t', '-v', 'ON_ERROR_STOP=0'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, env=env, bufsize=1)
        self.lines = queue.Queue()
        self.pending = None
        self.counter = 0
        self.busy_since = None
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        for line in self.proc.stdout:
            self.lines.put(line.rstrip('\n'))
        self.lines.put(None)

    def send(self, statements):
        assert self.pending is None, f'{self.name} already has a batch in flight'
        self.counter += 1
        text = []
        for i, statement in enumerate(statements):
            self.trace.append((self.name, statement))
            text.append(statement.rstrip().rstrip(';') + ';')
            text.append(f'\\echo __STMT__ {self.counter} {i} :ERROR')
        text.append(f'\\echo __BATCH__ {self.counter}')
        self.proc.stdin.write('\n'.join(text) + '\n')
        self.proc.stdin.flush()
        self.pending = statements
        self.busy_since = time.monotonic()

    def wait(self, timeout=STATEMENT_TIMEOUT_MS / 1000 + 30):
        """Returns [(statement, output lines, error flag)] for the batch in flight."""
        assert self.pending is not None
        results = []
        current = []
        deadline = time.monotonic() + timeout
        while True:
            try:
                line = self.lines.get(timeout=max(0.0, deadline - time.monotonic()))
            except queue.Empty:
                raise FuzzFailure(f'{self.name}: timed out waiting for batch {self.counter}: {self.pending}')
            if line is None:
                raise FuzzFailure(f'{self.name}: psql exited (server crash?) during {self.pending}')
            if line.startswith('__STMT__ '):
                _, batch, i, error = line.split()
                if int(batch) != self.counter:
                    continue
                results.append((self.pending[int(i)], current, error == 'true'))
                current = []
            elif line == f'__BATCH__ {self.counter}':
                break
            else:
                current.append(line)
        self.pending = None
        self.busy_since = None
        return results

    def run(self, statements, allow=()):
        """Sends and waits; unexpected errors are failures unless their SQLSTATE
        class is listed in `allow` (matched against the message)."""
        self.send(statements)
        results = self.wait()
        for statement, lines, error in results:
            if error and not any(token in '\n'.join(lines) for token in allow):
                raise FuzzFailure(f'{self.name}: {statement!r} failed:\n' + '\n'.join(lines))
        return results

    def close(self):
        try:
            if self.pending is not None:
                self.wait(timeout=5)
        except Exception:
            pass
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


# --- corpus and queries ----------------------------------------------------------

class Corpus:
    """Documents from a few templates so exact score ties abound."""

    def __init__(self, rng):
        self.rng = rng
        self.templates = []
        for _ in range(14):
            words = []
            for word in rng.sample(VOCABULARY, rng.choice([1, 1, 2, 2, 3, 4])):
                words += [word] * rng.choice([1, 1, 1, 2, 3, 5])
            if rng.random() < 0.15:
                words.append(rng.choice(RARE))
            self.templates.append((words, rng.choice([0, 0, 1, 2, 4, 8, 20])))
        self.next_id = 1
        self.live = set()

    def body(self):
        rng = self.rng
        words, pad = rng.choice(self.templates)
        words = list(words)
        if rng.random() < 0.1:
            words.append(rng.choice(VOCABULARY))
        if rng.random() < 0.3:
            rng.shuffle(words)
        if rng.random() < 0.2:
            pad = rng.choice([0, 1, 3, 7, 40, 200])
        return ' '.join(words + [FILLER] * pad)

    def new_ids(self, n):
        ids = list(range(self.next_id, self.next_id + n))
        self.next_id += n
        self.live.update(ids)
        return ids

    def pick(self, n):
        if not self.live:
            return []
        return self.rng.sample(sorted(self.live), min(n, len(self.live)))


def sql_literal(text):
    return "'" + text.replace("'", "''") + "'"


class Query:
    """A TINQL query with an equivalent regex predicate over `body`."""

    def __init__(self, tinql, predicate, shape):
        self.tinql = tinql
        self.predicate = predicate
        self.shape = shape

    @staticmethod
    def generate(rng):
        def word():
            pool = VOCABULARY + RARE + ['missing']
            return rng.choice(pool) if rng.random() < 0.15 else rng.choice(VOCABULARY)

        def term_regex(w):
            return f"body ~ {sql_literal(chr(92) + 'm' + w + chr(92) + 'M')}"

        def boost():
            return rng.choice(['', '', '', '^2', '^0.5', '^3', '^0', '^1.5'])

        shape = rng.choices(
            ['term', 'or2', 'or3', 'and2', 'and3', 'boosted_or', 'boosted_and', 'phrase',
             'andnot', 'prefix', 'atleast', 'mixed'],
            weights=[24, 14, 8, 14, 7, 7, 6, 4, 5, 2, 4, 5])[0]
        if shape == 'term':
            w = word()
            return Query(w + boost(), term_regex(w), shape)
        if shape in ('or2', 'or3', 'boosted_or'):
            n = 3 if shape == 'or3' else 2
            ws = [word() for _ in range(n)]
            boosted = shape == 'boosted_or'
            tinql = ' OR '.join(w + (boost() if boosted else '') for w in ws)
            if boosted and rng.random() < 0.3:
                tinql = f'({tinql})' + boost()
            return Query(tinql, '(' + ' OR '.join(term_regex(w) for w in ws) + ')', shape)
        if shape in ('and2', 'and3', 'boosted_and'):
            n = 3 if shape == 'and3' else 2
            ws = [word() for _ in range(n)]
            boosted = shape == 'boosted_and'
            tinql = ' AND '.join(w + (boost() if boosted else '') for w in ws)
            if boosted and rng.random() < 0.3:
                tinql = f'({tinql})' + boost()
            return Query(tinql, '(' + ' AND '.join(term_regex(w) for w in ws) + ')', shape)
        if shape == 'phrase':
            a, b = word(), word()
            return Query(f'"{a} {b}"', f"body ~ {sql_literal(chr(92) + 'm' + a + ' ' + b + chr(92) + 'M')}", shape)
        if shape == 'andnot':
            a, b = word(), word()
            return Query(f'{a} AND NOT {b}', f'({term_regex(a)} AND NOT {term_regex(b)})', shape)
        if shape == 'prefix':
            w = word()[:2]
            return Query(f'{w}*', f"body ~ {sql_literal(chr(92) + 'm' + w)}", shape)
        if shape == 'atleast':
            ws = rng.sample(VOCABULARY, 3)
            n = rng.choice([1, 2, 2, 3])
            count = ' + '.join(f'({term_regex(w)})::int' for w in ws)
            return Query(f'AT LEAST {n} OF [{" ".join(ws)}]', f'(({count}) >= {n})', shape)
        # mixed: (a OR b) AND c
        a, b, c = word(), word(), word()
        return Query(f'({a} OR {b}) AND {c}',
                     f'(({term_regex(a)} OR {term_regex(b)}) AND {term_regex(c)})', shape)


# HOT chain members mapped to their roots, as heap_get_root_tuples does: the
# index posts the root, the executor projects the member, and the scan's tie
# order is the root's heap order. Redirect line pointers name the next offset;
# HOT-updated tuples name it in t_ctid and the successor's xmin must match.
ROOTS_SQL = """WITH RECURSIVE items AS (
  SELECT blk, lp, lp_flags, lp_off, t_ctid, t_infomask2, t_xmin, t_xmax
  FROM generate_series(0, GREATEST(pg_relation_size('docs') / 8192 - 1, 0)::int) AS blk,
       heap_page_items(get_raw_page('docs', blk))
), chain AS (
  SELECT blk, lp AS root, lp AS cur, 0 AS depth FROM items
   WHERE lp_flags = 2 OR (lp_flags = 1 AND (t_infomask2 & 32768) = 0)
  UNION ALL
  SELECT c.blk, c.root, n.lp, c.depth + 1
    FROM chain c
    JOIN items i ON i.blk = c.blk AND i.lp = c.cur
    JOIN items n ON n.blk = c.blk AND n.lp = CASE WHEN i.lp_flags = 2 THEN i.lp_off
         WHEN (i.t_infomask2 & 16384) <> 0 AND ((i.t_ctid::text::point)[0])::int = i.blk
         THEN ((i.t_ctid::text::point)[1])::int END
   WHERE c.depth < 400 AND n.lp_flags = 1 AND (n.t_infomask2 & 32768) <> 0
     AND (i.lp_flags = 2 OR n.t_xmin = i.t_xmax)
)
SELECT format('(%s,%s)', blk, cur), format('(%s,%s)', blk, root) FROM chain WHERE cur <> root"""

LIMITS = [1, 1, 2, 3, 5, 8, 10, 20, 50, 100, 127, 128, 129, 200, 255, 256, 257, 300, 1000, 4095, 4096, 4097, 5000]
OFFSETS = [0, 0, 0, 0, 0, 1, 2, 3, 10, 50, 127, 128, 129, 200]
FETCHES = [1, 1, 2, 3, 5, 10, 50, 100, 127, 128, 129, 1000]


class Failure:
    def __init__(self, what, **detail):
        self.what = what
        self.detail = detail


# --- the fuzzer ---------------------------------------------------------------------

class Fuzzer:
    def __init__(self, args):
        self.args = args
        self.rng = random.Random(args.seed)
        self.root = Path(tempfile.mkdtemp(prefix=f'stannum-ranked-fuzz-{args.seed}-'))
        self.data = self.root / 'data'
        self.env = dict(os.environ, PGHOST=str(self.root), PGPORT=args.port, PGUSER='postgres',
                        PGDATABASE='postgres',
                        PGOPTIONS=f'-c statement_timeout={STATEMENT_TIMEOUT_MS} -c extra_float_digits=3')
        for name in ('PGSERVICE', 'PGSERVICEFILE', 'PGPASSWORD'):
            self.env.pop(name, None)
        self.trace = []
        self.corpus = Corpus(self.rng)
        self.writers = []
        self.readers = []
        self.stats = dict(episodes=0, comparisons=0, rows_compared=0, cursor_fetches=0, writer_ops=0,
                          pruned_plans=0, custom_plans=0, unstable_skipped=0, benign_errors=0,
                          vacuums=0, reindexes=0, folds_observed=0)
        self.schema = []

    # -- cluster --------------------------------------------------------------------
    def command(self, args, **kw):
        return subprocess.check_output(args, text=True, stderr=subprocess.STDOUT, env=self.env, **kw)

    def sql(self, text):
        return self.command(['psql', '-X', '-qAt', '-v', 'ON_ERROR_STOP=1'], input=text).strip()

    def start_cluster(self):
        self.command(['initdb', '-D', str(self.data), '-U', 'postgres', '-A', 'trust', '--no-locale',
                      '--encoding=UTF8'])
        with (self.data / 'postgresql.conf').open('a') as f:
            f.write(f"\nlisten_addresses=''\nport={self.args.port}\nunix_socket_directories='{self.root}'\n"
                    "shared_buffers='64MB'\nautovacuum=off\nfsync=off\nsynchronous_commit=off\n"
                    "full_page_writes=off\nlog_min_messages=warning\n")
        self.command(['pg_ctl', '-D', str(self.data), '-l', str(self.root / 'server.log'), '-w', 'start'])

    def stop_cluster(self):
        if (self.data / 'postmaster.pid').exists():
            subprocess.call(['pg_ctl', '-D', str(self.data), '-m', 'immediate', '-w', 'stop'],
                            env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # -- schema -----------------------------------------------------------------------
    def build_schema(self):
        rng = self.rng
        fillfactor = rng.choice([30, 50, 70, 90, 100])
        build_segment_docs = rng.choice([25, 50, 100, 300, 1000, 32768])
        statements = [
            'CREATE EXTENSION stannum',
            'CREATE EXTENSION pageinspect',
            f'CREATE TABLE docs(id int PRIMARY KEY, body text, revision int DEFAULT 0, grp int) '
            f'WITH (fillfactor={fillfactor}, autovacuum_enabled=off)',
            'CREATE TABLE keep(id int PRIMARY KEY) WITH (autovacuum_enabled=off)',
            f'SET stannum.build_segment_docs = {build_segment_docs}',
        ]
        ids = self.corpus.new_ids(self.args.corpus)
        rows = ', '.join(f"({i}, {sql_literal(self.corpus.body())}, 0, {i % 4})" for i in ids)
        statements.append(f'INSERT INTO docs VALUES {rows}')
        statements.append('CREATE INDEX docs_idx ON docs USING stannum(body)')
        keep = [i for i in ids if rng.random() < rng.choice([0.2, 0.5, 0.8])]
        if keep:
            statements.append('INSERT INTO keep VALUES ' + ', '.join(f'({i})' for i in keep))
        # A few writes before any reader, so the buffer is non-empty from the start.
        for _ in range(rng.randint(0, 3)):
            statements.append(self.writer_gucs())
            new = self.corpus.new_ids(rng.randint(1, 30))
            statements.append('INSERT INTO docs VALUES ' + ', '.join(
                f"({i}, {sql_literal(self.corpus.body())}, 0, {i % 4})" for i in new))
        self.schema = statements
        self.sql(';\n'.join(statements) + ';')
        for statement in statements:
            self.trace.append(('setup', statement))

    # -- writers ----------------------------------------------------------------------
    def writer_gucs(self):
        rng = self.rng
        fold_bias = getattr(self.args, 'fold_bias', False)
        docs = rng.choice([1, 2, 3, 5, 8, 16] if fold_bias else [1, 2, 3, 5, 8, 16, 64, 256, 1000])
        return (f'SET stannum.write_buffer_docs = {docs}; '
                f'SET stannum.write_buffer_bytes = {rng.choice([1024, 4096, 65536, 1048576])}; '
                f'SET stannum.merge_tier_factor = {rng.choice([2, 2, 3, 4, 8])}; '
                f'SET stannum.max_segments = {rng.choice([2, 3, 4, 6, 8, 16, 128])}; '
                f'SET stannum.max_merge_docs = {rng.choice([0, 0, 10, 100, 1000, 100000])}; '
                f'SET stannum.build_segment_docs = {rng.choice([10, 50, 200, 1000, 32768])}')

    def insert_statement(self, n):
        new = self.corpus.new_ids(n)
        return 'INSERT INTO docs VALUES ' + ', '.join(
            f"({i}, {sql_literal(self.corpus.body())}, 0, {i % 4})" for i in new)

    def writer_op(self, quiescent):
        """One writer step as a list of statements; updates the corpus model."""
        rng = self.rng
        corpus = self.corpus
        kinds = ['insert', 'delete', 'update_body', 'update_hot', 'vacuum', 'gucs', 'txn',
                 'rollback', 'reuse', 'keep']
        weights = [26, 14, 12, 12, 8, 5, 8, 4, 8, 3]
        if quiescent:
            kinds.append('reindex')
            weights.append(2)
        kind = rng.choices(kinds, weights=weights)[0]
        ids = lambda n: corpus.pick(n)

        def delete(chosen):
            corpus.live.difference_update(chosen)
            return f'DELETE FROM docs WHERE id = ANY(ARRAY[{",".join(map(str, chosen))}]::int[])'

        def update_body(chosen):
            values = ', '.join(f'({i}, {sql_literal(corpus.body())})' for i in chosen)
            return f'UPDATE docs d SET body = v.body FROM (VALUES {values}) AS v(id, body) WHERE d.id = v.id'

        def update_hot(chosen):
            return f'UPDATE docs SET revision = revision + 1 WHERE id = ANY(ARRAY[{",".join(map(str, chosen))}]::int[])'

        if kind == 'insert':
            n = rng.choice([1, 1, 2, 3, 5, 10, 40, 150])
            if len(corpus.live) > self.args.corpus * 3:
                n = 1
            return [self.insert_statement(n)]
        if kind == 'delete':
            chosen = ids(rng.choice([1, 2, 5, 20, 60]))
            return [delete(chosen)] if chosen else []
        if kind == 'update_body':
            chosen = ids(rng.choice([1, 2, 5, 20]))
            return [update_body(chosen)] if chosen else []
        if kind == 'update_hot':
            chosen = ids(rng.choice([1, 3, 10, 40]))
            return [update_hot(chosen)] if chosen else []
        if kind == 'vacuum':
            self.stats['vacuums'] += 1
            return [rng.choice(['VACUUM (INDEX_CLEANUP ON) docs', 'VACUUM docs',
                                'VACUUM (INDEX_CLEANUP ON, FREEZE) docs',
                                'VACUUM (INDEX_CLEANUP ON) docs; VACUUM (INDEX_CLEANUP ON) docs'])]
        if kind == 'gucs':
            return [self.writer_gucs()]
        if kind == 'txn':
            steps = ['BEGIN']
            for _ in range(rng.randint(1, 4)):
                which = rng.choice(['insert', 'delete', 'update_body', 'update_hot'])
                if which == 'insert':
                    steps.append(self.insert_statement(rng.choice([1, 3, 10])))
                else:
                    chosen = ids(rng.choice([1, 3, 10]))
                    if chosen:
                        steps.append({'delete': delete, 'update_body': update_body,
                                      'update_hot': update_hot}[which](chosen))
            steps.append('COMMIT')
            return steps
        if kind == 'rollback':
            new = corpus.new_ids(rng.choice([1, 5, 30]))
            rows = ', '.join(f"({i}, {sql_literal(corpus.body())}, 0, {i % 4})" for i in new)
            corpus.live.difference_update(new)
            steps = ['BEGIN', f'INSERT INTO docs VALUES {rows}']
            chosen = ids(rng.choice([0, 2, 5]))
            if chosen:
                steps.append(f'DELETE FROM docs WHERE id = ANY(ARRAY[{",".join(map(str, chosen))}]::int[])')
            steps.append('ROLLBACK')
            return steps
        if kind == 'reuse':
            # Free heap slots, reclaim them, then insert into the freed space.
            chosen = ids(rng.choice([3, 10, 40]))
            if not chosen:
                return []
            self.stats['vacuums'] += 1
            return [delete(chosen), 'VACUUM (INDEX_CLEANUP ON) docs', self.insert_statement(len(chosen))]
        if kind == 'keep':
            chosen = ids(rng.choice([1, 5, 20]))
            if not chosen or rng.random() < 0.5:
                return ['DELETE FROM keep WHERE id = ANY(ARRAY[' + ','.join(map(str, chosen)) + ']::int[])'] if chosen else []
            return ['INSERT INTO keep VALUES ' + ', '.join(f'({i})' for i in chosen) + ' ON CONFLICT DO NOTHING']
        if kind == 'reindex':
            self.stats['reindexes'] += 1
            return [f'SET stannum.build_segment_docs = {rng.choice([10, 50, 200, 1000])}',
                    rng.choice(['REINDEX INDEX docs_idx',
                                'CREATE INDEX CONCURRENTLY docs_idx_new ON docs USING stannum(body); '
                                'DROP INDEX docs_idx; ALTER INDEX docs_idx_new RENAME TO docs_idx'])]
        return []

    BENIGN = ('deadlock detected', 'could not serialize', 'canceling statement due to lock timeout')

    def finish_writer(self, writer):
        if writer.pending is None:
            return
        results = writer.wait()
        for statement, lines, error in results:
            if error:
                text = '\n'.join(lines)
                if any(token in text for token in self.BENIGN):
                    self.stats['benign_errors'] += 1
                    # Leave any open transaction so the session is reusable.
                    writer.send(['ROLLBACK'])
                    writer.wait()
                    return
                raise FuzzFailure(f'{writer.name}: {statement!r} failed:\n{text}')

    def quiesce_writers(self):
        for writer in self.writers:
            self.finish_writer(writer)

    def step_writer(self, writer, quiescent):
        self.finish_writer(writer)
        statements = self.writer_op(quiescent)
        if statements:
            self.stats['writer_ops'] += 1
            writer.send(statements)

    # -- readers ----------------------------------------------------------------------
    def reader_query(self, rng):
        """Chooses the SQL for one episode; returns a dict describing it."""
        query = Query.generate(rng)
        scorer = rng.choice(['stannum.full_score(d.ctid)', 'stannum.full_score(d.ctid)', 'stannum.score(d.ctid)'])
        limit = rng.choice(LIMITS)
        offset = rng.choice(OFFSETS)
        join = rng.random() < 0.15
        extra = None
        if not join and rng.random() < 0.15:
            extra = rng.choice([f'd.grp = {rng.randint(0, 3)}', 'd.id % 7 = 3', 'd.revision = 0', 'd.id % 2 = 1'])
        weights = dict(plain=50, cursor=getattr(self.args, 'cursor_weight', 40),
                       twin=getattr(self.args, 'twin_weight', 6))
        mode = rng.choices(list(weights), weights=list(weights.values()))[0]
        isolation = rng.choice(['REPEATABLE READ', 'REPEATABLE READ', 'REPEATABLE READ', 'READ COMMITTED'])
        from_clause = 'docs d JOIN keep k USING (id)' if join else 'docs d'
        where = f"d.body ==> {sql_literal(query.tinql)}"
        regex_where = query.predicate.replace('body ~', 'd.body ~')
        if extra:
            where += f' AND {extra}'
            regex_where += f' AND {extra}'
        select = f'SELECT d.id, d.ctid, {scorer} AS score FROM {from_clause} WHERE {where}'
        return dict(
            query=query, scorer=scorer, limit=limit, offset=offset, join=join, extra=extra, mode=mode,
            isolation=isolation,
            oracle=f'{select} ORDER BY score DESC, d.ctid',
            custom=f'{select} ORDER BY score DESC LIMIT {limit} OFFSET {offset}',
            regex=f'SELECT d.id, d.ctid FROM {from_clause} WHERE {regex_where}',
        )

    @staticmethod
    def parse_rows(lines):
        rows = []
        for line in lines:
            if not line:
                continue
            parts = line.split('|')
            rows.append(tuple(parts))
        return rows

    def check_rows(self, label, rows, visible, roots, membership=True):
        """Generic invariants on a ranked result: unique ids, finite scores,
        descending scores with ties in the heap order of the posting (the HOT
        chain root), membership in the snapshot's regex match set."""
        ids = [r[0] for r in rows]
        if len(set(ids)) != len(ids):
            dupes = sorted({i for i in ids if ids.count(i) > 1})
            return Failure(f'{label}: duplicate ids {dupes[:10]}')
        previous = None
        for id_, ctid, score in rows:
            value = float(score)
            if value != value or value in (float('inf'), float('-inf')):
                return Failure(f'{label}: non-finite score {score} for id {id_}')
            key = (-value, tid_key(roots.get(ctid, ctid)))
            if previous is not None and key < previous:
                return Failure(f'{label}: order violated at id {id_} ctid {ctid} '
                               f'(root {roots.get(ctid, ctid)}) score {score}')
            previous = key
            if membership and id_ not in visible:
                return Failure(f'{label}: id {id_} is not in the regex match set')
        return None

    def explain(self, reader, sql):
        out = reader.run(['EXPLAIN (FORMAT JSON) ' + sql])
        text = '\n'.join(out[0][1])
        return json.loads(text)[0]['Plan']

    def episode(self, reader):
        """One reader transaction: pin a snapshot, let writers churn, take the
        oracle and the custom-scan answer in one quiescent batch, then (for
        cursors) keep fetching while writers churn. Returns a Failure or None."""
        rng = self.rng
        spec = self.reader_query(rng)
        self.stats['episodes'] += 1
        reader.run([f"BEGIN ISOLATION LEVEL {spec['isolation']}", 'SELECT count(*) FROM docs'])
        # Writers churn while the snapshot is pinned: rows the snapshot sees are
        # deleted, updated, vacuumed and replaced by documents it cannot see.
        self.churn(rng.randint(0, 6))
        self.quiesce_writers()
        settings_oracle = ['SET LOCAL stannum.enable_custom_scan = off', 'SET LOCAL enable_seqscan = off',
                           'SET LOCAL enable_bitmapscan = on']
        settings_custom = ['SET LOCAL stannum.enable_custom_scan = on', 'SET LOCAL enable_seqscan = off',
                           'SET LOCAL enable_bitmapscan = off', 'SET LOCAL enable_hashjoin = off',
                           'SET LOCAL enable_mergejoin = off']
        declare_first = spec['mode'] != 'plain' and rng.random() < 0.5
        batch = []
        if declare_first:
            batch += settings_custom + [f"DECLARE c1 CURSOR FOR {spec['custom']}"]
        batch += settings_oracle + [spec['oracle'], spec['regex'], ROOTS_SQL] + settings_custom
        if spec['mode'] == 'plain':
            batch.append(spec['custom'])
            batch.append('EXPLAIN (FORMAT JSON) ' + spec['custom'])
        else:
            if not declare_first:
                batch.append(f"DECLARE c1 CURSOR FOR {spec['custom']}")
            batch.append(f'FETCH {rng.choice(FETCHES)} FROM c1')
        results = reader.run(batch)
        by_statement = {statement: lines for statement, lines, _ in results}
        oracle, visible, roots, failure = self.oracle_of(spec, by_statement)
        if failure:
            return failure
        expected = oracle[spec['offset']:spec['offset'] + spec['limit']]
        self.stats['comparisons'] += 1
        if spec['mode'] == 'plain':
            actual = self.parse_rows(by_statement[spec['custom']])
            plan = json.loads('\n'.join(by_statement['EXPLAIN (FORMAT JSON) ' + spec['custom']]))[0]['Plan']
            self.note_plan(plan)
            strict = not has_node(plan, 'Sort')
            return self.compare('custom scan', actual, expected, oracle, visible, roots, strict, spec)
        # Cursor modes: keep fetching with writes in between.
        actual = self.parse_rows(results[-1][1])
        self.stats['cursor_fetches'] += 1
        twin = None
        if spec['mode'] == 'twin':
            twin = self.open_twin(reader, spec, rng)
            if isinstance(twin, Failure):
                return twin
        exhausted = len(actual) < int(results[-1][0].split()[1])
        while not exhausted and rng.random() < 0.85:
            self.churn(rng.randint(0, 4), overlap_with=reader if rng.random() < 0.5 else None)
            if reader.pending is None:
                n = rng.choice(FETCHES + ['ALL'])
                reader.send([f'FETCH {n} FROM c1'])
            self.quiesce_writers()
            out = reader.wait()
            for statement, lines, error in out:
                if error:
                    return Failure(f'cursor fetch failed: {statement}', output=lines, spec=describe(spec))
            self.stats['cursor_fetches'] += 1
            rows = self.parse_rows(out[-1][1])
            actual += rows
            n = out[-1][0].split()[1]
            exhausted = n == 'ALL' or len(rows) < int(n)
            if twin and rng.random() < 0.6:
                failure = self.fetch_twin(reader, twin)
                if failure:
                    return failure
        if twin:
            failure = self.fetch_twin(reader, twin, all_rows=True)
            if failure:
                return failure
            reader.run(['CLOSE c2'])
        reader.run(['CLOSE c1'])
        strict = self.strict_for(reader, spec)
        if not exhausted:
            # Only a prefix was read; it must be a prefix of the expected rows.
            expected = expected[:len(actual)]
        return self.compare('cursor', actual, expected, oracle, visible, roots, strict, spec)

    def oracle_of(self, spec, by_statement):
        """The unpruned path's rows in the scan's promised order, the regex
        match set and the HOT root map, from one quiescent batch; checks the
        unpruned path against the regex scan first."""
        oracle = self.parse_rows(by_statement[spec['oracle']])
        visible = {r[0] for r in self.parse_rows(by_statement[spec['regex']])}
        roots = dict(self.parse_rows(by_statement[ROOTS_SQL]))
        oracle.sort(key=lambda r: (-float(r[2]), tid_key(roots.get(r[1], r[1]))))
        oracle_ids = {r[0] for r in oracle}
        if oracle_ids != visible:
            return oracle, visible, roots, Failure(
                'unpruned path and regex scan disagree on the match set', spec=describe(spec),
                only_index=sorted(oracle_ids - visible)[:20], only_regex=sorted(visible - oracle_ids)[:20])
        failure = self.check_rows('unpruned path', oracle, visible, roots)
        if failure:
            failure.detail.update(spec=describe(spec))
        return oracle, visible, roots, failure

    def open_twin(self, reader, spec, rng):
        """A second cursor in the same transaction, on the same or another
        query, with its own oracle taken right before its first fetch."""
        twin = self.reader_query(rng) if rng.random() < 0.5 else dict(spec)
        twin['mode'] = 'second cursor'
        self.churn(rng.randint(0, 3))
        self.quiesce_writers()
        batch = ['SET LOCAL stannum.enable_custom_scan = off', 'SET LOCAL enable_bitmapscan = on',
                 twin['oracle'], twin['regex'], ROOTS_SQL, 'SET LOCAL stannum.enable_custom_scan = on',
                 'SET LOCAL enable_bitmapscan = off', f"DECLARE c2 CURSOR FOR {twin['custom']}",
                 f'FETCH {rng.choice(FETCHES)} FROM c2']
        results = reader.run(batch)
        by_statement = {statement: lines for statement, lines, _ in results}
        oracle, visible, roots, failure = self.oracle_of(twin, by_statement)
        if failure:
            return failure
        twin['oracle_rows'] = oracle
        twin['visible'] = visible
        twin['roots'] = roots
        twin['expected'] = oracle[twin['offset']:twin['offset'] + twin['limit']]
        twin['actual'] = self.parse_rows(results[-1][1])
        twin['exhausted'] = len(twin['actual']) < int(results[-1][0].split()[1])
        return twin

    def fetch_twin(self, reader, twin, all_rows=False):
        if twin['exhausted']:
            return None
        n = 'ALL' if all_rows else self.rng.choice(FETCHES)
        out = reader.run([f'FETCH {n} FROM c2'])
        rows = self.parse_rows(out[-1][1])
        twin['actual'] += rows
        twin['exhausted'] = n == 'ALL' or len(rows) < int(n)
        self.stats['cursor_fetches'] += 1
        if twin['exhausted'] or all_rows:
            expected = twin['expected'] if twin['exhausted'] else twin['expected'][:len(twin['actual'])]
            return self.compare('second cursor', twin['actual'], expected, twin['oracle_rows'],
                                twin['visible'], twin['roots'], self.strict_for(reader, twin), twin)
        return None

    def strict_for(self, reader, spec):
        """Tie order is the scan's promise only when no Sort sits above it."""
        if not spec['join']:
            return True
        return not has_node(self.explain(reader, spec['custom']), 'Sort')

    def compare(self, label, actual, expected, oracle, visible, roots, strict, spec):
        self.stats['rows_compared'] += len(actual)
        failure = self.check_rows(label, actual, visible, roots)
        if failure:
            failure.detail.update(spec=describe(spec), actual=actual[:40], expected=expected[:40])
            return failure
        if strict:
            if actual != expected:
                return Failure(f'{label} differs from the unpruned path', spec=describe(spec),
                               first_difference=first_difference(actual, expected),
                               actual=actual[:40], expected=expected[:40], oracle_rows=len(oracle))
            return None
        # A Sort above the scan breaks ties arbitrarily: same scores in order,
        # each id with the score the unpruned path gave it.
        scores = {r[0]: r[2] for r in oracle}
        if [r[2] for r in actual] != [r[2] for r in expected] or any(scores.get(r[0]) != r[2] for r in actual):
            return Failure(f'{label} (tie-tolerant) differs from the unpruned path', spec=describe(spec),
                           actual=actual[:40], expected=expected[:40])
        return None

    def note_plan(self, plan):
        if has_node(plan, 'Stannum Text Search Scan', key='Custom Plan Provider'):
            self.stats['custom_plans'] += 1
        if 'Top K' in json.dumps(plan):
            self.stats['pruned_plans'] += 1

    def churn(self, steps, overlap_with=None):
        """Issues `steps` writer operations, overlapping a reader batch when
        asked, so writes land while a cursor is between fetches or mid-fetch."""
        rng = self.rng
        for _ in range(steps):
            writer = rng.choice(self.writers)
            self.step_writer(writer, quiescent=False)
            if overlap_with is not None and overlap_with.pending is None and rng.random() < 0.5:
                overlap_with.send([f'FETCH {rng.choice(FETCHES)} FROM c1'])

    # -- main loop --------------------------------------------------------------------
    def run(self):
        args = self.args
        started = time.monotonic()
        deadline = started + args.seconds
        failure = None
        try:
            self.start_cluster()
            self.build_schema()
            self.writers = [Session(f'writer{i + 1}', self.env, self.trace) for i in range(args.writers)]
            self.readers = [Session(f'reader{i + 1}', self.env, self.trace) for i in range(args.readers)]
            for writer in self.writers:
                writer.run(['SET enable_seqscan = off', self.writer_gucs()])
            for reader in self.readers:
                reader.run(['SET enable_seqscan = off'])
            episode = 0
            while time.monotonic() < deadline and failure is None:
                # Quiescent writer steps (REINDEX) only run between episodes.
                if self.rng.random() < 0.08:
                    self.quiesce_writers()
                    writer = self.rng.choice(self.writers)
                    self.step_writer(writer, quiescent=True)
                    self.finish_writer(writer)
                else:
                    self.churn(self.rng.randint(1, 3))
                reader = self.rng.choice(self.readers)
                episode += 1
                try:
                    failure = self.episode(reader)
                finally:
                    try:
                        reader.run(['COMMIT'], allow=('WARNING',))
                    except FuzzFailure:
                        pass
                if failure is None and args.stop_at and episode >= args.stop_at:
                    break
                if failure is None and episode % 25 == 0:
                    self.observe()
            self.quiesce_writers()
        except FuzzFailure as error:
            failure = Failure(str(error))
        finally:
            for session in self.writers + self.readers:
                session.close()
            self.stats['seconds'] = round(time.monotonic() - started, 1)
            report = self.report(failure)
            self.stop_cluster()
            if failure is None and not args.keep:
                shutil.rmtree(self.root, ignore_errors=True)
        return report

    def observe(self):
        try:
            info = self.sql("SELECT count(*) FILTER (WHERE kind='immutable'), count(*) FROM stannum.segment_info('docs_idx')")
            immutable, total = info.split('|')
            self.stats['folds_observed'] = max(self.stats['folds_observed'], int(immutable))
        except subprocess.CalledProcessError:
            pass

    def report(self, failure):
        args = self.args
        summary = dict(seed=args.seed, seconds=args.seconds, writers=args.writers, readers=args.readers,
                       corpus=args.corpus, status='failed' if failure else 'passed', stats=self.stats)
        if failure:
            summary['failure'] = failure.what
            summary['detail'] = {k: v for k, v in failure.detail.items()}
            repro = self.root / 'repro.sql'
            with repro.open('w') as f:
                f.write(f'-- Stannum ranked-scan fuzz failure: {failure.what}\n')
                f.write(f'-- Reproduce: python3 postgres/tests/ranked_fuzz.py --seed {args.seed} '
                        f'--seconds {args.seconds} --writers {args.writers} --readers {args.readers} '
                        f'--corpus {args.corpus}\n')
                for key, value in failure.detail.items():
                    f.write(f'-- {key}: {json.dumps(value, default=str)}\n')
                f.write('-- Statements in issue order; each session is a separate connection.\n')
                f.write('-- Server settings: autovacuum=off; PGOPTIONS extra_float_digits=3.\n')
                for session, statement in self.trace:
                    f.write(f'-- [{session}]\n{statement};\n')
            summary['repro'] = str(repro)
            (self.root / 'failure.json').write_text(json.dumps(summary, indent=2, default=str) + '\n')
        summary['artifacts'] = str(self.root)
        return summary


def describe(spec):
    return {k: v for k, v in spec.items() if k in ('scorer', 'limit', 'offset', 'join', 'extra', 'mode', 'isolation', 'oracle', 'custom', 'regex')} | {'tinql': spec['query'].tinql, 'shape': spec['query'].shape}


def first_difference(actual, expected):
    for i, (a, e) in enumerate(zip(actual, expected)):
        if a != e:
            return dict(position=i, actual=a, expected=e)
    return dict(position=min(len(actual), len(expected)), actual_len=len(actual), expected_len=len(expected))


def tid_key(ctid):
    block, offset = ctid.strip('()').split(',')
    return int(block), int(offset)


def has_node(plan, value, key='Node Type'):
    if value in str(plan.get(key, '')):
        return True
    return any(has_node(child, value, key) for child in plan.get('Plans', []))


def run_once(**overrides):
    parser = build_parser()
    args = parser.parse_args([])
    for key, value in overrides.items():
        setattr(args, key, value)
    return Fuzzer(args).run()


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--seed', type=int, default=int(time.time()) % 100000)
    parser.add_argument('--seconds', type=float, default=60)
    parser.add_argument('--writers', type=int, default=3)
    parser.add_argument('--readers', type=int, default=2)
    parser.add_argument('--corpus', type=int, default=1500, help='initial documents')
    parser.add_argument('--port', default=PORT)
    parser.add_argument('--stop-at', type=int, default=0, help='stop after this many episodes')
    parser.add_argument('--keep', action='store_true', help='keep the cluster directory on success')
    parser.add_argument('--smoke', action='store_true', help='fixed seeds and the regression list, under two minutes')
    return parser


def main():
    args = build_parser().parse_args()
    if args.smoke:
        runs = [dict(seed=1, seconds=35, writers=2, readers=2, corpus=700)] + REGRESSIONS
        results = []
        for overrides in runs:
            overrides = dict(overrides, port=args.port)
            result = run_once(**overrides)
            print(json.dumps(result, default=str))
            results.append(result)
            if result['status'] != 'passed':
                sys.exit(1)
        total = sum(r['stats']['comparisons'] for r in results)
        print(f'smoke: {len(results)} runs passed, {total} ranked comparisons')
        return
    result = Fuzzer(args).run()
    print(json.dumps(result, indent=2, default=str))
    if result['status'] != 'passed':
        sys.exit(1)


if __name__ == '__main__':
    main()
