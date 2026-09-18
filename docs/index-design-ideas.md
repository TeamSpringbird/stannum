# Index design ideas for fast full-text search in Lead

Written 2026-09-17 against `3fcf441` plus the uncommitted work on the
`t3code/investigate-lead-search-performance` worktree (LDP1 postings, in-memory
retrieval core, benchmark harness, and the design docs under `docs/`). This is a
design proposal, not a measurement. Where a claim rests on arithmetic rather than a
run, the arithmetic is shown.

## 1. What the index has to answer

Everything below is dictated by the query language and scoring policy already in
the repository. An index that cannot answer one of these exactly must either
recheck the heap for that query shape or fall back.

| Need | Source of the requirement | Index data required |
| --- | --- | --- |
| Term, AND, OR, NOT, `AT LEAST n OF` | `tinql/src/runtime/mod.rs` `Query` | term → document set |
| Phrase with gaps, alternatives, slop; `THEN/N`, `NEAR/N`, `WITHIN` | `boldi_vigna::SpanQuery` | term → (document, positions) |
| `ENCLOSES`, `OVERLAPPING`, `BEFORE`, `AFTER` and negations | `SpanQuery::Containing` etc. | positions |
| `IN FIRST/LAST/MIDDLE n%`, `IN WORDS a TO b` | `SpanPositionFilter::resolve_window(doc_len)` | document length in positions |
| `brew*`, `MATCHES`, `a TO cat`, `beer~2` | `Query::Regex/Range/Fuzzy` | ordered term dictionary |
| BM25 with 4-bit tf buckets, `dense_ratio`, boosts, stop words | `postgres/src/bm25.rs`, `tf_bucket.rs` | df, N, avg length, per-posting tf bucket, doc length |
| Highlighting | `highlight_udfs.rs` retokenizes the text | nothing extra |

Two properties of the current code shape the design more than anything else:

* Postings can be keyed by heap TID directly. The evaluator never needs a dense
  document id, and PostgreSQL's bitmap heap scan consumes TIDs natively. This is
  the property TIN's public description leans on and it is what lets deletes
  and merges avoid renumbering.
* The tokenizer is already a pure, deterministic pipeline with a compact spec
  (`TokenizerPipelineSpec`). The index can store the spec it was built with and
  refuse queries analyzed with a different one.

## 2. Why LDP1 is the right spike and the wrong foundation

LDP1 (`postgres/src/postings.rs` on the sibling worktree) proved the lifecycle
plumbing: generic WAL, buffer guards, VACUUM callback, memory-context-owned scan
state, standby fallback, cancellation. Keep all of that. The data layout itself
does not scale, for reasons that are structural rather than tuning:

1. **Lookup cost is independent of df.** A term lookup walks its whole bucket
   chain. With 128 buckets, every lookup reads 1/128 of all postings. On the
   prepared 1,000,000-document Wikipedia corpus (about 2.8 GB of normalized text),
   assume roughly 250 distinct terms per document: 250M postings × 16 bytes is
   4 GB of index, about 3,900 pages (31 MB) read per term whether the term
   matches one document or a million. A miss costs the same as a hit.
2. **One WAL record per (document, term).** `insert` calls `append` once per
   distinct term, each with its own `GenericXLogStart/Finish` and an exclusive
   lock on the bucket head. A 250-term document is 250 WAL records and 250
   head-lock acquisitions. This is where the sustained-write benchmark will fail.
3. **No dictionary, no positions, no statistics.** Fingerprints cannot support
   prefix, range, regex or fuzzy expansion; phrases must recheck every candidate;
   scoring still rebuilds the corpus through SPI.
4. **No reclamation.** VACUUM compacts in place but never unlinks or reuses pages.

Because fixing any one of these changes the format, the next format should be the
one intended to last. The rest of this document describes it.

## 3. Proposed architecture: a small LSM inside the index relation

The shape is the one TIN describes publicly and the one GIN's pending list already
uses inside PostgreSQL: a mutable write buffer that is cheap to append to, folded
into immutable sorted segments that are cheap to read, all inside ordinary
PostgreSQL pages of the index relation.

```
block 0   meta page: format, tokenizer spec, segment directory, global stats
buffer    document-ordered append log (one WAL record per document)
segment*  immutable: dictionary pages → posting pages → payload pages → doc table
```

### 3.1 Meta page

* Format magic and version.
* The serialized `TokenizerPipelineSpec` used at build time. Scans compile the
  query with this spec, not `default_pipeline()`. See section 7 for the operator
  mismatch this exposes.
* Segment directory: for each segment, its first block, page count, document
  count, total document length in positions, dead-document count, and a
  generation number. Bounded (say 64 entries); overflow to a directory page.
* Global counters that are derivable from the directory but cached: N, sum of
  lengths.

Readers copy the directory once at `amrescan` and never touch the meta page
again. Writers publish a new directory with one generic WAL record. Immutable
segments therefore need no locks after publication; only the buffer and the meta
page are ever locked.

### 3.2 Mutable buffer: one record per document

The buffer is a chain of pages holding **forward records**, not postings:

```
record := tid, doc_len u32, term_count u16,
          [term bytes, tf_bucket u4, position deltas varint...]  sorted by term
```

* `aminsert` tokenizes once, sorts terms, and appends one record with one
  generic WAL record. This replaces 250 WAL records with one and removes the
  per-term lock storm. Lock order stays trivial: buffer tail, then extension.
* Queries scan the buffer sequentially and evaluate each record with the
  existing `evaluate()` against a `TokenizedDoc` reconstructed from the record.
  The buffer is bounded (a `max_mutable_segment_size` reloption, default 4 MiB
  to match TIN's documented knob, and a document cap around 16,384), so this is
  a small constant per query. It is also exact: positions and lengths are there.
* Fold: when the buffer exceeds its bound, the inserting backend (or a later
  maintenance pass) sorts the buffer into a new immutable segment and publishes
  it. This is the same amortization GIN's `fastupdate` relies on, and it has the
  same known tail: the folding insert is slow. Mitigations in order of cost:
  fold in `amvacuumcleanup` and in `ambulkdelete` first, then a
  `tin.maintain(index)` SQL function, then a background worker only if measured
  tails demand it. Do not start with a background worker.

### 3.3 Immutable segment

Built from a sorted (term, tid) stream, written as fresh full-image pages, then
published. Nothing in a segment is ever modified after publication except its
dead bitmap (section 3.6).

**Dictionary.** Terms sorted bytewise, prefix-compressed in blocks of about 64
terms, with a block index of first terms for binary search. Each entry holds df,
the term's posting extent (block, offset, byte length), the payload extent, and
a per-term maximum tf bucket. Sorted real terms, not fingerprints, are what make
`brew*` a range scan, `a TO cat` a range scan, and `MATCHES`/`~2` a bounded
dictionary walk (section 5).

**Postings, TID-native, in 256-page groups.** A term's postings are a sequence of
groups. A group covers 256 consecutive heap blocks:

```
group := first_block u32, page_bitmap [32 bytes], cumulative_count u32,
         per present page: encoding tag + offsets
```

Per-page offsets use one of two encodings chosen by density: a sorted `u16` list
when few tuples on the page match, or a 291-bit tuple bitmap (MaxHeapTuplesPerPage
for 8 KiB pages is 291) when many do. The tag costs one byte. This is the
"page and tuple bitmaps" idea from TIN's description, stated concretely enough to
implement. What it buys:

* AND of two terms is first an AND of 32-byte page bitmaps; only pages set in
  both are decoded. A conjunction of a rare and a common term decodes almost
  nothing from the common term.
* Count is popcount over tuple bitmaps or list lengths, with the cumulative
  count allowing a whole group to be skipped or summed without decoding.
* Output is already in (block, offset) order, which `tbm_add_tuples` wants and
  which makes bitmap heap scans sequential.
* Rare terms (df below a few hundred) skip groups entirely and store a plain
  delta-coded TID list; the group format is only used above a density threshold
  measured on the Wikipedia corpus, not guessed.

**Payload stream, parallel to postings.** Positions and tf buckets live in a
separate byte stream per term, in the same tuple order as the postings, with a
per-group byte offset so a phrase query can seek to the group it needs. Boolean
and count queries never read this stream. Positions are delta-coded varints;
the tf bucket is the low 4 bits of the first byte. This separation is the single
biggest reason to expect Boolean queries to be fast: their working set is
postings only.

**Document table.** One entry per document in the segment, ordered by TID:
document length in positions (u32). Stored as the same group/page-bitmap
structure keyed by TID so lookup by TID is a bitmap rank. This feeds
`IN LAST n%`, BM25 length normalization, and `sum(len)` for average length.

### 3.4 Segment statistics and the scoring contract

Each segment records N, sum of lengths, and per-term df (in the dictionary).
Global df is the sum across segments plus a buffer scan. This removes
`build_corpus` and `load_documents` from the query path entirely.

There is a policy choice here that must be made explicitly. TIN documents that
its statistics include dead documents until reclamation. Lead today computes
statistics from the visible corpus through SQL. Recommendation: adopt the
segment-level (dead-inclusive) semantics, because Lead's stated purpose is to be
a substitute for TIN and because visible-corpus statistics require exactly the
per-query corpus reconstruction this design removes. Record the change in the
README compatibility boundary and add a scoring fixture that runs a delete
followed by a score before and after VACUUM.

### 3.5 Index build

`ambuild` should not go through `aminsert`. Tokenize in the heap scan callback
and feed (term, tid, tf bucket, positions) into a tuplesort bounded by
`maintenance_work_mem`, then write segments directly from the sorted stream.
The already-accepted `initial_segment_count` reloption partitions the heap block
range into that many segments; each partition is an independent sort and can
later be a parallel worker. Build never touches the buffer.

### 3.6 Deletes, VACUUM, TID reuse, HOT

* `ambulkdelete` walks each segment's document table once, asks the callback
  per TID, and sets bits in a per-segment **dead bitmap** (same TID-keyed group
  structure, stored in pages allocated at segment creation so VACUUM never
  extends the relation). One generic WAL record per modified page. Dead count
  goes into the directory.
* Queries AND-NOT the dead bitmap before emitting TIDs. Counts subtract dead.
* The buffer is scanned by VACUUM too: dead records are removed by compaction
  (it is small).
* **TID reuse is safe by construction.** PostgreSQL only reuses a heap line
  pointer after VACUUM has called the index callback for it, so by the time a
  new document occupies that TID its postings live in a newer segment and the
  old segment has the TID marked dead. The old postings never resurrect.
* **HOT updates** never reach `aminsert`, and HOT guarantees the indexed
  expression is unchanged, so the root TID's postings and positions remain
  exact for the visible tuple. Non-HOT updates arrive as a new TID.
* **Merge.** When `dead / N` in a segment exceeds `dead_percent_threshold`
  (another TIN knob, default 0.5), or the segment count exceeds
  `target_segment_count`, rewrite the affected segments into one, dropping
  dead documents. TIDs never change, so merge is a k-way merge of dictionaries
  and a concatenation of group streams. Old segment pages are released to the
  FSM only once no scan can still hold the old directory: stamp the directory
  generation with the publishing transaction's xid and recycle when that xid
  precedes the oldest running xid, the same rule B-tree page deletion uses.

### 3.7 Concurrency summary

| Actor | Locks | Notes |
| --- | --- | --- |
| Reader | meta page shared for the directory copy, then none | Immutable segments, dead bitmaps read without locks (torn bit reads only add candidates and the heap recheck catches them; for `recheck=false` output, read the dead bitmap under a shared page lock) |
| Inserter | buffer tail exclusive, extension lock on new page | One WAL record per document |
| Folder / merger | builds new pages unlocked, then meta page exclusive for publication | Old pages recycled after the xid horizon |
| VACUUM | per dead-bitmap page exclusive, meta page exclusive for counts | Never blocks readers for long |

## 4. Query execution against this layout

### 4.1 Plans with an exactness flag

Extend the sibling worktree's `CandidatePlan` so every node carries
`exact: bool`. A node is exact when the index alone decides membership:

* `Term`: exact (positions not needed).
* `And`, `Or`, `AtLeast`: exact if all children are exact.
* `Not`: exact **only** if its child is exact; the complement is taken against
  the segment's live document table, never against a candidate superset. This
  is the rule the retrieval core already states in its module comment.
* `Span`, `SpanExpr`: exact when every slot resolves to stored positions and
  document length is available, which is always true in this layout. Evaluate
  with the existing `SpanSolver` over a `TermPositions` implementation that reads
  the payload stream instead of a `TokenizedDoc`.
* `Regex`, `Range`, `Fuzzy`: exact after dictionary expansion (section 5), with
  a cap; above the cap the node is a superset and forces recheck.

If the root is exact, call `tbm_add_tuples(..., recheck = false)`. That single
flag has two large effects in PostgreSQL itself: the executor stops invoking
`tin_text_cmpfunc` per row (today's dominant per-row cost), and bitmap heap
scans can skip fetching heap pages that the visibility map marks all-visible
when no columns are needed, which makes `SELECT count(*)` nearly heap-free on a
vacuumed table without any custom scan node.

### 4.2 Segment-at-a-time, rarest-first

Evaluate the plan per segment (buffer included), emitting TIDs in order, and
union into the caller's `TIDBitmap`. Within a conjunction, order children by df
from the dictionary so the rarest term drives group iteration and the common
terms are probed by page bitmap. This is the "adaptive conjunction" TIN
exposes as `debug_force_conjunction_mode`; start with the simple rule and add
the pushdown variant only if profiles show group decoding dominating.

### 4.3 Cost estimation

`amcostestimate` currently returns a fixed selectivity of 0.1. `Query` already
has `estimate_tuples(total, lookup)` with a conjunction/disjunction/at-least
model waiting for a df lookup closure. Wire it to the dictionary: parse the scan
key, sum df across segments, and report real selectivity and page counts. This
matters as much as speed: with a good estimate the planner will pick the index
path unprompted, and the benchmark harness stops needing `enable_seqscan=off`.

### 4.4 Scoring without corpus reconstruction

`score_support` already locates the matching tin index for the scored
expression. Change the bound function it rewrites to from
`score_bound(document_text, ...)` to `score_bound(ctid, index_oid, ...)`, and
have the index scan that produced the candidates populate the existing
backend-local `SCORE_CACHE` with TID → score as a side effect of evaluating the
query (tf bucket and doc length are in hand while decoding). The cache key
already includes transaction, command, index and query, so its lifetime rules
carry over. `tin.max_score` becomes a max over that map. This keeps the SQL
surface byte-identical while dropping the O(corpus) rebuild.

True top-k pruning (block-max WAND) becomes possible once the per-term, per-group
maximum tf bucket is in the group header: an upper bound on any document's
score in a group is computable from idf, the max bucket and the minimum length
in the group. That is an optimization for a later stage; it requires either an
ordered index scan or a custom scan to exploit `LIMIT`, because
`ORDER BY tin.score(ctid)` is opaque to the planner. Section 8 covers that.

## 5. Expansion queries against a sorted dictionary

* `brew*` and `email.*`: extract the literal prefix from the regex HIR
  (`regex-syntax` is already a dependency), binary search to the prefix, scan
  forward while the prefix holds.
* `a TO cat`: two binary searches.
* `MATCHES hop.*s`: same prefix pruning when the pattern has a literal prefix;
  otherwise a full dictionary walk of that segment. Dictionaries are small
  relative to postings (a few million terms for all of English Wikipedia), so a
  walk is acceptable at development scale and cheap to bound.
* `beer~2`: the fixed prefix (`~P:N`) gives a range; run the existing
  `FuzzyMatcher` over that range.
* Cap expanded terms per node (for example 1,024). Over the cap, either union
  the first 1,024 and mark the node inexact so the heap recheck restores
  correctness, or fall back to `All`. Never drop terms silently.

Expanded terms feed the same posting union as `OR`, so nothing else changes.

## 6. Bulk and write-path details worth deciding early

* **Varint and bitmap codecs are pure Rust with no PostgreSQL dependency.** Put
  them in a new crate (or in `tinql::retrieval`) with property tests that
  round-trip random posting sets and check AND/OR/NOT/count against `BTreeSet`.
  The sibling worktree's `retrieval.rs` tests are the template.
* **Version every page kind** with a one-byte kind and a format number in the
  page's special space, so a mixed-version relation is detectable.
* **Use `pd_special`** for the kind/version, `pd_lower/pd_upper` for content, so
  `pageinspect` and checksums behave normally.
* **Generic WAL is fine for now.** Every page write is either a full image of a
  new page or a small delta on the buffer, dead bitmap, or meta page. A custom
  resource manager (PostgreSQL 15+) would give proper standby conflict handling
  and smaller records; defer it until the standby fallback is measured to
  matter.
* **Reject cross-version surprises loudly.** The existing "REINDEX required"
  error path is the right behavior; keep it.

## 7. The tokenizer contract has to be fixed before pruning is trusted

Today `tin_text_cmpfunc` (the `==>` operator) tokenizes with
`default_pipeline()`, LDP1 inserts with `default_pipeline()`, and only scoring
reads the index reloptions. An index built with `case_folding = preserve` would
prune with folded terms and recheck with folded terms, silently matching the
wrong set relative to TIN.

Options, in increasing ambition:

1. Store the spec in the meta page; the AM tokenizes documents and queries with
   it; results are exact so the operator is not consulted in index plans. In
   seq-scan plans the operator still uses defaults. Document this and rely on
   accurate costing to keep index plans chosen. Cheapest, and no worse than
   today.
2. Add a `tin.matches(document, query, index regclass)` function for explicit
   non-index use with the index's options.
3. A planner hook (or custom scan) that binds `==>` to the index's options in
   every plan shape. This is what TIN's `enable_custom_scan` implies. Needed
   eventually for top-k; not needed to start.

Recommendation: 1 and 2 now, 3 with the custom scan work.

## 8. Where a custom scan eventually becomes necessary

The index AM interface can deliver exact bitmaps and good costing, which covers
filtering and counts well. Two things it cannot do:

* **Top-k by `tin.score(ctid)`.** The planner cannot see through a function on
  `ctid`. Either add an ordering operator (`body <=> 'query'`, `amcanorderbyop`,
  as pgvector does) and accept a different SQL surface from TIN, or add a
  `CustomScan` provider that recognizes `ORDER BY tin.score(ctid) DESC LIMIT k`
  over a `==>` qual and runs the block-max traversal. Compatibility argues for
  the custom scan.
* **Exact counts fully inside the index** when pages are not all-visible. The
  bitmap heap scan handles this correctly already; a custom scan only makes it
  faster.

Both are stage-4 work in the existing plan and nothing in sections 3 through 6
needs to change to support them; the group headers (max tf bucket, cumulative
counts) are the hooks they need.

## 9. Suggested sequencing on top of the existing stage plan

This slots into the stage table in `search-engine-plan.md`; it refines stages
1 through 3 rather than replacing them.

1. **Codec crate with property tests.** Dictionary blocks, group/page bitmaps,
   payload varints, forward records. No PostgreSQL. One afternoon of code, the
   whole design's correctness surface.
2. **Format LDP2: meta page, buffer, single immutable segment from `ambuild`.**
   Inserts go to the buffer; no fold yet (error when full, like an unimplemented
   path should). Terms and Boolean queries exact; `recheck=false`. Re-run the
   lifecycle suite and the 10k campaign. Expected: miss and rare queries stop
   scaling with heap size; write path is one WAL record per row.
3. **Fold and merge, dead bitmaps in VACUUM, page recycling.** Sustained-write
   run at 100k Wikipedia documents.
4. **Positions and document lengths; phrases and spans exact.** Phrase queries
   stop rechecking.
5. **Statistics and scoring from the index.** Delete `load_documents`. Decide
   and document the dead-inclusive statistics contract.
6. **Dictionary expansion, real cost estimation.**
7. **Custom scan for top-k and count pushdown; custom rmgr if standby matters.**

Each step is a format bump until step 5; after that the format should be
stable enough to promise REINDEX-free upgrades.

## 10. Status: the codec crate exists

Step 1 of the sequencing is implemented as the `segment` workspace crate
(`segment/src/`). It has no PostgreSQL dependency and covers the dictionary,
grouped and sparse TID postings with seeking cursors and ordinals, the payload
stream with an ordinal skip table, forward records, and composable set
operations. Every decoder is fallible on malformed input, and property tests
check each component against `BTreeSet`/`BTreeMap` oracles plus a
never-panics test over arbitrary bytes:

```sh
cargo test -p segment
PROPTEST_CASES=4000 cargo test -p segment --release
cargo clippy -p segment --all-targets -- -D warnings
```

Constants that section 11 says to measure (`LIST_MAX`, `BLOCK_TERMS`,
`SKIP_INTERVAL`, the sparse-versus-grouped choice) are single definitions in
the crate and are not yet tuned on real data.

## 11. Things to measure before committing to constants

* Distinct terms per document and df distribution on the prepared Wikipedia
  corpus: decides the list-versus-bitmap threshold and dictionary block size.
* Tuples per heap page in the benchmark tables: Wikipedia bodies are TOASTed,
  so pages hold few tuples and the `u16` list encoding will dominate; the
  synthetic 10k fixture is the opposite. Both must be fast.
* Fold cost at 4 MiB and 16k documents, and its effect on insert p99.
* Buffer scan cost per query at the buffer's maximum size, to confirm it is
  negligible relative to one heap page fetch.
* How often the planner chooses the index path with the new cost estimate and
  default settings.
