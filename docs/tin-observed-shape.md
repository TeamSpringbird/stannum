# TIN's observed shape, from a live PlanetScale database

Observed 2026-09-17 against PlanetScale Postgres 18.6 with `tin` 1.0.2, through
a non-superuser role. `pageinspect` and raw file reads were unavailable, so
these are behaviors and structure names exposed by TIN's own SQL surface and
`EXPLAIN` output, not bytes. Every table created for this was dropped
afterwards. Nothing here is a claim about TIN's source code.

## SQL surface

Functions in schema `tin` beyond the ones Lead already provides:

| Function | Result columns | What it tells us |
| --- | --- | --- |
| `segment_info(index)` | ordinal, kind (`immutable`/`mutable`), root_block, docs, dead_docs, sum_doc_lengths, npostings, total_pages, source_state, origin (`build`/`promotion`/`merge`), sequence | A segment directory with per-segment document count, dead count and total length, which is exactly the statistics BM25 needs |
| `promote(index, extent_cap_bytes)` | consumed_controls, linked_segments, docs_promoted, terms_added | Folding the mutable segment into an immutable one is a first-class operation |
| `merge(index, target_segment_count, high_water_multiplier, max_fan_in, force)` | considered, retired_only, merged, linked, output_docs, output_postings, replayed_kills, no_op_reason | Merges have a high-water policy, a fan-in cap (default appears to be 4) and drop dead documents ("kills") |
| `fsm_rebuild(index)` | start_nblocks, marked_blocks, swept_blocks, swept_runs | Free pages are tracked in the FSM and grouped into runs |
| `fsck(index, heapcheck)` | segment, structure, blockno, message | Structural verification exists per segment and structure |
| `tin_text_cmpfunc_indexed(lhs, rhs, index oid)` plus `_support` | | The planner binds `==>` to a specific index (see below) |
| `tin_text_term_array_cmpfunc_indexed(lhs, text[], use_or, index oid)` | | An array form of the operator exists |
| `tin_text_restrict`, `tin_text_join` | | `==>` has its own selectivity estimators |

GUCs match the public settings reference. `tin.index_maintenance_mode`
defaults to `background`, and a background worker did promote and merge on its
own during the session.

## Structures named by `EXPLAIN`

With `tin.track_page_reuse_stats = on`, the custom scan reports page touches
by structure. Names observed: `Metadata`, `Term Map`, `Postings Footer`,
`Positions`, `DL Sidecar`, `Liveness Bitmap`. A single-term plan also printed
`Postings: PagePack page stream` and `Positions: Not read`.

That maps onto LDP2's segment almost one to one:

| TIN | LDP2 |
| --- | --- |
| Term Map | dictionary |
| PagePack page stream, Postings Footer | grouped postings; the footer is per-term index data read before the stream |
| Positions, read only when needed | payload stream, separate from postings |
| DL Sidecar | length table |
| Liveness Bitmap, one per segment, updated by VACUUM | dead list per segment |
| Metadata (4 unique pages touched for a 3-segment index) | meta page plus per-segment header |

The name `PagePack` matches the term-frequency bucket table Lead already
carries, so postings and buckets share one encoding there.

## Segments and the write path

* A 20,000-document build produced four immutable segments of about 5,100
  documents each (`initial_segment_count` follows parallelism), each 15 to 16
  pages, so about 5.3 bytes per posting including positions and dictionary.
  With `initial_segment_count = 1`, 10,000 documents became one 20-page
  segment. LDP2 built the same 20,000 documents into 549 KB against TIN's
  1,294 KB.
* An empty mutable segment already occupies 52 pages; 100 documents took 57
  and 864 took 83. The mutable structure is preallocated and page-oriented,
  not a record log.
* Promotion is byte-triggered: with `max_mutable_segment_size = 131072`, the
  mutable segment promoted at 2,001 documents.
* After 20,000 inserts, background maintenance had promoted several times and
  merged the results into one 19,236-document segment (origin `merge`). The
  relation grew to 10.5 MB in the process; `fsm_rebuild` then reported 1,283
  blocks with 129 marked and 1,154 swept, so retired pages are reclaimed
  lazily through the FSM. LDP2 reached 1.3 MB after the same inserts.
* `merge` with defaults answered `below_high_water`. Forced to one segment it
  merged four of five (fan-in), emitted only live documents and left a single
  build segment with its dead count intact.
* Root blocks of parallel-built segments interleave (151, 149, 147, 145), so
  a segment is not a contiguous block range.
* A single 20,000-document segment with 16,000 dead after VACUUM was not
  rewritten by background maintenance within 20 seconds, but a default
  `merge` call then rewrote it alone (considered 1, merged 1, 4,000 output
  documents), so the dead-percent threshold applies to single segments and
  the trigger is a maintenance pass, not VACUUM itself. A five-document
  segment with two dead was left alone even when forced, so there is also a
  minimum size.

## Scoring statistics: the contract, verified to the bit

Same five documents on TIN and on local Lead, `tin.full_score` for
`rare OR common`:

| State | TIN | Lead (visible corpus) |
| --- | --- | --- |
| All five live | 1.1631508, 0.39556286, 0.36165747, 1.1436889 | identical |
| Two deleted, before VACUUM | 1.1631508, 1.1436889 | 0.9983526, 0.9018668 |
| After VACUUM (dead_docs = 2) | unchanged | |
| After REINDEX | 0.9983526, 0.9018668 | identical to Lead |
| Two more inserted into the mutable segment | 1.1196322, 1.0063113, 0.78576607, 0.6938147 | identical when Lead has the same five live |

Dense-term elision agreed as well (`2.1812243` and elided terms match). So:

1. Lead's BM25 arithmetic, bucket quantization and elision policy are
   bit-identical to TIN's.
2. TIN's statistics are the sum over segments of dead-inclusive counts
   (`docs`, `sum_doc_lengths`, per-term df) plus the mutable segment's live
   documents. Dead documents leave the statistics only when their segment is
   rewritten by a merge or REINDEX.
3. `tin.score` and `tin.full_score` cannot appear together on one relation;
   TIN raises an error. Lead allows it today.

For LDP2 this means the persisted-statistics milestone can use exactly what
the directory already stores: `docs` and `total_length` per segment entry,
`df` from each dictionary, and the buffer's live documents. A segment rewrite
at the dead threshold (TIN's `dead_percent_threshold` default 0.5, LDP2's
"at least half dead" rule) changes statistics at the same moment TIN's do.

## Scoring policy details, verified by the oracle

`benchmarks/oracle.py` compares Lead against the live TIN on identical
fixtures, forty-one query shapes, five states. Getting it to agree on every
one of the 205 query/state pairs, match sets and score bits alike, required
four changes to Lead, each observed first on TIN:

* **Token-less documents are not documents.** A body that tokenizes to
  nothing (empty, or `...`) is absent from `segment_info` counts and from N
  and the average length. Lead used to count them.
* **Expansions score their dictionary terms.** `ra*`, `MATCHES r.*e`,
  `x TO z` and `rare~1` each contribute every matching term at the node's
  boost (`ra*^2` gives each term weight 2); a fuzzy term's own literal is one
  of them. Lead scored none of the regex and range forms and only the fuzzy
  literal.
* **Boolean NOT drops its subtree from scoring**: `a AND NOT (b OR c)` scores
  `a` alone and `* AND NOT c` scores nothing. Negative span relations keep
  both sides: `a NOT OVERLAPPING b` scores `a` and `b`. Lead scored negated
  terms.
* **`tin.max_score` is the maximum actual score over the visible matching
  rows**, not an upper bound and not over candidates the index still holds
  for deleted rows. Alone it uses the full policy; beside `tin.score` in the
  same target list it adapts to the dense policy. Lead's old maximum ranged
  over every document holding any scoring term under the dense policy.

TIN also rejects `tin.score` and `tin.full_score` together on one scanned
relation. Lead still allows that.

## Tokenizer binding and fallback paths

* With the custom scan disabled, TIN still offers a `Bitmap Index Scan` on
  the `==>` operator, and `EXPLAIN ANALYZE` shows no rows removed by recheck
  for phrases, `AND NOT`, or wildcards: the bitmap path is exact, as LDP2's is.
* On an index built with `case_folding = preserve`, a query that cannot use
  the custom scan fails with
  `queries targeting a tin index with non-default tokenization require a usable tin custom scan path`.
  TIN never silently tokenizes with defaults; it binds the operator to the
  index at plan time (`tin_text_cmpfunc_indexed` carrying the index OID) and
  refuses to run it elsewhere. On a table with no `tin` index the plain
  operator runs as an ordinary filter.
* A query on a `==>` operator with no matching index produces a normal
  sequential scan with the plain operator.

Lead's equivalent is to mirror the function pair: a planner support on
`tin_text_cmpfunc` rewrites to `tin_text_cmpfunc_indexed(lhs, rhs, oid)` when
a `tin` index exists on the expression, and that function's own support
answers `SupportRequestIndexCondition` with `lhs ==> rhs` marked exact so the
bitmap path still applies. Executed outside an index path, Lead can tokenize
with the index's stored settings, which is stricter than today and more
useful than TIN's error.

## Server-side timings, TIN versus LDP2

The harness's `tin` engine ran the mixed profile against PlanetScale, but
every query came back at about 31 ms median: that is client-to-AWS round
trip, not TIN. Server-side execution time from `EXPLAIN ANALYZE`, median of
seven, on the harness's 10,000-document fixture, is the comparable figure.
Different hardware (PlanetScale's instance versus a local machine that was
also running a Docker benchmark campaign), so treat ratios as coarse.

| Query | TIN, ms | Lead LDP2, ms |
| --- | ---: | ---: |
| miss count | 0.24 | 0.38 |
| rare count | 0.33 | 0.51 |
| AND count | 0.46 | 0.68 |
| OR count (10,000 matches) | 0.49 | 2.25 |
| phrase count | 0.41 | 0.47 |
| rare ranked, top 10 | 0.94 | 0.82 |
| AND ranked | 1.12 | 1.70 |
| phrase ranked | 0.69 | 0.61 |
| OR ranked (10,000 scored) | 1.32 | 14.6 |

At 100,000 Wikipedia articles (median of five, warm session, page-granular
reads with the incremental buffer index):

| Query | TIN, ms | Lead, ms |
| --- | ---: | ---: |
| miss count | 0.25 | 0.2 |
| rare count (8 matches) | 0.38 | 0.2 |
| common count (22,063 matches) | 3.3 | 0.9 |
| AND count | 2.5 | 0.8 |
| phrase "united states" count (15,381) | 5.3 | 2.5 |
| rare ranked, top 10 | 0.91 | 1.8 |
| common ranked | 2.4 | 8.2 |
| phrase "united states" ranked | 6.5 | 9.4 |
| OR ranked | 1.4 | 2.6 |

Counts are now at or below TIN; ranked queries over broad terms are 2 to 3x
slower because Lead scores every candidate before the top-k heap. Block-max
pruning inside the custom scan is the remaining gap.

Selective shapes are within about 1.5x either way. The broad `OR` is where
TIN's count strategies and in-scan top-k bound pay off: Lead still drains
every posting into a bitmap and scores every matching row through the
executor. That gap is the custom-scan milestone.

## Plan shapes, for the custom-scan milestone

Custom scan nodes observed: `Tin Count` with a `Count Strategy` of `Fold` or
`Page Drain`; `Text Search Scan` under a `TID Materializer` for row
retrieval; `Projector` above `Text Search Scan` with `Top K: n` and
`Scoring: dense-term elision` for `ORDER BY tin.score(ctid) DESC LIMIT n`.
Query text is printed in a lowered form (`AND(REGEX(need.*), NOT(w3))`,
`SPAN(MAXGAPS(3, UNORDERED(needle, beta)))`, `SPAN(rare) IN FIRST 3 WORDS`)
that matches Lead's lowered `Query` shapes.

## What this changes in the plan

* Adopt the dead-inclusive statistics contract for persisted scoring; it is
  both TIN's behavior and the cheap one.
* Add `tin.segment_info` to Lead over the LDP2 directory; it is the natural
  observability surface and the fixtures for it are easy.
* Implement the `tin_text_cmpfunc_indexed` pair for tokenizer binding instead
  of the planner-hook option in the design doc.
* Keep LDP2's record-log write buffer: TIN's preallocated mutable structure
  is a size and complexity cost that a dev/test substitute does not need.
