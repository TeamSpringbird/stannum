# Segmented storage, format LDP2

LDP2 replaces the LDP1 fingerprint chains described in
[durable-postings.md](durable-postings.md). A logged `tin` index now holds a
write buffer of per-document records and a directory of immutable segments,
each carrying a real term dictionary, TID-native postings, token positions and
document lengths. Scans compile the query with the index's own tokenizer, run
it against every segment through `tinql::runtime::plan`, and emit exact tuple
bitmaps with recheck off wherever the plan is exact, which is every query form
except an expansion past the 1,024-term cap.

## What changes for a query

| Query shape | LDP1 | LDP2 |
| --- | --- | --- |
| Term, AND, OR | exact candidates, heap recheck | exact, no recheck |
| Phrase, `NEAR`, `THEN`, `WITHIN`, relations, positional filters | Boolean superset, heap recheck | exact from stored positions and lengths |
| Wildcard, regex, range, fuzzy | full heap scan | dictionary expansion, exact within the cap |
| `NOT`, `AT LEAST n OF` | full heap scan | exact |
| Insert | one WAL record per distinct term | one WAL record per document |
| Index reloptions such as `case_folding` | ignored by matching | stored in the index and applied to index results |

`count(*)` over an exact bitmap lets PostgreSQL skip heap fetches for
all-visible pages, so counts on a vacuumed table approach index-only cost
without a custom scan.

Ranking reads the index too. `tin.score`, `tin.full_score` and `tin.max_score`
bind to `score_bound_indexed`, which builds per-term scorers from the segment
directory and dictionaries and looks each row up by TID with forward-seeking
cursors. Statistics follow TIN's contract, verified bit for bit against a live
TIN in [tin-observed-shape.md](tin-observed-shape.md): document counts,
total lengths and document frequencies include dead documents until their
segment is rewritten, buffered documents count immediately, and dense-term
elision uses immutable segments only. Indexes without LDP2 storage keep the
heap-reloading scorer.

## Page layout

Every page is a standard PostgreSQL page with an 8-byte special area holding
the magic `LDP2`, a kind byte and a version byte. Payloads occupy the line
pointer region up to `pd_lower`. The codecs are in
`postgres/src/storage/layout.rs` and are tested as plain bytes.

| Kind | Contents |
| --- | --- |
| `META` (block 0) | index identity, serialized tokenizer settings, write-buffer state, segment directory (at most 128 entries), runs pending reclamation (at most 64) |
| `BUFFER` | chained pages of the write buffer: a `next` link and raw stream bytes |
| `RUN` | chained pages of one immutable blob: a segment or a dead list |
| `FREE` | a released page carrying the transaction id that released it |

A **segment** blob is the `segment` crate's `LSG1` layout: prefix-compressed
dictionary with per-term document frequency, maximum term-frequency bucket and
extents; per-term postings in sparse or 256-page-group form; per-term payload
of term-frequency bucket and delta-coded positions; a document table of every
TID; and a length per document. A **dead list** is a postings stream of TIDs
VACUUM has reported dead in that segment.

The write buffer is a byte stream of `forward` records, one per inserted
document, holding its sorted terms with positions and its token count. The
meta page's byte count says how much of the chain is live; bytes beyond it are
stale and get overwritten after a fold.

## Operations

* **Build.** `ambuild` tokenizes each heap tuple into an in-memory segment
  builder and writes a segment every `tin.build_segment_docs` documents
  (default 32,768). The write buffer starts empty.
* **Insert.** One forward record is appended to the buffer under an exclusive
  lock on the meta page. When the buffer would exceed `tin.write_buffer_docs`
  documents (default 16,384) or 4 MiB, it is first folded: decoded, built into
  a segment, written as a new run, and published in the directory. The buffer
  then restarts from its head page, reusing its chain.
* **Merge.** When the directory would exceed `tin.max_segments` (default and
  maximum 128), every segment is rewritten into one, skipping dead documents.
  The old runs move to the pending list.
* **Scan.** Under a shared meta lock the scan copies the directory and
  brings the backend's buffer index up to date, then releases the lock. Each
  segment is planned against a page-granular reader and drained into the
  caller's bitmap, minus its dead list. Both segments and the buffer are
  queried through one `Index` trait (`segment::index`).
* **Segment reads.** Every segment run has a companion page table run listing
  its block numbers, so any byte range of the blob is reached with one page
  read per 8 KiB, no chain walk. A query reads the header, the dictionary's
  block index, the dictionary blocks its terms fall in, those terms' postings
  and payload extents, and the document lengths it scores. Whole-segment reads
  no longer happen; the buffer manager is the cache.
* **Buffer index.** The write buffer is a forward stream, so each backend keeps
  an incremental inverted index over it (`MutableIndex`), keyed by index
  identity and the buffer's epoch. A scan appends only the records written
  since the backend last looked; a fold or VACUUM rewrite bumps the epoch and
  the index starts over. Per-term postings are encoded lazily on first use in
  the same on-disk shape a segment uses, so planning code does not know which
  kind of index it is reading.
* **VACUUM.** `ambulkdelete` asks PostgreSQL's callback about every document
  in every segment and writes a new dead list where anything changed; the
  buffer is rewritten without dead records. `amvacuumcleanup` rewrites any
  segment whose dead list covers at least half its documents, then reclaims
  pending runs whose releasing transaction id is older than every snapshot
  (`GlobalVisCheckRemovableXid`), marking their pages `FREE` and recording
  them in the free space map for reuse.

The GUCs exist so tests can drive folds, merges, rewrites and reclamation at
small scale. Their defaults are the intended operating values.

## Locking and WAL

The meta page serializes every structural change. Writers hold it exclusively
for the whole insert, fold, merge or VACUUM step. Readers hold it shared only
while copying the directory and buffer. Segment runs are immutable, so readers
need no lock on them beyond the per-page content lock. A run released by a
merge or rewrite is not touched until reclamation, and reclamation waits for
the transaction-id horizon, so a reader holding an older directory can never
see a reused page. Lock order is meta page, then buffer or run pages, then the
relation extension lock; no operation holds two run pages at once.

All page changes use the generic WAL API. A new run is written page by page,
last page first so each page carries its successor's block number, before the
directory entry that references it is written. A crash between the two leaks
unreferenced pages until REINDEX; it never publishes a reference to unwritten
pages.

## Correctness boundaries

* Tuple-location reuse is safe because PostgreSQL removes index references
  through the VACUUM callback before a line pointer is reused; by then the
  old segment lists the TID as dead and any new document lives in a newer
  segment or the buffer.
* HOT updates never reach the index and cannot change the indexed expression,
  so a root TID's stored tokens remain exact for its visible tuple.
* Empty documents match nothing, including `*`, matching the reference
  evaluator. `NOT` complements against a segment's non-empty documents.
* Recovery-mode and standby reads, temporary and unlogged indexes, and indexes
  in any older format keep the full-heap reference path. Older formats report
  `REINDEX required` when touched by a scan or insert.
* `==>` evaluated outside an index scan, for example in a sequential scan,
  still tokenizes with the default pipeline. Index results use the index's
  own settings. An index with non-default tokenizer options therefore matches
  differently from a sequential scan, as it did before; a planner-level fix is
  future work.

## Known limits of this slice

* Page reads go through the buffer manager one page at a time with a pin and
  unpin per page; there is no readahead or batching for long postings lists.
* The buffer index lives in one backend; a new connection rebuilds it from the
  buffer stream on its first query. Measured on Wikipedia articles this is
  about 22 ms per megabyte of buffer, so the 4 MiB fold cap bounds the cost
  near 90 ms per fresh backend. Pooled connections pay it once.
* Folds happen in the inserting backend and rewrite the whole buffer; merges
  rewrite every segment. Both are bounded but make the triggering insert slow.
* Ranked queries score every candidate before selecting the top k: about
  100 ns per candidate through the per-source cursors, or 2.2 ms for the
  22,000 articles matching `history` at 100,000 documents. TIN reaches 2.4 ms
  end to end on that query; Lead is at 4.3 ms. Block-level score bounds that
  let the scan skip candidates are the next step in the segment format.
* The pending-free list caps at 64 runs; beyond that, released pages leak
  until REINDEX with a warning.
* PostgreSQL 17 has not been run for this slice.

## First native measurement

One run each of the other agent's `benchmarks/run.py` harness, native ARM64
Homebrew PostgreSQL 18.6 through a pgrx-managed instance, 10,000 synthetic
documents, mixed profile, two readers, 20 seconds, a scheduled 20 updates/s.
Same machine, same harness, same fixture; builds differ only in the extension.
Single runs, warm cache, not a controlled campaign: use them to see structure,
not to quote a speedup. Raw artifacts are in `benchmarks/results/*-native-0*`
(ignored by Git).

| Query, median ms | Original Lead `3fcf441` | LDP1 `38e4671` | LDP2 |
| --- | ---: | ---: | ---: |
| rare count | 39.6 | 0.55 | 0.13 |
| AND count | 71.0 | 1.04 | 0.14 |
| OR count | 71.7 | 71.4 | 0.59 |
| phrase count | 44.2 | 0.17 | 0.12 |
| miss count | 56.1 | 0.11 | 0.10 |
| rare ranked | 53.5 | 14.1 | 14.5 |
| OR ranked | 87.7 | 87.5 | 17.0 |
| Total read queries/s, all twelve shapes | 32 | 112 | 360 |
| Achieved writes/s (20 scheduled), p95 ms | 18.4, 10.5 | 18.4, 10.4 | 18.4, 10.3 |

LDP1 could not prune `OR`; LDP2 answers it from postings. The write buffer is
planned as a cached in-memory segment keyed by its version, so a scan pays a
rebuild only after an insert changed it. Ranked queries sit at about 15 ms in
every build that finds candidates quickly: that is the scoring path reloading
and retokenizing the corpus, unchanged here and the next thing to remove.

### With indexed scoring

Same protocol, one run, after `tin.score` moved to the index:

| Query, median ms | LDP2 heap scoring | LDP2 indexed scoring |
| --- | ---: | ---: |
| rare ranked | 14.5 | 0.16 to 0.45 |
| AND ranked | 15.3 | 0.22 to 1.3 |
| phrase ranked | 14.7 | 0.12 to 0.30 |
| OR ranked (all 10,000 rows scored) | 17.0 | 2 to 14 |
| Total read queries/s, all twelve shapes | 360 | 740 to 1,160 |

Ranges span several single runs taken while a Docker benchmark campaign was
running on the same machine, so sub-10 ms medians moved with its load;
direct timing of the 10,000-row scoring sum gave 1.7 to 4.5 ms across
repetitions. The per-source cursors advance monotonically with the bitmap
heap scan's TID order; in an isolated release-mode probe
(`segment/tests/scoring_cost.rs`) the lookup sequence costs 21 ns per row
against 2.4 µs with fresh cursors per row.

## Custom scan nodes

Two planner hooks add paths when `tin.enable_custom_scan` is on (the default)
and a `==>` restriction on a base relation is answered by a segmented index
whose partial predicate, if any, the planner has proven:

* **Lead Text Search Scan** replaces the relation scan. It compiles the query
  with the index's own tokenizer, plans it against every segment and the
  buffer, and fetches each candidate by TID under the query snapshot,
  evaluating any remaining quals. The `==>` clause itself is not re-evaluated
  unless an expansion exceeded its cap, in which case the node rechecks those
  rows with the original clause. When the query orders by `tin.score`,
  `tin.full_score` or `tin.max_score` bound to the same index, the path claims
  the sort's path keys and emits rows by descending score, so `LIMIT k` stops
  after k fetches. The planner's `limit_tuples` (offset plus limit, when both
  are constants) is carried into the node as `Top K`: every candidate is
  scored, but only the top k are ordered up front, by selection rather than a
  full sort. A parent that reads past k, such as a nested loop that filters
  joined rows, gets the remainder ordered on demand.
* **Lead Count** replaces `SELECT count(*)` when the `==>` clause is the only
  restriction. It counts candidates on pages the visibility map marks
  all-visible without touching the heap and fetches the rest.

Both fall back to a heap scan evaluating the original clause when the
snapshot was taken during recovery, and neither is offered while the server
is in recovery, so standbys keep the bitmap path. Setting the GUC off leaves
the bitmap index scan, which is also exact. `EXPLAIN ANALYZE` reports the
index, the query, the order, candidate and heap-fetch counts, and the number
of all-visible pages skipped.

Server-side execution time on the harness's 10,000-document fixture, median
of seven, alongside the earlier measurements:

| Query, ms | TIN | LDP2 bitmap path | LDP2 custom scan |
| --- | ---: | ---: | ---: |
| miss count | 0.24 | 0.38 | 0.13 |
| rare count | 0.33 | 0.51 | 0.17 |
| AND count | 0.46 | 0.68 | 0.20 |
| OR count, 10,000 matches | 0.49 | 2.25 | 0.32 |
| phrase count | 0.41 | 0.47 | 0.17 |
| rare ranked, top 10 | 0.94 | 0.82 | 0.21 |
| AND ranked | 1.12 | 1.70 | 0.31 |
| phrase ranked | 0.69 | 0.61 | 0.23 |
| OR ranked, 10,000 scored | 1.32 | 14.6 | 1.05 |

The harness's mixed profile went from about 1,160 to 9,700 read queries per
second on this machine. TIN's numbers come from PlanetScale's hardware and
Lead's from a local machine, so treat the comparison as coarse; the point is
that no shape is an order of magnitude apart any more.

## Page-granular reads and the buffer index, at 100,000 documents

Same Wikipedia fixture and mixed protocol as before (two closed-loop readers,
one writer scheduled at 20 updates/s, 30 s), on the local machine. The
"paged" column reads segments page by page but rebuilt the buffer's in-memory
segment per backend after each write; the last column keeps an incremental
buffer index per backend instead.

| | Whole-segment reads | Paged reads | Buffer index | Top-k selection |
| --- | ---: | ---: | ---: | ---: |
| Read queries/s, all twenty shapes | 74 | 182 | 815 | 1,146 |
| Count queries, median ms | 5 to 12 | 3 to 5.5 | 0.26 to 2.5 | 0.22 to 2.2 |
| Ranked queries, median ms | 120 to 350 | 6.7 to 16.6 | 0.5 to 9.4 | 0.4 to 6.0 |
| Ranked queries, p95 ms | | 29 to 57 | 1.1 to 11 | 0.5 to 6.6 |
| Achieved writes/s, p95 ms | | 19.8, 10.4 | 19.8, 11.2 | 19.8, 10.8 |
| Index build, s | 15 | 14.7 | 14.6 | 14.6 |
| Index size | 161 MB | 161 MB | 161 MB | 161 MB |

The last column also stops copying a page on every cache hit and reads
document lengths without the arena, which halved the per-candidate scoring
cost, and reads only the term-frequency bucket from the payload when scoring.
Server-side execution time from `EXPLAIN ANALYZE` on the same index once a
session is warm: count queries 0.15 to 2.1 ms, ranked 0.33 to 5.8 ms. TIN on
PlanetScale for the same 100,000 articles: count 0.25 to 5.3 ms, ranked 0.33
to 6.5 ms. A fresh session pays the buffer index build first (about 18 ms
with 600 articles buffered), then nothing.

## Validation

```sh
cargo pgrx test pg18 --package tin --no-default-features --features pg18
cargo pgrx install --package tin --no-default-features --features pg18 --release
python3 postgres/tests/postings_lifecycle.py
cargo clippy --locked --workspace --all-targets --no-default-features --features 'pg18 pg_test' -- -D warnings
```

Forty extension tests pass, including a fold-and-merge test at small
thresholds that compares every query shape against a sequential-scan reference
and asserts zero rows removed by recheck, and a test that index tokenizer
options govern matching. The lifecycle script passes all eighteen checks,
including a new cycle of deletes, inserts and double VACUUMs that must keep
results exact while the index size stays bounded by page reuse.
