# TIN search documentation: contracts and follow-up tests

Researched 2026-09-20 from the twelve PlanetScale pages supplied by the user.
These are documented behavior, not measurements or verification of Stannum.
Some operational findings already appear in [TIN probes and interpretation](tin-probe-interpretation.md).
The test suggestions below are proposals, not claims that coverage is missing.
Pin the Lead revision before promoting a documented behavior to an oracle expectation;
record disagreements among documentation, Lead, and Stannum separately.

## Search overview

TIN promises snapshot-consistent search, exact counts during writes/VACUUM and on
replicas, cross-column relevance, and cost-based parallel queries. Default analysis
uses Unicode boundaries, case/accent folding, and searchable emoji; stemming is
absent. The overview says a row's score is execution-plan independent.
[Search overview](https://planetscale.com/docs/postgres/search)

Proposed invariant: compare membership, counts, and scores across available
execution strategies using the same snapshot and index state. Keep physical
maintenance behavior outside the Lead semantic oracle.

## Getting started and the oracle's scope

Lead uses the `tin` extension name, supports PostgreSQL 17/18, and scans rows;
it is a small-fixture compatibility tool, not a TIN performance stand-in.
Installation supports UTF8 and SQL_ASCII, rejecting other database encodings.
Scoring without a corresponding scan errors, including unsupported DML
`RETURNING` contexts. Zero-token input matches nothing, whereas explicit empty
phrase/alternative syntax errors. Hyphenated fuzzy input can fail after analysis
produces multiple tokens. [Getting started](https://planetscale.com/docs/postgres/search/get-started)

Proposed checks: distinguish successful empty results from parse errors; exercise
encoding rejection in a disposable database; retain the approximately fifteen-minute
routine oracle budget rather than running Lead on benchmark-scale inputs.

## TINQL boundaries

Juxtaposition means AND, contradicting the older video transcript's OR description.
Keywords are uppercase; standalone NOT is invalid. Fuzzy defaults to one fixed
prefix character. Regex matches whole dictionary terms without folding. Ranges
are inclusive. Phrase gaps, alternatives, tolerance, percentage thresholds, and
composed span operators are supported. Proximity distance counts intervening
words: zero means adjacency. Positions start at zero; explicit word ranges
include both ends. ENCLOSES returns outer spans, ENCLOSED BY inner spans.
Precedence, loose to tight: OR, AND, AND NOT, position filters, relations,
proximity, WITHIN, boost, primary. Equal levels associate leftward.
[TINQL](https://planetscale.com/docs/postgres/search/tinql)

Proposed cases: implicit versus explicit AND; lowercase keywords; adjacency and
one-word gaps; first/last zero and one; inclusive endpoint; reversed proximity;
outer versus inner highlighting; percentage rounding on three alternatives;
fuzzy zero-prefix; normalized regex; precedence versus parenthesized equivalents.
Percentage rounding expectations need oracle confirmation, not inference.

## Scoring

Default `dense_ratio=0.10` drops terms with `df >= ratio*N`; ratios above one
disable this. Explicit boosts, even `^1.0`, pin terms. All-elided matches score
exactly zero without changing membership. Added/replacement terms are analyzed;
the two options cannot coexist. Calls must share identical density/term arguments,
but k1/b may differ. Runtime arguments must be statement-constant. `full_score`
bypasses elision and stop words and cannot share a scanned relation with `score`.
Visible rows alone compete for top-k, but retained dead index entries can still
influence corpus statistics until rewritten. Concurrently updated locked rows
may have NULL scores. [Scoring](https://planetscale.com/docs/postgres/search/scoring)

Proposed cases: 9/10/11 occurrences among 100 documents; explicit unit boost;
all-zero ties; argument conflict; term replacement changing scores but not matches;
locks and maintenance phases. Never require pre/post-VACUUM scores to remain
identical, or assume a row-scanning oracle shares TIN's physical statistics lifecycle.

## Highlighting and statement lifecycle

Implicit queries are discovered in the same statement, including subqueries,
CTEs and DML actions. Explicit query arguments avoid that dependency. Adjacent
or overlapping highlights merge; tags do not nest. `$QUERY_PART` escapes HTML
and `$QUERY_LABEL` produces a CSS-safe label. ANSI highlighting optionally wraps
at a positive width. **Documentation conflict:** prose for `a BEFORE b` says only
qualifying b occurrences are wrapped, but its example also wraps the witnessing a.
Treat this as unresolved until checked against Lead or clarified upstream.
[Highlighting](https://planetscale.com/docs/postgres/search/highlighting)

Proposed cases: explicit/implicit equivalence, UPDATE RETURNING, MERGE,
ON CONFLICT, CTE discovery, overlapping query-part labels, Unicode byte spans,
and the disputed BEFORE example. Avoid guessing behavior from that prose alone.

## Operations and resource boundaries

Build workers prefer roughly 1 GB each; `maintenance_work_mem` is divided among
them and also budgets write folding. Lower budgets can leave lasting fragmentation.
Segment count limits parallelism. Exhausted worker pools can push maintenance
into writers. `work_mem` bounds identifier sorting/deduplication before spilling.
Index-only count shortcuts need trustworthy live counts and all-visible heap
pages; churn causes heap checks until vacuum/maintenance recover them. Long
transactions delay cleanup. Sequential index access, heap-order results within
segments, and optional readahead are documented. PostgreSQL cost settings price
candidate plans; the docs do not publish crossover thresholds.
[Operations](https://planetscale.com/docs/postgres/search/operations)

Proposed experiments: retain build memory/worker/segment settings with query
results; measure query cost after low-memory builds; test vacuum recovery with
an old snapshot held open; check spill and foreground-maintenance behavior.
This supports continuing our memory-budget work, not copying an undocumented
TIN implementation.

## Index options

One text/citext source per index; partial and expression indexes are supported.
Queries must match the expression and imply partial predicates. Scoring defaults
are k1=1.2 and b=0.75, bounded respectively by [0,10000] and [0,1]. Stop-word entries
are exact stored terms and affect scoring only. Scoring option changes need no
rebuild; analysis changes require REINDEX. Analysis includes boundary/folding,
long-token, grapheme, and position-gap policies; token-byte bounds are [4,2692].
Initial/target segment counts accept [1,4096]; mutable folding defaults to 4 MiB
or 16,384 documents; maximum merged size defaults to 2000 MB and dead fraction
threshold to 0.5. [Indexes](https://planetscale.com/docs/postgres/search/reference/indexes)

Proposed checks: exact option endpoints, partial-index implication, expression
identity, scoring-only option changes, and analysis changes followed by REINDEX.
Reloption compatibility does not prove equivalent storage or maintenance.

## Operator and SQL NULL probes

`==>` accepts a prepared query parameter. `==> ANY(array)` matches any query
element; SQL AND/OR can combine search and ordinary predicates. Multiple indexed
fields contribute to relevance. The operator reference explicitly distinguishes
zero-token queries from invalid empty syntax.
[Operator](https://planetscale.com/docs/postgres/search/reference/operator)

Proposed cases: empty, singleton, duplicate and NULL-bearing query arrays;
NULL document/query parameters; generic versus custom prepared plans; changed
parameters on rescans. NULL behavior is a test question here, not an explicit
contract stated by this page.

## Function contracts

`max_score` is the highest matching score and is constant across rows.
`score_inspect` reports scored terms and boost weights. `tokenize` accepts the
index analysis options. `maybe_quote` protects terms interpreted as syntax.
`fsck` is read-only, ownership-restricted, optionally checks heap TIDs, and returns
no rows for a clean index; repair requires REINDEX.
[Functions](https://planetscale.com/docs/postgres/search/reference/functions)

Proposed checks: maximum consistency against exhaustive scores; inspect versus
scoring configuration; quote/tokenize round trips; fsck permissions and clean
results after build/update/vacuum. Do not infer that Lead's structural checker
can validate Stannum's distinct on-disk representation.

## SQL shapes and planner scope

Ranked LIMIT is the intended bounded retrieval shape. Separate indexed fields
sum their matching contributions, including SQL OR. Unindexed filters require
additional candidates when selective; btree point/range restrictions can narrow
search earlier. Mixed-index OR deduplicates its union. Join scores bind to each
relation's CTID and require each scored side to have a text predicate. Correlated
LATERAL searches can bind the query from each outer row and return per-row top-k.
[SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes)

Proposed cases: cross-column OR with one/both matches; duplicate-producing mixed
OR; join CTIDs that happen to be equal across tables; missing scored-side predicate;
LATERAL repeated/empty/changing queries; restrictive filters that exhaust prefixes.
Benchmark these independently of semantic comparison; no particular plan node
or identical strategy is promised for Stannum.

## Limitations and lifecycle

Partitions have independent BM25 statistics. Every queried partition needs a
usable TIN index; some parent aggregate/window shapes require scores computed
in a MATERIALIZED CTE or a single partition. Freed relation space is reused,
but shrinking requires REINDEX. Concurrent index creation and reindexing are
supported. Replicas require `hot_standby_feedback`; replay can still cause
retryable SQLSTATE 40001 while successful results remain exact.
[Limitations](https://planetscale.com/docs/postgres/search/reference/limitations)

Proposed cases: unequal partition distributions; one missing leaf index;
aggregate/window refusal and workaround; concurrent reindex under writes;
physical-replica replay alongside maintenance. These lifecycle tests need a
real indexed engine and multiple sessions, not merely scalar Lead comparisons.

## Settings and observable strategy families

Documented controls expose custom versus generic scanning; background,
foreground or manual maintenance; build I/O concurrency; RSS-based worker
planning; and per-database maintenance fairness. Fairness is server-level,
not session SET. Page-reuse statistics can appear in EXPLAIN ANALYZE.
Debug choices cover fused/factored Boolean scans, sparse/stripe multi-index
iteration, bounded/exhaustive ranking, conjunction pushdown/adaptation,
streaming/sorted visibility, count pushdown, btree probing and parallel plans.
They preserve semantics and still reject unsafe shapes.
[Settings](https://planetscale.com/docs/postgres/search/reference/settings)

Inference: these are useful axes for comparative experiments and Stannum's own
strategy controls. They reveal neither data structures, SIMD codecs, selection
thresholds, nor a requirement to reproduce those GUCs. Verify actual extension
support before using controls; PostgreSQL accepting an arbitrary custom setting
is not proof the engine implements it.

## Existing coverage and specific gaps in differential checks

The synthetic [oracle](../../benchmarks/oracle.py) already exercises 47 query
shapes through five states: built, deleted, vacuumed, inserted, and reindexed.
It includes wildcard/fuzzy expansion, proximity, span relations, positions,
thresholds, boosts, Unicode, and implicit HTML/ANSI highlighting. This is not a
proposal to reimplement that suite.

The distinction is what gets compared. `observe` collects `max_score`, but
`comparable` omits it in `order` mode. That mode also excludes ranking comparisons
for six documented reference expansion cases while retaining membership and
highlight checks. The fixed single-column SQL does not parameterize scoring
policy or custom highlight tags. Those are narrow, actionable opportunities for
a separate small differential suite; do not remove existing exceptions without
rechecking the actual reference version. The
[reference launcher](../../script/reference-oracle) records that revision.

[PostgreSQL integration tests](../../postgres/src/lib.rs) already cover scoring
policy, runtime bounds, max-score matching scope, prepared plans and rescans,
expression/partial-index binding, tokenizer modes, explicit/implicit highlights,
joins, and partition binding. This is useful same-engine regression coverage,
not proof of differential coverage for every SQL shape. Likewise the
[query-shape campaign](../../benchmarks/query_shapes.py) supplies same-engine
baseline comparisons rather than an independent Lead oracle.

The [calibrated Wikipedia check](../benchmarks/lead-verification-budget.md)
already verifies all 906 forms with exact full-score bits and optimized top-10
agreement on 5,000 rows. Its 851.2-second measured runtime leaves little room in
the 900-second budget. Keep its dataset and contract stable; use tens or hundreds
of purpose-built rows for density endpoints, parse/error contracts, max-score
semantics, scoring arguments, DML query discovery, and highlight labels.

No fresh oracle run was performed in this audit. Implementation changes and
new tests should first verify support in the pinned Lead revision, then classify
any mismatch as a documented divergence, reference limitation, or Stannum bug.

## Recommended use of these findings

1. Audit the existing compatibility matrix and pinned Lead revision first.
   Prefer small targeted additions covering documented thresholds and SQL
   lifecycle transitions over another broad benchmark harness.
2. Separate semantic equality, error contracts, physical lifecycle invariants,
   and performance expectations. Require membership/ranking validation before
   treating a faster execution path as successful.
3. Keep the current allocation/budget work ahead of speculative strategy work.
   Record build provenance and post-build query behavior together.
4. Expand paired performance cases only where the audit exposes an unmeasured
   behavior: mixed predicates, correlated rescans, churn, spill, and partitioning.
   Defer another paid TIN campaign until these local probes are actionable.

These priorities are our synthesis. No fresh TIN experiments or coverage changes
were performed for this research note.
