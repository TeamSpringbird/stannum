# How Stannum works

Stannum is a PostgreSQL index access method for text search. PostgreSQL owns the
rows, transactions, and visibility rules. Stannum stores searchable tokens and
ranking statistics in index pages and uses them to find matching row locations.

## Code map

| Directory | Responsibility |
| --- | --- |
| `postgres/src/` | PostgreSQL integration, SQL functions, query planning, and the score functions |
| `engine/src/` | The query engine without PostgreSQL: BM25, the ranked walk over ordinals, the count fold |
| `bench/` | The engine measured outside PostgreSQL: trace replay over dumped segments, kernel benchmarks ([how](../offline-engine.md)) |
| `postgres/src/storage/` | Index pages, write buffer, segments, WAL, and reclamation |
| `segment/src/` | Dictionaries, ordinal streams, the document table, positions, document lengths, and cursors; `tinshape/` holds the ctid-addressed format the extension writes and reads ([TIN shape](tin-shape.md)), which the ordinal codecs still serve through translation |
| `tinql/src/` | Query parsing, reference evaluation, and indexed query planning |
| `tokenizer/src/` | Text normalization and token positions |
| `boldi-vigna/src/` | Minimal-interval evaluation of phrases, proximity and span operators over token positions |
| `benchmarks/` | Workload generation, correctness checks, and performance recording |

## Writing an index

An index contains a mutable **write buffer** (TIN's mutable write segment), at
most two **sealed write segments** and immutable **segments**. Each segment
maps terms to PostgreSQL tuple locations (`ctid`) and stores token positions,
term frequencies, and document lengths, in TIN's shape ([TNS1](tin-shape.md)).

1. Index creation tokenizes existing rows into segments (origin `build`).
2. Inserts append a document's tokens to the write buffer. Updates that change
   indexed content add a new tuple version; PostgreSQL controls its visibility.
3. When the buffer fills it is **sealed** in place: its chain of pages stops
   taking documents and joins the sealed list, and a fresh page starts the
   next buffer. Sealing writes one page under the meta lock and copies
   nothing.
4. A sealed segment is **promoted** into an immutable segment (origin
   `promotion`): by a background worker when one can take it, otherwise by
   the inserting backend after it has published the seal. In manual
   maintenance mode it waits for `stannum.promote()` or VACUUM, except that
   a third seal promotes the oldest first, as TIN does.
5. Segments merge in size tiers, like the levels of a log-structured merge
   tree, so the directory stays small without ever rewriting the whole index
   at once (origin `merge`). VACUUM identifies dead tuple references and can
   rewrite segments to reclaim space.

Searches read segments, sealed segments and the buffer, so new rows do not
wait for a promotion to be searchable. Sealed segments and the buffer count
towards BM25's statistics but not towards elision, which counts immutable
segments only: TIN's rule, so a word in every row of a write segment is
scored until the segment is promoted. Per-backend caches reuse immutable
segment data, build each sealed segment's in-memory index once and
incrementally index new buffer records (see [per-backend
caches](#per-backend-caches)).
Retained document cursors keep their encoded length
array from the same buffer state. Refreshing the buffer can insert a reused heap
location before existing rows; looking up old ordinals in a new length array
would change scores midway through a ranked scan.

| Setting | Default | Purpose |
| --- | ---: | --- |
| `stannum.build_segment_docs` | 32,768 | Documents per segment during index creation |
| `stannum.write_buffer_docs` | 12,288 | Documents before the write buffer is sealed (where TIN seals one of short documents) |
| `stannum.write_buffer_bytes` | 4,194,304 | Encoded forward-record bytes before the write buffer is sealed (TIN's `max_mutable_segment_size`) |
| `stannum.max_merge_docs` | 1,024 | Total input documents ordinary merges may rewrite under the lock as a promotion publishes |
| `stannum.deferred_merge_docs` | 262,144 | Input documents of the one merge an insert may run after promoting, outside the metadata lock |
| `stannum.merge_tier_factor` | 8 | Segments per size tier before they merge |
| `stannum.max_segments` | 96 | Soft bound on directory entries; 96 is the hard on-disk bound |

An index may override three of these for itself with the TIN-named storage
options: `max_mutable_segment_size` (bytes) for `write_buffer_bytes`,
`target_segment_count` for `max_segments`, and `max_merged_segment_size`
(megabytes) for the merge input ceiling. `dead_percent_threshold` sets the dead
fraction at which VACUUM rewrites a segment, 0.5 by default.

The next insert seals a nonempty buffer before appending a record that would
exceed either cap. A single document may exceed the byte cap: it remains one
record and is sealed before the following insert. The two caps bound document
count and encoded input size, not elapsed time or tokenizer cost. At 2.8 KiB
per record the byte limit seals roughly 1,500 documents (TIN 1.0.4 seals about
2,000 short rows by size); short documents hit the 12,288-document limit
first, where TIN 1.0.3 and 1.0.4 seal the rows of `catalog.S-07f`.

Promotion is built from the sealed chain without the meta lock, under the
maintenance lock, and published in one meta page write that adds the
segment, retires the sealed chain to the pending list (whose reclamation
frees buffer pages as it frees runs) and spends the budgeted merges. A
promotion whose sealed segment VACUUM rewrote meanwhile is discarded and
retried. `stannum.promote(index, extent_cap_bytes)` splits each sealed
segment into segments of about that many bytes of forward records.

### Insert preparation

An insert captures the index identity and persisted tokenizer settings under a
short shared metadata lock, then releases it before tokenizing and encoding its
forward record. Temporary token/record allocations are also dropped before the
exclusive lock is acquired. Under that lock it validates identity and settings
and uses the latest buffer and directory; unrelated appends, folds, or VACUUM do
not invalidate the prepared record. An identity/settings change retries
preparation. Current reloptions are not substituted for the persisted pipeline.

Text preparation is therefore outside the exclusive critical section. Folding,
its segment write, the budgeted merges of a fold, and publication remain
locked. Every run written without the meta lock and not yet published (a
deferred merge, a merge that makes room in a full directory, a build's
merges) is written under the maintenance lock instead, which orphan
reclamation also takes, so it never frees such a run.

### Merge policy

A merge walks its inputs' sorted dictionaries and merges each term's ordinal
streams directly, dropping dead documents and recomputing statistics and score
bounds, rather than rebuilding documents and sorting them again. A fold's
budgeted merges call the segment crate's validated direct-merge API under the
metadata lock. It validates every source before reusing any of it. Source blobs and dead
sets are retained through construction, then freed before writing the output
run. Aggregate encoded inputs or document counts beyond `u32::MAX` fall back to
rebuilding documents from the inputs, since deletion can still yield a
representable output. These format bounds do not impose a peak-memory cap.
An unlocked merge's output is the same segment as a locked one's, byte for
byte, so a build writes the same index either way.

PostgreSQL defers interrupts while the metadata buffer lock is held. Merge
checkpoints respect that deferral; insert checks again immediately after
publication releases the lock. That is why only the budgeted merges run
there: at most `max_merge_docs` input documents. Every other merge (VACUUM's,
an insert's deferred merge and its merge to make room in a full directory,
and every merge of an index build) uses the validated API on owned source
blobs without the lock, so its checkpoints deliver a cancel or a termination
during construction. An interrupted pre-publication write may leave orphan
pages, which VACUUM reclaims. Decoder failures are
reported as corruption only if the identity and every captured input still match;
retired inputs cause a retry. Publication revalidates the complete entries and
discards stale output. All-dead inputs have no successor. Oversized aggregate
inputs fall back to rebuilding documents, as foreground merges do.

A merge takes at most 3 GiB of input (`SEGMENT_BYTES_CAP`, or the index's
`max_merged_segment_size` when smaller), dropping its largest members until it
fits, because a run records its length in 32 bits; writing a longer segment is
an error rather than a wrapped length.

An index build ends by compacting its directory: the smallest segments that
fit one run together under the segment byte cap are merged, repeatedly, so
the build leaves the fewest segments the cap allows rather than the leftovers
of every tier. Every query pays a dictionary lookup and a stream head per
term per segment, which is what the compaction buys back. The build then packs
its live runs into the lowest pages of the relation, marks the pages it reused
as used in the free space map, and truncates the rest: tier merges retire
about as many pages as they keep, and freed pages are reusable but never
returned to the operating system, so without packing a built index would keep
every page its merges retired.

A build merges and packs without the meta lock, publishing each merge as
VACUUM does and taking the lock only for that, so canceling or terminating a
`CREATE INDEX` or `REINDEX` takes effect at the next merge checkpoint rather
than after minutes of merging up to the segment byte cap. No other backend
writes an index being built (plain builds lock it, and a concurrent build's
index takes no inserts until it is ready), so its merges always publish.

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
insert waits. Without it, a table that autovacuum has not reached yet fills
its directory quickly under sustained updates, and the full pending list is
then freed under the metadata lock, stalling every query. Zero disables it.

With `shared_preload_libraries = 'stannum'`, a fold in the default
`background` maintenance mode spends no merge budget under the lock and
queues the index for a background maintenance worker instead of running the
deferred merge; the worker merges and reclaims without a document budget, as
VACUUM does. Inserts fall back to the inline merges above when no worker can
take the job. `stannum.index_maintenance_mode = manual` leaves these merges
to VACUUM and `stannum.merge()`. See
[maintenance workers](maintenance-workers.md).

`max_segments` is a soft bound. A directory over it merges its smallest
`entry_count - max_segments + 1` entries (normally two), which is the cheapest
set of that size and therefore the cheapest way back under the bound; an
insert performs that merge only when it fits the remaining budget, preferring
a due tier merge that fits, and otherwise lets the directory grow for VACUUM
to shrink. VACUUM merges due tiers and then the smallest entries until the
directory fits, with no budget. The on-disk directory of 96 entries is the
hard bound: an insert that would fold into a full directory first merges its
two smallest entries whatever they cost. **That is the only unbudgeted merge
an insert performs.** It runs without the meta lock, under the maintenance
lock, the way a deferred merge does, so readers and other writers proceed
and a cancel stops it; the insert then takes the lock again, and makes room
again should a concurrent fold have taken it. A fixed document
ceiling is impossible alongside a fixed 96-entry directory when all 96
entries already exceed that ceiling.

Worst case: without VACUUM, folds keep adding entries; once the directory is
full, every fold merges the two smallest. While unmerged folds remain those
are two folds (1,024 documents at the default fold size), so the cost stays
at the ordinary budget; after about 96 folds every entry has doubled and the
cost doubles with it, and so on geometrically. Lowering `max_segments` below
96 makes budget-fitting merges happen earlier, keeps the directory smaller
and leaves `96 - max_segments` folds of headroom before the hard bound.
Keep VACUUM timely to avoid emergency work. These settings do not promise a
maximum wall-clock insert latency; I/O, lock waits, huge documents and other
VACUUM work still matter.

### Deferred merges and autovacuum

Large merges run from `amvacuumcleanup`. PostgreSQL already supplies per-table
maintenance scheduling, relation locking, process lifetime and error cleanup,
so no `shared_preload_libraries` entry is needed for an index to stay in
shape. Preloading adds background workers that take an insert's merges (see
[maintenance workers](maintenance-workers.md)); VACUUM's cleanup still runs
its own. The segment directory itself records deferred work, so restart does
not lose it: the workers' queue holds only wakeups.

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

The lock order is meta, buffer/run pages, extension lock. Permanent-index
page writes use generic WAL; temporary and unlogged main-fork writes skip
WAL. Readers with captured directories are protected by the snapshot horizon
that delays reuse of retired runs. The maintenance builder uses
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

A private-cluster merge lifecycle scenario checks insert-triggered autovacuum
without preload, concurrent inserts and merges, retained readers, restart,
index verification, and a crash between a run write and its publication
followed by orphan reclamation; [testing](../testing.md) lists it with the
other suites.

## Segment format

Index pages carry the signature `LDP2` with page layout version 4, and
segments the signature `TNS1`, the ctid-addressed format of [the TIN-shape
guide](tin-shape.md). The page layouts are defined in
`postgres/src/storage/layout.rs` and the segment layout in the `segment`
crate (`segment::tinshape`). Every page is a standard PostgreSQL page whose
special area names its kind: the meta page (block 0: tokenizer settings,
write-buffer state, the sealed write segments, the segment directory with
each entry's origin, and runs awaiting reclamation), write-buffer pages
(the buffer's and the sealed segments' chains), run pages holding an
immutable blob (a segment, its page table or a dead list), and free pages.
An index of an earlier page layout (version 2 held `STN3` segments) is not read: `REINDEX`
rebuilds it.

A `TNS1` segment's postings are sets of ctids, laid out as [the TIN-shape
guide](tin-shape.md#the-format) describes. Its documents' rank in ctid order
is what the ordinal format called an ordinal, and the extension reads a
segment through `segment::tinshape::index::Reader`, which hands each term out
as an ordinal stream translated once per backend, so the planner, the
Boolean cursors, the ordinal walk and the count fold below read either
format. A dead list (the published liveness of a segment) is an ordinal
stream of dead ranks.

The rest of this section describes `STN3`, the ordinal format the
translation produces and the write buffer's in-memory index still encodes.

```text
blob := "STN3", doc_count, total_length, section lengths,
        dictionary, ordinals area, positions area,
        offsets, lengths, classes, pages
```

Documents are numbered in heap order, and that ordinal addresses everything
per document:

- **Dictionary.** Terms sorted by their UTF-8 bytes in prefix-compressed
  blocks of 64, with a block index of first terms for binary search. An
  entry holds the term's document frequency and largest term-frequency
  bucket packed into one varint, and the extents of its two streams as gaps
  from the previous term's (zero, since streams are laid out back to back),
  so the dictionary alone answers selectivity and score-bound questions.
- **Ordinal stream**, one per term: the term's documents, which is the only
  representation of its document set. At most 64 documents are a delta list
  with one score bound. Longer streams are 65,536-document chunks, each a
  sorted array of 16-bit offsets or, from 1,024 members, a bitmap, with a
  directory entry and a score bound per chunk. A bound names the
  term-frequency buckets that occur with the shortest document per bucket,
  and per 1,024-document sub-block the largest bucket there. Each member's
  bucket is stored beside it as a nibble, so scoring never reads positions.
- **Positions stream**, one per term: each document's token positions,
  addressed by the document's rank in the term's ordinal stream through a
  skip table every 32 entries. Only phrases and other positional shapes read
  it.
- **Document table**: a two-byte heap offset per document, and a page table
  of (heap block, first ordinal) pairs. An ordinal becomes a tuple location,
  and a location an ordinal, by a binary search of the page table and one
  offset read.
- **Lengths**: four bytes per document, and a one-byte **length class** per
  document that names the shortest length it covers, so a ranked walk can
  bound a candidate before it reads the exact length.

A dead list is an ordinal stream without bounds or buckets. Segments with any
other signature are not read; `REINDEX` rebuilds them in the current format
(see [releasing](../RELEASING.md#on-disk-compatibility)).

`script/dump-segments.py` writes an index's segment blobs to files and
`cargo run -p segment --release --example breakdown -- --reencode <blobs>`
reports where their bytes go, by section and by term document frequency.

### Why this layout

The layout is sized for an index larger than shared buffers, where the cost
of a query is the pages it reads. A term's documents are stored once, as
ordinals, so a Boolean count is a word-wise fold over chunks rather than a
visit per match ([ADR 0003](../adr/0003-address-postings-by-document-ordinal.md))
and a ranked walk prunes whole chunks by their bounds
([ADR 0004](../adr/0004-rank-by-document-ordinal.md)). The bucket beside each
member keeps scoring out of the positions stream, visibility for all-visible
pages comes from the visibility map rather than a heap fetch per candidate,
and the length class bounds a candidate before its exact length is read. On
the 150 million row Stack Exchange corpus the index is 47 GB, and
disjunctions read no positions at all.

Fewer segments mean fewer dictionary lookups and stream heads per query, which
is why a build compacts its directory. Segments are bounded by the 3 GiB merge
input cap and the 32-bit run length: a direct merge holds every input blob and
its output in memory, so larger segments would need a streaming merge.

## Reading an index

The query is parsed as TINQL, tokenized using the index's settings, and compiled
into cursors over each segment and the buffer. A segment's cursors read the
terms' ordinal streams and, for positional shapes, their positions streams
(see [Segment format](#segment-format)), and turn ordinals into tuple
locations through the document table. Cursors combine those streams for
Boolean, phrase, proximity, and positional queries. The write buffer keeps
the same streams in memory, rebuilt per term as records arrive.

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
  supported ranked queries the scorer selects the top results (see
  [Ranking](#ranking)).
- **Count** of a Boolean combination of plain terms folds document ordinals.
  The count combines the terms' chunks word by word in fixed scratch buffers,
  visits only chunks some term occupies, clears the segment's dead documents
  a word at a time from the backend's bitmap of them, so a count after a
  large delete and VACUUM costs close to what it costs on a fresh index, and
  counts set bits.
  The visibility map is read once, after the view; if a dead list was published
  in between, the count starts over, because a page VACUUM marked all-visible
  may hold tuples the older view still lists. Matches on pages that are not
  all-visible are mapped back to TIDs through the segment's page table and
  checked a heap page at a time under one buffer lock. The write buffer is
  counted the same way, always against the heap. Per-source counts are
  summed: a location is live in one source only.
- **Ranked disjunctions and conjunctions** walk the same ordinal streams:
  block-max WAND over the terms' chunks with the score bounds each stream
  stores per chunk and per
  1,024-document sub-block, and within an admitted chunk only the members of
  the essential terms are visited, those without which the rest cannot reach
  the threshold; the other terms are tested by bit. A candidate's
  term-frequency bucket is the payload entry at its rank in the term's
  stream, its length a table lookup by ordinal, and its TID is resolved only
  when it enters the top k. A conjunction is led by its rarest term through
  the chunks every term and elided filter holds, testing the shared members
  by bit; a phrase walks its words' conjunction and reads positions only for
  candidates that would rank. Other combinations of terms and phrases under
  AND, OR, AT LEAST and AND NOT (`w OR "p q"`, `(a AND b) OR c`,
  `a AND NOT b`) walk the disjunction of their scoring terms: a document's
  score is the sum over the scoring terms it holds whatever the shape, so the
  disjunction's bounds hold, and the shape is tested a word of documents at a
  time over the terms' bits, a phrase's positions read only for a candidate
  that would rank. Expansions (prefixes, regexes, fuzzy terms, ranges) and
  other span shapes score every candidate of the stream.
  `stannum.count_fold = off` selects the strategies below for
  these queries too.
- **Other counts** use page masks when a Boolean term is dense enough to be
  stored as bitmap chunks; purely sparse or positional plans keep
  the scalar path. The bulk path streams exact offset masks in heap-page order,
  one page-table entry at a time; Boolean AND/OR/NOT combine
  those masks, segment dead lists are subtracted, and a streaming union removes
  cross-segment duplicates. All-visible pages use popcount when the predicate
  is exact and is the query's only restriction. Other pages retain tuple-by-tuple
  visibility checks and, where required, text rechecks. Within bulk plans,
  sparse and positional subexpressions adapt the scalar cursors into page
  masks. The bulk path does not build or sort a vector of
  every candidate CTID.

Unordered searches own their captured index view until the scan ends, including
across cursor FETCH calls. Rescans rebuild cursors against that same view, so
buffer appends and directory changes do not replace the original candidate set.
The cursor is dropped before its owning view, also on error cleanup. Streaming
bounds decoded candidate buffering, not all index memory or I/O. Planner
startup cost includes estimated index I/O; candidate traversal CPU is run
cost.

Reads from a segment are bounded per cursor: a payload cursor holds one span
of skip slots, an ordinal cursor one chunk, a document-table cursor one window
of 4,096 offsets and a length lookup one window of 64 documents, each replaced
by the next. Ordinal chunks, payload spans and offset windows are shared
through a per-backend least-recently-used cache of `stannum.read_cache_mb`
(64 MiB), so the hot
chunks of frequent terms stay resident across queries while a sweep of a long
stream displaces only itself. Only headers, dictionary blocks and page tables
stay in the reader's arena, whose total across a backend's cached readers is
bounded by `stannum.reader_cache_mb` (384 MiB). Ranked queries the scorer
cannot prune score the candidate stream as it arrives and keep only the top
`k`; reading past `k` rows completes the ordering as a pruned scan does.

A ranked walk reads ordinal chunks, position spans, and the length and class
pages of its candidates in place, from shared-buffer pages it holds pinned,
rather than copying them into the per-backend cache: with eight backends
each copying into its own cache, the copies cost more than the scoring. A
walk holds only a few pages per term at a time. Each pass of a walk gives up
its pins as it ends, error and cancellation release them during unwinding,
and a backend that exits mid-walk leaves them to PostgreSQL's resource owner
instead of releasing them a second time.

The word operations are portable Rust; there is no architecture-specific SIMD
dispatch.

`EXPLAIN ANALYZE` shows the chosen path and, for executed custom counts,
`Count Strategy: ordinal fold`, `page bitmaps` or `scalar`. Unordered searches show
`Candidate Strategy: streaming page bitmaps` or `streaming scalar` and
`Candidates Visited`; that counter records consumed candidates, not an unknown
full cardinality when LIMIT stops early. `SET stannum.enable_custom_scan = off`
selects the PostgreSQL bitmap path for comparison.

### Read accounting

`EXPLAIN (ANALYZE, BUFFERS)` on the custom scan reports where reads go:
`Bytes Fetched` and `Disk Pages By Area` per segment area, `Page Touches By
Area` (every page read or found in shared buffers, with the ordinals split
into what is copied, which includes stream heads and bounds, chunks read in
place and bucket nibbles read in place, for comparing with engines that count
page touches), `Disk Pages By Phase` for reads outside the segment reader,
`Walk Setup Blocks` and
`Walk Body Blocks`, `Chunks Loaded`, `Position Lists Read`, `Visibility
Checks`, `Visibility Map Hits` and `Heap Fetches`, and the pins a walk held
(`Pages Pinned`, `Pages Held Peak`). `stannum.debug_seed_score`
(superuser-only) prunes a walk against a supplied threshold, to measure what
a perfect threshold would save; it can change results and is not for
production use.

### Per-backend caches

A query captures the directory and the buffer state under one shared meta
lock, then reads through four caches that live in the backend and key on
what the meta page says, so every backend sees the same thing without any
coordination:

- **Segment readers**, by index identity and segment generation. A reader
  keeps the byte ranges it has fetched (dictionary index, dictionary blocks,
  ordinal chunks, payload, document table) for as long as the generation is
  in the directory, with the segment's dead list as stored and decoded (see
  below). Generations never repeat within an identity, and REINDEX changes
  the identity, so a cached reader can never describe a different segment.
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

A reader decodes its segment's dead list once per published list, into a
bitmap over the segment's ordinals with an 8 KiB block per 65,536-document
chunk that holds a dead document and nothing for the others: at most a bit
per document, however many are dead. The ranked walk and the count clear a
chunk's dead documents word by word, and the scorer tests a row's ordinal.
Every other reader of a dead list (bitmap scans, streaming scans, the count's
fallback, `max_score`) streams it from the stored ordinal stream beside its
own cursor. The decoded form used to be a set of heap locations plus a
vector of ordinals, 16 to 24 bytes per dead document in every backend: after
VACUUM published 45 million dead rows of 150 million, eight query backends
each held about a gigabyte and the server ran out of memory.

Each captured view drops the readers (with their dead lists) and page tables
of generations its index's directory no longer holds, so a long-lived backend
(behind a connection pooler, say) does not keep what merges retired. It then
adds up the readers' fetched bytes, the dead lists stored and decoded, and the
page tables, across every index the backend has read; past
`stannum.reader_cache_mb` (384 MiB) both caches are emptied together. Dead
lists count because they grow with deletes, not with what queries read, but
stored and decoded they take at most about a quarter of a byte per document:
under 40 MB for 150 million documents.

The buffer index is not shared between backends. Sharing it would need a
shared-memory rendezvous (`shared_preload_libraries` or the DSM registry of
PostgreSQL 17+), a serialized form of the index, and lifetime management
across epochs and identities, to save at most one build per connection and
one per VACUUM rewrite per backend, bounded by the byte cap. That build
cost, and the reader cost of small folds, is what the fold defaults trade
against write stalls.

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
segment dead lists: exact dead-document subtraction for terms with at most 1,024
documents, and live-fraction scaling for more common terms and their expansions.
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
evaluate the parameter. Unknown queries use the default cost estimate.

Arbitrary query expressions, correlated parameters, dynamic scoring settings,
and parameterized unordered or count scans are not planned as Stannum custom
scans; PostgreSQL's ordinary paths evaluate them.
A runtime LIMIT remains correct but cannot supply the planner's constant top-k
bound. PostgreSQL still chooses between custom and generic plans normally.
A parameterized `LIMIT` becomes a runtime top-k bound only when no residual
SQL filter remains on the scan.

A ranked scan with a known `LIMIT` of at most 4,096 rows prunes instead of
scoring every candidate. Each term's ordinal stream carries a bound per
65,536-document chunk (the shortest document per term-frequency bucket) and
per 1,024-document sub-block (the largest bucket), and every document has a
one-byte length class. The scan walks the sources in ordinal order, keeps the
k-th best score as a threshold, and skips every chunk and sub-block whose
summed bounds cannot reach it (block-max WAND). Bounds are evaluated at each
chunk's minimum length and over every bucket up to its maximum, and summed in
the scorer's term order, so rounding never puts a bound below a score it
covers; a run whose bound equals the threshold is skipped only when every
location in it sorts after the current k-th row. The result is therefore
identical to scoring every candidate: same rows, same scores, same tie order.

Flat `AND` and `OR` chains of terms prune this way, as do phrases (walked as
the conjunction of their words, with positions read only for candidates that
would enter the top k) and combinations of terms and phrases under `AND`,
`OR`, `AT LEAST` and `AND NOT` (walked as the disjunction of their scoring
terms). A conjunction whose estimated matches warrant it
(`stannum.warmup_min_matches`) first evaluates its best-bounded chunks
(`stannum.warmup_chunks`) to raise its threshold early. Expansions (prefixes,
regexes, fuzzy terms, ranges), span shapes that do not require every word, and
limits above 4,096 rows score every candidate. Every term an expansion brings
gets a scorer of its own; `stannum.max_expansion_terms` (default 65,536)
bounds their number per query, past which ranking fails with SQLSTATE 54000
rather than scoring some of them. `EXPLAIN ANALYZE` reports
`Pruning: ordinal`, `Scored Candidates`, `Positions Checked`,
`Top-K Completions` and `Exhaustive Score Calls`; the last two are cumulative
across rescans.

Residual SQL filters run after ranking, on the rows the scan returns. The
walk checks each row's snapshot visibility as it enters the top k, so the
dead version an update leaves beside its successor never takes a place. When
the parent still reads past the pruned top k (the filter rejects rows, or
top rows were deleted), the scan deepens the same pruned search, to four
times the previous depth and at least 40 rows, up to 4,096, before it scores
every remaining match. Each deepening is a `Top-K Completion`. Rows already
emitted are not repeated, and documents indexed since the top k was built can
rank into the completed ordering, so a completion is not resumed by
position.

A ranked scan keeps its scorer for as long as it lives, under its own
identity, with the score of every row it ranked: a cursor fetched across
later statements, or two scans on one query open at once, each report the
scores they ranked by even as writes move the statistics. A HOT-updated row
is indexed at its chain root; the score functions resolve the visible member's
location to that root. [Testing](../testing.md#the-ranked-scan-under-concurrency)
describes the concurrency fuzzer that checks this against the unpruned path.

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
`stannum.segment_info('index_name')` exposes the segment layout for inspection;
`stannum.promote()` folds the write buffer and `stannum.merge()` merges toward a
target on demand (see [maintenance workers](maintenance-workers.md#sql-functions)).

Page writes survive an aborted transaction, and WAL replay can apply any
prefix of them, so storage follows one rule: **nothing the on-disk meta page
references changes before the meta page records the change.** New runs are
written first and published second, so a failure in between leaks
unreferenced pages rather than referencing unwritten ones. Retired runs are
joined into the pending chain, and drained pending runs are freed, only
after the meta page that no longer lists them is written; the frees still
happen under the meta lock, behind a removal-horizon record. A write buffer
that a fold or VACUUM replaces is written to pages the published buffer does
not cover (the chain's stale tail, then fresh pages), so the old meta page
and its buffer stay readable together until the new meta page lands.

Reclamation therefore publishes first and frees second: VACUUM walks the
chains of the pending runs no snapshot can still read, removes those entries
from the meta page under the exclusive lock (each matched exactly against
what it walked, so an entry an insert coalesced more runs into meanwhile
waits for the next VACUUM), and marks their pages FREE once that meta page is
written. A failure between the two, like one between writing a run and
publishing it, leaves pages that nothing references and that are not FREE.
Inserts free at most `stannum.reclaim_pages` pages of retired runs per call,
and each run records its last page, so joining chains is one page write
rather than a walk of the run under the meta lock.

VACUUM's cleanup reclaims such orphans. It holds the index's **maintenance
lock** (a heavyweight lock on page 0) for the whole pass; an insert's
deferred merge takes the same lock conditionally, and an insert's merge to
make room in a full directory and a build's merges take it unconditionally,
while they write an unpublished run, so the pass never frees a run that is
about to be published. The pass computes every page the captured directory references
(page 0, each entry's run through its page table, the page-table and
dead-list chains, the whole buffer chain, pending runs up to their recorded
lengths), reads the kind of every other page that existed at the capture,
and keeps the ones not marked FREE as candidates. Under a shared meta lock it
walks only what changed since the capture and confirms the candidates the
current directory still does not reference; it frees them after releasing the
lock. No snapshot can reference such a page: a reader's directory holds only
published entries, a retired entry stays referenced through the pending list
until reclaimed, only FREE pages are ever allocated, and a crash ends every
session. The number reclaimed is written to the server log.

The free space map is not WAL-logged either. After a crash, or on a promoted
standby, pages freed since the map was last written are FREE but unlisted;
the orphan pass records every FREE page it meets in the map again, so they
are reused rather than leaked until `REINDEX`. Every allocation checks under
its lock that a page the map offers is still FREE.

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
  each term's ordinal and positions extents inside their areas and not
  overlapping, the ordinal stream well formed with `df` members below the
  document count, the positions stream holding one entry per member whose
  count matches the member's bucket, `max_tf_bucket`, chunk bounds equal to
  what the buckets and document lengths imply, the page table and document
  table agreeing, and document lengths nonzero, summing to the header's
  total, equal to the positions the terms hold for each document and in the
  recorded length class;
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
| `segment generation G, ...` | The blob decoded from those pages is inconsistent inside: header, dictionary, a term (`term "x"`), a document (`document (block,offset)`), the document table or the dead list. Queries touching that term or document fail or return wrong rows. | `REINDEX` |
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
- Hot standbys use the segmented path only when the extension is preloaded on the primary and
  the standby (removal-horizon WAL records, see
  [recovery](recovery-and-parallel.md#standby-selective-reads)); otherwise
  they use the reference path.
- Unordered custom scans are worker-safe but do not split a scan across workers.
- Fresh connections rebuild their own buffer index. Large buffers increase
  first-query latency; connection pooling amortizes that work.
- Folding and a fold's budgeted merges hold the meta lock for their
  duration, so concurrent inserts and readers wait for them. VACUUM holds it exclusively
  only to publish (a few page writes per merge, rewrite, dead list or
  reclamation), plus the scan of whatever inserts folded during its last
  unlocked round and the rewrite of the write buffer without dead records.
- The pending-free list holds 48 entries; runs released together share one.
  A full list first frees runs no snapshot can still read and otherwise
  appends to its newest entry, delaying that entry's reclamation. A crash
  before a new run is published, or between removing a reclaimed pending
  entry and freeing its pages, leaves orphaned pages that
  `stannum.verify_index` lists as `page N` warnings until the next VACUUM
  cleanup reclaims them.
- Ordinary insert merges have a document budget, and so do merges that bring
  the directory back under `max_segments`. Only the merge that keeps the
  directory within its 96-entry on-disk bound is unbudgeted; its cost grows
  geometrically with the number of folds VACUUM has missed (see Merge
  policy).

Run the suites in [testing](../testing.md) when changing these paths. Performance
methodology and current results are in [benchmarks](../benchmarks.md).

See [recovery, relation persistence and parallel execution](recovery-and-parallel.md)
for temporary/unlogged index support and the standby safety boundary.
