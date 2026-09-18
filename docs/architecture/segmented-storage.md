# How Stannum works

Stannum is a PostgreSQL index access method for text search. PostgreSQL owns the
rows, transactions, and visibility rules. Stannum stores searchable tokens and
ranking statistics in index pages and uses them to find matching row locations.

## Code map

| Directory | Responsibility |
| --- | --- |
| `postgres/src/` | PostgreSQL integration, SQL functions, query planning, and scoring |
| `postgres/src/storage/` | Index pages, write buffer, segments, WAL, and reclamation |
| `segment/src/` | Dictionaries, postings, positions, document lengths, and cursors |
| `tinql/src/` | Query parsing, reference evaluation, and indexed query planning |
| `tokenizer/src/` | Text normalization and token positions |
| `boldi-vigna/src/` | Integer encoding used by the storage codecs |
| `benchmarks/` | Workload generation, correctness checks, and performance recording |

## Writing an index

An index contains a mutable **write buffer** and immutable **segments**. Each
segment maps terms to PostgreSQL tuple locations (`ctid`) and stores token
positions, term frequencies, and document lengths.

1. Index creation tokenizes existing rows into segments.
2. Inserts append a document's tokens to the write buffer. Updates that change
   indexed content add a new tuple version; PostgreSQL controls its visibility.
3. When the buffer fills, the inserting backend converts it into a segment.
4. When the segment limit is reached, segments merge. VACUUM identifies dead
   tuple references and can rewrite segments to reclaim space.

Searches read both segments and the buffer, so new rows do not wait for a fold to
be searchable. Per-backend caches reuse immutable segment data and incrementally
index new buffer records.

| Setting | Default | Purpose |
| --- | ---: | --- |
| `stannum.build_segment_docs` | 32,768 | Documents per segment during index creation |
| `stannum.write_buffer_docs` | 16,384 | Documents before folding the write buffer |
| `stannum.max_segments` | 128 | Segment count before merging |

The write buffer also folds at 4 MiB. Folding and merging happen synchronously,
so the insert that triggers them can take longer.

## Reading an index

The query is parsed as TINQL, tokenized using the index's settings, and compiled
into cursors over each segment and the buffer. Cursors combine postings and
positions for Boolean, phrase, proximity, and positional queries.

Wildcard, regex, range, and fuzzy queries expand terms from the dictionary.
Expansions beyond 1,024 terms use conservative candidates and recheck the query
against row text.

PostgreSQL can execute these candidates through a bitmap scan or Stannum's
custom scan nodes:

- **Text Search Scan** checks tuple visibility and any remaining SQL filters.
  For supported ranked queries it scores candidates and selects the top results.
- **Count** skips heap reads on all-visible pages when the search predicate is
  exact and is the query's only restriction.

`EXPLAIN ANALYZE` shows the chosen path. `SET stannum.enable_custom_scan = off`
selects the bitmap path for comparison.

## Ranking

BM25 scoring reads term frequencies, document lengths, and corpus statistics from
the index. `stannum.score` can elide very common terms; `stannum.full_score` keeps
them. `stannum.max_score` supplies a normalization value using the associated
scoring policy. Calls must bind to a matching `==>` predicate at the same query
level.

Statistics include buffered documents immediately. Dead documents remain in
segment statistics until rewriting removes them. Partial indexes use their
indexed population. These choices affect scores as well as performance.

A ranked scan with a known `LIMIT` prunes instead of scoring every candidate
when the query is a flat `AND` or `OR` of terms (a single term included) whose
terms are exactly the scoring terms. Each term's postings carry a bound per
block of 128 postings: the largest term-frequency bucket, the smallest document
length and the block's last location. The scan walks the sources in tuple
order with one cursor per term, keeps the k-th best score as a threshold, and
skips every run of postings whose summed block bounds cannot reach it
(block-max WAND). Bounds are evaluated at each block's minimum length and over
every bucket up to its maximum, and summed in the scorer's term order, so
rounding never puts a bound below a score it covers; a run whose bound equals
the threshold is skipped only when every location in it sorts after the
current k-th row. The result is therefore identical to scoring every candidate:
same rows, same scores, same tie order. `EXPLAIN ANALYZE` reports `Pruning:
block-max` and the number of candidates actually scored. Phrase, positional,
expansion, `NOT` and `AT LEAST` queries, and limits above 4,096 rows, score
every candidate as before; so does a query over segments written before block
bounds existed. Should the parent read past the limit (for example because
top rows were deleted), the scan scores every candidate and continues from the
same position.

## Durability and maintenance

Logged indexes use PostgreSQL's generic write-ahead log. A metadata page tracks
the buffer, segment directory, and runs awaiting reclamation. Structural changes
hold its exclusive lock; readers copy the directory under a shared lock.

Segments are immutable. Retired pages become reusable only after PostgreSQL's
visibility horizon makes reuse safe for readers with older snapshots. VACUUM
records dead tuples, rewrites sufficiently dead segments, and reclaims pages.
`stannum.segment_info('index_name')` exposes the segment layout for inspection.

The page and segment format signatures are `LDP2` and `LSG2`. Their definitions
live in `postgres/src/storage/layout.rs` and the `segment` crate. `LSG2` adds
per-block score bounds to term postings and fixed-width payload skip offsets;
`LSG1` segments are still read, and ranked scans over them score every
candidate. Unsupported old formats require rebuilding the index.

## Current limits

- Nondefault tokenizer settings can produce different matches in indexed and
  sequential scans. The sequential path still uses the default tokenizer.
- Standby/recovery reads and temporary or unlogged indexes use slower reference
  paths rather than the normal segmented search path.
- Custom scans do not use parallel workers.
- Fresh connections rebuild their own buffer index. Large buffers increase
  first-query latency; connection pooling amortizes that work.
- Folding, merging, and VACUUM can hold up concurrent operations.
- The pending-free list holds 64 runs. Overflow, or a crash before a new run is
  published, can leave pages unreclaimed until REINDEX.

Use the tests listed in the [project README](../../README.md#validate-changes)
when changing these paths. Performance evidence and its limitations are kept in
[the benchmark summary](../benchmarks/README.md).
