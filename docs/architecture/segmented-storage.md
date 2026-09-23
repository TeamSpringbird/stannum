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
4. Segments merge in size tiers, like the levels of a log-structured merge
   tree, so the directory stays small without ever rewriting the whole index
   at once. VACUUM identifies dead tuple references and can rewrite segments
   to reclaim space.

Searches read both segments and the buffer, so new rows do not wait for a fold to
be searchable. Per-backend caches reuse immutable segment data and incrementally
index new buffer records (see [per-backend caches](#per-backend-caches)).
Retained document cursors keep their encoded length
array from the same buffer state. Refreshing the buffer can insert a reused heap
location before existing rows; looking up old ordinals in a new length array
would change scores midway through a ranked scan.

| Setting | Default | Purpose |
| --- | ---: | --- |
| `stannum.build_segment_docs` | 32,768 | Documents per segment during index creation |
| `stannum.write_buffer_docs` | 512 | Documents before folding the write buffer |
| `stannum.write_buffer_bytes` | 1,048,576 | Encoded forward-record bytes before folding |
| `stannum.max_merge_docs` | 1,024 | Total input documents ordinary insert merges may rewrite per fold |
| `stannum.deferred_merge_docs` | 262,144 | Input documents of the one merge an insert may run after a fold, outside the metadata lock |
| `stannum.merge_tier_factor` | 8 | Segments per size tier before they merge |
| `stannum.max_segments` | 128 | Soft bound on directory entries; 128 is the hard on-disk bound |

An index may override three of these for itself with the TIN-named storage
options: `max_mutable_segment_size` (bytes) for `write_buffer_bytes`,
`target_segment_count` for `max_segments`, and `max_merged_segment_size`
(megabytes) for the merge input ceiling. `dead_percent_threshold` sets the dead
fraction at which VACUUM rewrites a segment, 0.5 by default.

The next insert folds a nonempty buffer before appending a record that would
exceed either cap. A single document may exceed the byte cap: it remains one
record and is folded before the following insert. The two caps bound document
count and encoded input size, not elapsed time or tokenizer cost. At 2.8 KiB
per record, the byte limit now folds roughly 365 documents rather than roughly
1,460. Short documents hit the 512-document limit. Smaller folds also reduce
the amount a fresh reader must index before its first query.

### Insert preparation

An insert captures the index identity and persisted tokenizer settings under a
short shared metadata lock, then releases it before tokenizing and encoding its
forward record. Temporary token/record allocations are also dropped before the
exclusive lock is acquired. Under that lock it validates identity and settings
and uses the latest buffer and directory; unrelated appends, folds, or VACUUM do
not invalidate the prepared record. An identity/settings change retries
preparation. Current reloptions are not substituted for the persisted pipeline.

This removes text preparation from the exclusive critical section. Folding,
segment writes, foreground merges, and publication remain locked. In particular,
orphan reclamation relies on that lock to exclude unpublished foreground runs;
moving their writes outside it requires a separate reservation protocol.

### Merge policy

The [direct-merge architecture decision](../adr/0001-preserve-posting-order-before-changing-encoding.md)
records the move to preserving sorted postings during merges, with a separate
evidence gate for any SIMD-friendly on-disk format. Foreground merges now invoke
the [validated codec API](../benchmarks/hardened-merge.md) under the existing
metadata lock, retaining LSG3 output, merge selection and WAL publication. It
validates every source and merges ordered dictionaries/postings directly. Source
blobs and dead sets are retained through construction, then freed before writing
the output run. Aggregate encoded inputs or document counts beyond `u32::MAX`
use the previous reconstruction path, since deletion can still yield a
representable output. These format bounds do not impose a peak-memory cap.

PostgreSQL defers interrupts while the metadata buffer lock is held. Merge
checkpoints respect that deferral; insert checks again immediately after
publication releases the lock. An interrupted pre-publication write may leave
orphan pages, which VACUUM reclaims. VACUUM also uses the validated API for deferred
merges and deletion rewrites, retaining owned source blobs while unlocked. Its
checkpoints can deliver cancellation during construction. Decoder failures are
reported as corruption only if the identity and every captured input still match;
retired inputs cause a retry. Publication still revalidates the complete entries
and discards stale output. All-dead inputs have no successor. Oversized aggregate
inputs retain the previous reconstruction fallback. See the
[VACUUM measurements](../benchmarks/vacuum-direct-merge.md) and the
[integration measurements](../benchmarks/direct-merge-integration.md) for validation
and the limits of the performance evidence.

Each segment belongs to a size tier by document count: tier *t* holds
segments with `factor^t` to `factor^(t+1) - 1` documents. The lowest full tier
supplies `merge_tier_factor` entries for a merge. Merging skips dead documents,
publishes a fresh generation, and queues the old runs for delayed reclamation.
Generation exhaustion raises an error requiring REINDEX, rather than wrapping
and reusing a reader-cache key.

Inserts spend at most `max_merge_docs` input documents on ordinary merges
across the entire fold, including cascades. A due merge that exceeds the
remaining budget waits for VACUUM, and the directory can therefore hold more
than `factor - 1` entries in a tier. Zero defers all ordinary insert merges.
Index construction retains unrestricted tier maintenance.

After a fold has published and the metadata lock is released, the inserting
backend first frees retired runs no snapshot can still read, then merges one
due tier of at most `deferred_merge_docs` input documents the way VACUUM does:
built from a captured directory without the lock and published only if every
input is still listed. One backend does this at a time, under a heavyweight
lock on the metadata page taken conditionally; the others skip it. Only that
insert waits. Without it, a table autovacuum had not reached yet filled its
directory in about a minute at 1,000 updates a second, and the full pending
list was then freed under the metadata lock, stalling every query for half a
minute. Zero disables it.

`max_segments` is a soft bound. A directory over it merges its smallest
`entry_count - max_segments + 1` entries (normally two), which is the cheapest
set of that size and therefore the cheapest way back under the bound; an
insert performs that merge only when it fits the remaining budget, preferring
a due tier merge that fits, and otherwise lets the directory grow for VACUUM
to shrink. VACUUM merges due tiers and then the smallest entries until the
directory fits, with no budget. The on-disk directory of 128 entries is the
hard bound: an insert that would leave 129 entries merges the two smallest
whatever they cost. **That is the only unbudgeted merge.** A fixed document
ceiling is impossible alongside a fixed 128-entry directory when all 128
entries already exceed that ceiling.

Worst case: without VACUUM, folds keep adding entries; once the directory is
full, every fold merges the two smallest. While unmerged folds remain those
are two folds (1,024 documents at the default fold size), so the cost stays
at the ordinary budget; after about 128 folds every entry has doubled and the
cost doubles with it, and so on geometrically. Lowering `max_segments` below
128 makes budget-fitting merges happen earlier, keeps the directory smaller
and leaves `128 - max_segments` folds of headroom before the hard bound.
Keep VACUUM timely to avoid emergency work. These settings do not promise a
maximum wall-clock insert latency; I/O, lock waits, huge documents and other
VACUUM work still matter.

### Deferred merges and autovacuum

Large merges run from `amvacuumcleanup`. PostgreSQL already supplies per-table
maintenance scheduling, relation locking, process lifetime and error cleanup;
using it avoids a second queue, worker-slot exhaustion, dynamic-worker launch
races and a new preload requirement. No `shared_preload_libraries` entry is
needed. The segment directory itself records deferred work; restart does not
lose a maintenance queue, and the on-disk page format is unchanged.

VACUUM holds the meta lock exclusively only to publish. Every phase works
from a directory captured under a shared lock and then runs with no meta
lock at all: reading the input runs and dead lists, comparing documents with
the dead-tuple callback, building the output segment or dead list, writing
its run pages (through the FSM and the extension lock, as any writer) and
walking pending chains. Publication reacquires the lock exclusively, checks
the index identity and matches every input entry against the directory
again, complete entry including the dead-list run, then swaps in the new
entry or dead list, retires the old runs and writes the meta page: a few
page writes. Work whose inputs an insert changed meanwhile is dropped and
its pages are freed at once; the job is retried against the new directory.
Changes to other entries or the write buffer are preserved.

Reading without the lock is sound because a published entry's pages are
immutable until it is retired, retirement happens only under the exclusive
lock, an entry that has not been retired has never had a page freed, and
generations never repeat. An entry found unchanged at publication therefore
proves the bytes read were its own. A read that fails while unlocked is
reported as corruption only if the entry is still published; otherwise it
was a race with a retirement. Bulk deletion scans segments this way in up
to three rounds, so entries that inserts fold or merge during a round are
scanned in the next; whatever appears during the last round is finished
under the lock, as is the write buffer, which the fold caps keep small. No
document the callback knows dead survives in any segment.

The lock order stays meta, buffer/run pages, extension lock. Permanent-index
page writes still use generic WAL; temporary and unlogged main-fork writes
skip WAL. Readers with captured directories retain the existing
snapshot-horizon protection for retired runs. The maintenance builder uses
owned bytes while unlocked, so it does not depend on VACUUM having a reader
snapshot. A cleanup call attempts at most twice the number of directory
entries it observed on entry, so continuing inserts cannot extend its merge
loop indefinitely.

PostgreSQL 17/18 call `amvacuumcleanup` even when AUTO skips bulk deletion,
so insert-triggered autovacuum reaches deferred merges without table changes.
For a predictable maintenance cadence on insert-only or mixed workloads, use:

```sql
ALTER TABLE documents SET (
  vacuum_index_cleanup = auto,
  autovacuum_vacuum_insert_threshold = 1000,
  autovacuum_vacuum_insert_scale_factor = 0,
  autovacuum_vacuum_threshold = 1000,
  autovacuum_vacuum_scale_factor = 0
);
```

Tune these thresholds for the workload and keep server `autovacuum` and
`track_counts` enabled. PostgreSQL's default insert trigger includes a scale
factor. The AUTO bypass applies to bulk deletion, not the cleanup callback;
explicit OFF and the transaction-wraparound failsafe skip cleanup. Stannum
does not silently alter application table settings. Manual
`VACUUM (INDEX_CLEANUP ON) documents` also drains deferred tiers. With autovacuum
disabled or index cleanup disabled, inserts remain correct but eventually pay
emergency merges. See PostgreSQL's [autovacuum settings](https://www.postgresql.org/docs/18/runtime-config-vacuum.html)
and the [cleanup/bypass implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/vacuumlazy.c).

The private-cluster scenario in `docs/benchmarks/merge_lifecycle.py` checks
insert-triggered autovacuum without preload, concurrent inserts and merges,
retained readers, restart, index verification, and a crash between a run
write and its publication followed by orphan reclamation. The [merge-budget experiment](../benchmarks/merge-budget.md)
records latency, reader tails, segment counts and correctness results; the
[VACUUM publication experiment](../benchmarks/vacuum-publication.md) measures
the unlocked VACUUM and budgeted overflow merges against it.

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
  Unordered searches stream distinct candidates in heap order, using page masks
  for dense Boolean terms and scalar cursors otherwise. They do not collect or
  sort all candidate CTIDs before returning the first row. `LIMIT` stops the
  traversal after enough visible rows pass the remaining SQL filters. For
  supported ranked queries the existing scorer selects the top results.
- **Count** of a Boolean combination of plain terms folds document ordinals.
  `LSG4` segments store each term's documents as ordinals into the segment's
  TID-ordered document table: a short list for rare terms, otherwise
  65,536-document chunks that are sorted arrays or bitmaps. The count combines
  the terms' chunks word by word in fixed scratch buffers, visits only chunks
  some term occupies, clears the segment's dead documents and counts set bits.
  The visibility map is read once, after the view; if a dead list was published
  in between, the count starts over, because a page VACUUM marked all-visible
  may hold tuples the older view still lists. Matches on pages that are not
  all-visible are mapped back to TIDs through the segment's page table and
  checked a heap page at a time under one buffer lock. The write buffer and
  segments older than `LSG4` are counted through their TID postings, always
  against the heap. Per-source counts are summed: a location is live in one
  source only.
- **Ranked disjunctions** over `LSG5` segments walk the same ordinal streams
  (`stannum.rank_by_ordinal`, on by default): block-max WAND over the terms'
  chunks with the score bounds each stream stores per chunk and per
  1,024-document sub-block, and within an admitted chunk only the members of
  the essential terms are visited, those without which the rest cannot reach
  the threshold; the other terms are tested by bit. A candidate's
  term-frequency bucket is the payload entry at its rank in the term's
  stream, its length a table lookup by ordinal, and its TID is resolved only
  when it enters the top k. A conjunction is led by its rarest term through
  the chunks every term and elided filter holds, testing the shared members
  by bit. Phrases and other shapes walk the TID postings with block bounds,
  as do segments older than `LSG5` and the write buffer. `stannum.count_fold = off` selects the strategies below for
  these queries too.
- **Other counts** use page masks when a Boolean term has grouped postings averaging at
  least four tuples per occupied page; purely sparse or positional plans keep
  the scalar path. The bulk path streams exact offset masks in heap-page order.
  Dense grouped postings decode directly into five machine words; Boolean AND/OR/NOT combine
  those masks, segment dead lists are subtracted, and a streaming union removes
  cross-segment duplicates. All-visible pages use popcount when the predicate
  is exact and is the query's only restriction. Other pages retain tuple-by-tuple
  visibility checks and, where required, text rechecks. Within bulk plans,
  sparse postings and positional subexpressions adapt the existing scalar
  cursors into page masks. The bulk path does not build or sort a vector of
  every candidate CTID.

Unordered searches own their captured index view until the scan ends, including
across cursor FETCH calls. Rescans rebuild cursors against that same view, so
buffer appends and directory changes do not replace the original candidate set.
The cursor is dropped before its owning view, also on error cleanup. Encoded
postings bytes can still be fetched up front; streaming bounds decoded candidate
buffering, not all index memory or I/O. Planner startup cost continues to include
estimated index I/O and moves only candidate traversal CPU into run cost.

Reads from a segment are bounded per cursor: a payload cursor holds one span
of skip slots, an ordinal cursor one chunk, a lengths cursor one window of
16,384 documents and a postings cursor one 16 KiB window, each replaced by
the next. Those ranges are shared through a per-backend least-recently-used
cache of `stannum.read_cache_mb` (128 MiB), so the hot chunks of frequent
terms stay resident across queries while a sweep of a long stream displaces
only itself. Only headers, dictionary blocks, bounds tables and page tables
stay in the reader's arena, whose total across a backend's cached readers is
bounded by `stannum.reader_cache_mb`. Ranked queries the scorer cannot prune,
such as phrases, score the candidate stream as it arrives and keep only the
top `k`; reading past `k` rows completes the ordering as a pruned scan does.

Ranked retrieval and PostgreSQL bitmap scans retain their existing execution
strategies. The word operations are portable Rust; no architecture-specific
SIMD dispatch or on-disk format change is required.

`EXPLAIN ANALYZE` shows the chosen path and, for executed custom counts,
`Count Strategy: page bitmaps` or `scalar`. Unordered searches show
`Candidate Strategy: streaming page bitmaps` or `streaming scalar` and
`Candidates Visited`; that counter records consumed candidates, not an unknown
full cardinality when LIMIT stops early. `SET stannum.enable_custom_scan = off`
selects the PostgreSQL bitmap path for comparison.

### Per-backend caches

A query captures the directory and the buffer state under one shared meta
lock, then reads through four caches that live in the backend and key on
what the meta page says, so every backend sees the same thing without any
coordination:

- **Segment readers**, by index identity and segment generation. A reader
  keeps the byte ranges it has fetched (dictionary index, dictionary blocks,
  postings, payload, document table) for as long as the generation is in the
  directory; the readers of one backend hold at most 64 MiB of fetched bytes
  before they are all dropped. Generations never repeat within an identity,
  and REINDEX changes the identity, so a cached reader can never describe a
  different segment.
- **Dictionary lookups**, per cached segment: a term's entry or its absence,
  at most 4,096 terms per segment. A statement resolves each of its terms in
  every segment several times (planning, statistics, cursor setup), and the
  next statement repeats that. With a dozen small segments those walks over
  prefix-compressed dictionary blocks cost more than the lookups they serve,
  so the memo answers repeats without them. Segments are immutable, so the
  memo needs no invalidation of its own; it lives and dies with the reader.
- **Page tables**, by identity and generation.
- **The buffer index**, by identity and buffer epoch. It is an in-memory
  inverted index of the buffer's forward records, extended from the last
  byte it covered on each use (an insert by any backend only appends), and
  rebuilt when the epoch changes: a fold empties the buffer, and VACUUM
  rewrites it without dead records. Building costs about 11 ms per MiB of
  records on the benchmark machine, so the worst case for a fresh connection
  at the default caps is a few tens of milliseconds; existing backends absorb
  each record once, as it arrives.

The buffer index is not shared between backends. Sharing it would need a
shared-memory rendezvous (`shared_preload_libraries` or the DSM registry of
PostgreSQL 17+), a serialized form of the index, and lifetime management
across epochs and identities, to save at most one build per connection and
one per VACUUM rewrite per backend, bounded by the byte cap. The
[buffer-index measurements](../benchmarks/buffer-index.md) record that cost
and the reader cost of small folds, which is what the defaults trade against
write stalls.

### One tokenizer per clause

`document ==> 'query'` is evaluated outside the index too: by sequential
scans, by bitmap and custom-scan rechecks, and wherever else the expression
appears. On its own the operator knows nothing about the document, so it
would tokenize with the default settings and disagree with an index built
with others. A planner support function on the operator's function therefore
rewrites the clause when the document is a column or expression covered by a
`stannum` index whose predicate the query's restrictions imply: it becomes
`document ==> '{"index":<oid>,"query":"..."}'::stannum.indexed_query`, a
second operator (strategy 2 of the operator class) whose function tokenizes
with that index's settings, read from the index's meta page and compiled once
per backend. A non-constant query is wrapped as `stannum.bind_query(expr,
oid)`. `EXPLAIN` shows the bound form, and a plan holding it is invalidated
when the index changes.

The binding is deterministic: among the covering indexes the first by OID
(the order PostgreSQL lists them in) whose predicate holds. When indexes with
different settings cover the same expression, the others can still be
scanned, but `amcostestimate` prices such a scan as a full recheck and the
scan itself rechecks every row with the bound settings, so the result never
depends on the plan. `stannum.highlight` and `stannum.highlight_ansi` bind
the same way, whether the query is taken from a `==>` clause or given
explicitly, so highlights agree with matches. `stannum.score` and its
relatives already analyze `term_add` and `term_replace` with the index they
are bound to. `stannum.tokenize` and `stannum.ql_parse` take explicit
settings and are unaffected: pass the index's options to reproduce its
analysis.

Binding needs a query to plan. Everything PostgreSQL plans binds: views,
CTEs, cursors, data-modifying statements, inlined SQL functions, PL/pgSQL
statements and `EXECUTE` (through SPI), prepared statements (a generic plan
keeps the binding and is invalidated when the index changes), and
row-security `USING` and `WITH CHECK` expressions. The operator is also
evaluated by `expression_planner`, which has no query: the predicate of a
partial index (at build and on every insert), a CHECK constraint, a stored
generated column, a trigger's `WHEN` clause, an expression in extended
statistics. There `==>` means the default settings whatever index the column
has. The same holds when the document is not a column: a PL/pgSQL variable
or `NEW.body`, the argument of a SQL function that is not inlined, an
expression no index covers, a literal. Nothing resolves these at execution
time: a function sees its argument's `Var`, a range-table position, not a
table, so it cannot find the column's index, and warning on every unbound
evaluation would flag the legitimate default-settings uses above. The one
place the mismatch is cheap to detect is a `stannum` index built with other
settings whose own predicate holds a `==>` clause, where the index's
settings would be expected: `CREATE INDEX` and `REINDEX` raise a WARNING.

A partial index whose predicate is a `==>` clause therefore holds the rows
the default settings match, and only a query clause meaning the default
settings can prove it. Before the planner examines a relation's indexes, a
`get_relation_info` hook rewrites such a predicate clause into the bound
form of the query's matching clause when that clause is bound to an index
with the default settings; `predicate_implied_by` then sees equal clauses
and the partial index, of any access method, is usable (for partitions, the
parent's clause is translated to the partition first). A clause bound to an
index with other settings never proves it: a partial index with other
settings and a `==>` predicate is still bound to when it is the first
covering index by OID, but is never scanned for that clause.

## Ranking

BM25 scoring reads term frequencies, document lengths, and corpus statistics from
the index. `stannum.score` can elide very common terms; `stannum.full_score` keeps
them. `stannum.max_score` supplies a normalization value using the associated
scoring policy. Calls must bind to a matching `==>` predicate at the same query
level.

Scoring statistics include buffered documents immediately and retain dead
documents until a segment rewrite. Planner estimates instead subtract known
segment dead lists: exact dead-posting subtraction for terms with at most 1,024
postings, and live-fraction scaling for more common terms and their expansions.
DELETE alone does not populate these lists; VACUUM must first identify dead
versions. Estimates cannot account for those unknown deaths beforehand. The
write buffer has no dead list. Partial indexes use their indexed population.

Ranked-path recognition requires the search predicate and its bound scoring
call to expose the same constant query or the same external text parameter.
The scoring support function simplifies the query copied from the parse tree
for custom plans. Generic ranked plans retain a parameter expression in
`custom_exprs`, allowing PostgreSQL to track it during plan finalization.
The executor binds it on first access; NULL returns no rows, and rescans clear
ranked rows and the scan-owned scorer before rebinding. Plain EXPLAIN does not
evaluate the parameter. Unknown queries use the existing fallback cost estimate.

Arbitrary query expressions, correlated parameters, dynamic scoring settings,
and parameterized unordered/count custom scans retain their previous paths.
A runtime LIMIT remains correct but cannot supply the planner's constant top-k
bound. PostgreSQL still chooses between custom and generic plans normally.
See the [generic prepared-plan follow-up](../benchmarks/generic-ranked-plans.md)
and the [original diagnosis](../benchmarks/ranked-prepared-queries.md).

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
current k-th row. Conjunctions additionally use the largest of all current blocks' minimum
document lengths for every term, combined with each bucket's own minimum. The
shared bound is cached through the nearest block end and can skip the rarest
term before another intersection walk. The result is therefore identical to
scoring every candidate:
same rows, same scores, same tie order. `EXPLAIN ANALYZE` reports `Pruning:
block-max` and the number of candidates actually scored. Phrase, positional,
expansion, `NOT` and `AT LEAST` queries, and limits above 4,096 rows, score
every candidate as before; so does a query over segments written before block
bounds existed. Should the parent read past the limit (for example because
top rows were deleted), the scan scores every candidate and continues with the
rows it has not emitted yet; documents indexed since the top k was built can
rank into the completed ordering, so it is not resumed by position.

A ranked scan keeps its scorer for as long as it lives, under its own
identity, with the score of every row it ranked: a cursor fetched across
later statements, or two scans on one query open at once, each report the
scores they ranked by even as writes move the statistics. A HOT-updated row
is posted at its chain root; the score functions resolve the visible member's
location to that root. `docs/testing.md` describes the concurrency fuzzer
that checks this against the unpruned path and the bug classes it found.

## Durability and maintenance

Logged indexes use PostgreSQL's generic write-ahead log. A metadata page tracks
the buffer, segment directory, and runs awaiting reclamation. Structural changes
hold its exclusive lock; readers copy the directory under a shared lock.

Segments are immutable. Retired pages become reusable only after PostgreSQL's
visibility horizon makes reuse safe for readers with older snapshots. With the
extension preloaded, freeing pages first logs a removal horizon through a
custom WAL resource manager so hot standbys resolve the same conflict on replay.
VACUUM records dead tuples, rewrites sufficiently dead segments, and reclaims
pages.
`stannum.segment_info('index_name')` exposes the segment layout for inspection.

Reclamation publishes first and frees second: VACUUM walks the chains of the
pending runs no snapshot can still read, removes those entries from the meta
page under the exclusive lock (each matched exactly against what it walked,
so an entry an insert coalesced more runs into meanwhile waits for the next
VACUUM), and marks their pages FREE afterwards. A crash between the two, like
a crash between writing a run and publishing it, leaves pages that nothing
references and that are not FREE. VACUUM's cleanup reclaims such orphans: it
computes every page the captured directory references (page 0, each entry's
run through its page table, the page-table and dead-list chains, the whole
buffer chain, pending runs up to their recorded lengths), reads the kind of
every other page that existed at the capture, and keeps the ones not marked
FREE as candidates. Under a shared meta lock, which no writer can hold a
half-written run beneath, it walks only what changed since the capture and
confirms the candidates the current directory still does not reference; it
frees them after releasing the lock. No snapshot can reference such a page:
a reader's directory holds only published entries, a retired entry stays
referenced through the pending list until reclaimed, only FREE pages are ever
allocated, and a crash ends every session. The number reclaimed is written
to the server log.

The page and segment format signatures are `LDP2` and `LSG5`. Their definitions
live in `postgres/src/storage/layout.rs` and the `segment` crate. `LSG2` added
per-block score bounds to term postings and fixed-width payload skip offsets.
`LSG3` keeps the same bounds in less space: a term whose postings fit one
block (128 postings, the vast majority of a vocabulary) stores a single term
bound without the per-block last location and byte offset, the payload skip
table omits the always-zero slot for entry 0, and dictionary entries store
each term's extents as gaps from the previous term's (zero, since streams
are laid out back to back) with `df` and `max_tf_bucket` packed into one
varint. `LSG4` keeps those streams and adds, per term, the ordinal stream
Boolean counts fold, and per segment a page table from heap block to first
ordinal. `LSG5` adds to every ordinal stream a score bound per chunk and per
sub-block, in the block bound's encoding plus a byte per sub-block, so ranked
disjunctions prune over the ordinals. `LSG4`, `LSG3`, `LSG2` and `LSG1`
segments are still read: Boolean counts over `LSG4` fold ordinals and over
older formats use TID postings; ranked scans over `LSG4` and earlier walk the
TID postings, pruning over `LSG2` exactly as over `LSG3`, and over `LSG1`
scoring every candidate.
Unsupported old formats require rebuilding the index.
`script/dump-segments.py` writes an index's segment blobs to files and
`cargo run -p segment --release --example breakdown -- --reencode <blobs>`
reports where their bytes go, by section and by term document frequency.

## Checking an index

Readers validate what they touch and fail with `ERROR: ... REINDEX required`
(SQLSTATE `XX002`, index corrupted) naming the page, segment generation or
buffer involved. That is the right behavior for a query, but it reports one
problem, only when a query happens to read it. `stannum.verify_index` walks
the whole index instead and lists every inconsistency it finds, in the spirit
of PostgreSQL's `amcheck`:

```sql
SELECT * FROM stannum.verify_index('documents_search');
SELECT * FROM stannum.verify_index('documents_search', heap_check => true);
```

It returns `(severity, location, message)` rows; no rows means the index is
consistent. `severity` is `error` when a reader can fail or return wrong
results and `warning` when every reader copes but something is off. The check
holds a `ShareLock` on the index and its table (`AccessShareLock` on a
standby), so queries proceed while inserts, folds, merges and VACUUM wait for
it; it reads every page once and never uses the per-backend caches. With
`heap_check`, it also scans the table: every live location in the index must
point at a heap line pointer that exists (visibility aside), and every
visible row with at least one token must be in the index. Rows whose indexed
value is NULL or has no tokens are not required, since folds drop empty
documents.

What is checked:

- the meta page: page kind and layout version, the tokenizer spec, every
  directory entry (generation numbering, run shapes, page table present),
  the pending-free list and the write-buffer counters;
- every segment run, page table run and dead-list run: page kinds, chain
  length, every page but the last full, byte counts, and the page table
  listing exactly the chain's pages;
- every segment blob: header, dictionary block index and blocks in order,
  each term's postings and payload extents inside their areas and not
  overlapping, postings sorted and all present in the document table, the
  payload holding one entry per posting with a bucket that matches its
  positions, `max_tf_bucket`, block bounds equal to what the postings and
  document lengths imply, document lengths nonzero, summing to the header's
  total and equal to the positions the terms hold for each document;
- every dead list: decodes, sorted, a subset of its segment's documents, and
  the directory entry's document count and total length match the blob;
- the write buffer: page kinds, full pages before the tail, tail state
  matching the byte count, record framing, one record per counted document
  and no location recorded twice;
- pending-free runs: chains of run pages, nothing already `FREE`;
- page accounting: no page referenced twice, no referenced page marked
  `FREE`, no live document in two sources, and pages nothing references.

### Operator guide

`location` says where a finding is; the first words name its class.

| Location starts with | Meaning | Remedy |
| --- | --- | --- |
| `meta page` | Page 0 is unreadable, has the wrong kind or version, its tokenizer spec does not decode, or the buffer counters contradict each other. Every read of the index fails. | `REINDEX` |
| `directory entry N` | A generation number repeats or is not below the next one. Per-backend caches key on generations, so readers can serve the wrong segment. | `REINDEX` |
| `segment generation G run` / `page table` / `dead list` | The chain of pages holding that blob is broken: a page has the wrong kind, is marked `FREE`, belongs to something else, holds too few bytes, or the page table disagrees with the chain. | `REINDEX` |
| `segment generation G, ...` | The blob decoded from those pages is inconsistent inside: header, dictionary, a term (`term "x"`), a document (`document (block,offset)`), the document table or the dead list. Queries touching that term or document fail or return wrong rows. An `LSG1` warning means the segment predates block bounds and ranked scans over it score every candidate. | `REINDEX`; for the `LSG1` warning only if pruning matters |
| `write buffer` | The buffer chain or its records are unreadable, or the counters in the meta page disagree with the stream. Inserts and every search fail. | `REINDEX` |
| `pending entry N` (warning) | A run awaiting reclamation is shorter than recorded; the pages past the break are unreferenced. Harmless to queries. | `VACUUM` reclaims the entry and the orphaned remainder |
| `page N` (warning) | A page nothing references and not marked `FREE`: typically leaked by a crash between writing a run and publishing it. Harmless to queries. | `VACUUM` reclaims it |
| `heap` | A visible row with tokens is missing from the index, so searches miss it. | `REINDEX` |
| any source, `document (b,o) points ...` | The index holds a location the heap no longer has (beyond its end or an unused line pointer), so VACUUM missed a deletion. Searches may return wrong rows after the slot is reused. | `REINDEX` |
| any source, `document (b,o) is also live in ...` | One location is live in two segments or in a segment and the buffer; a search can return it twice and scores add up. | `REINDEX` |

In short: every `error` means `REINDEX`, because the on-disk structure no
longer describes the table and nothing rewrites a segment in place.
`VACUUM` is the answer to things `verify_index` does not report as errors:
dead documents still counted in segment statistics (`dead_docs` in
`stannum.segment_info`), runs waiting on the pending-free list, pages nothing
references, and a directory holding empty segments (a warning), all of which
VACUUM's dead lists, rewrites and reclamation take care of. If the index is
inconsistent, the table is the source of truth; `REINDEX` rebuilds from it.

## Current limits

- `==>` means the default tokenizer settings wherever no query is planned
  around it (partial-index predicates, CHECK constraints, generated columns,
  trigger `WHEN` clauses, statistics expressions) and wherever the document
  is not a column a `stannum` index covers (PL/pgSQL variables, non-inlined
  function arguments, uncovered expressions, literals). A partial index with
  other settings and a `==>` predicate holds the rows the defaults match, is
  never scanned for that clause, and warns when built. See "One tokenizer
  per clause".
- Legacy zero-page indexes use the slower reference path. Hot standbys use
  the segmented path only when the extension is preloaded on the primary and
  the standby (removal-horizon WAL records, see
  [recovery](recovery-and-parallel.md#standby-selective-reads)); otherwise
  they use the reference path.
- Unordered custom scans are worker-safe but do not split a scan across workers.
- Fresh connections rebuild their own buffer index. Large buffers increase
  first-query latency; connection pooling amortizes that work.
- Folding and insert-side merging hold the meta lock for their duration, so
  concurrent inserts and readers wait for them. VACUUM holds it exclusively
  only to publish (a few page writes per merge, rewrite, dead list or
  reclamation), plus the scan of whatever inserts folded during its last
  unlocked round and the rewrite of the write buffer without dead records.
- The pending-free list holds 64 entries; runs released together share one.
  A full list first frees runs no snapshot can still read and otherwise
  appends to its newest entry, delaying that entry's reclamation. A crash
  before a new run is published, or between removing a reclaimed pending
  entry and freeing its pages, leaves orphaned pages that
  `stannum.verify_index` lists as `page N` warnings until the next VACUUM
  cleanup reclaims them.
- Ordinary insert merges have a document budget, and so do merges that bring
  the directory back under `max_segments`. Only the merge that keeps the
  directory within its 128-entry on-disk bound is unbudgeted; its cost grows
  geometrically with the number of folds VACUUM has missed (see Merge
  policy).

Use the tests listed in the [project README](../../README.md#validate-changes)
when changing these paths. Performance evidence and its limitations are kept in
[the benchmark summary](../benchmarks/README.md).

See [recovery, relation persistence and parallel execution](recovery-and-parallel.md)
for temporary/unlogged index support and the standby safety boundary.
