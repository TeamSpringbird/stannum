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

Ranking is unchanged in this slice: `tin.score` still rebuilds its statistics
from the heap per query. Persisting statistics is the next milestone.

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
* **Scan.** Under a shared meta lock the scan copies the directory and the
  live buffer bytes, then releases the lock. Each segment is planned and
  drained into the caller's bitmap, minus its dead list. The buffer is built
  into an in-memory segment cached per backend by its version, so it is
  planned exactly like the others and rebuilt only after it changes.
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

* A scan reads each segment's bytes into a backend-local cache (64 MiB cap)
  on first use. Segments are immutable and keyed by index identity, first
  block and generation, so the cache never serves stale data, but the first
  query after a fold pays the read.
* Folds happen in the inserting backend and rewrite the whole buffer; merges
  rewrite every segment. Both are bounded but make the triggering insert slow.
* Ranking statistics are not yet read from segments.
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
