# Filter candidates before ranking: implementation and proof plan

Status: proposed, based on code at `7d441c1` (PR #32). No execution strategy or
performance improvement is implemented by this document. Runtime LIMIT work is
independent until both changes meet in ranked scan execution.

The accompanying instrumentation exposes `Exhaustive Score Calls` and
`Top-K Completions` in ranked EXPLAIN ANALYZE plans. Both are cumulative across
rescans (not per-loop averages). The first counts calls from exhaustive ranking,
including completion; it excludes block-max scoring and score projection. The
second counts transitions from a pruned prefix to exhaustive completion, whether
caused by SQL filtering, invisible tuples or further cursor consumption. Existing
`Scored Candidates` describes block-max work and is not a cumulative total of all
ranking work. The new counters add observability, not a strategy change.

## Evidence and the gap in Stannum

The remote TIN experiments in PR #35 measured `history OR war` with an SQL
filter selecting the first 25% of IDs and LIMIT 10. Forced TID pushdown reduced
the million-document median from 378.955 ms to 254.639 ms. A controlled 100k
repeat with stable visibility reduced 29.338 ms to 18.700 ms. Three repetitions
are diagnostic evidence, not confidence intervals or a Stannum speedup claim.
TIN's pushdown emitted fewer text candidates despite touching more index pages.

Stannum currently ranks before applying residual SQL filters:

| Code location | Existing behavior and implication |
| --- | --- |
| `postgres/src/customscan.rs::rel_pathlist_hook` | Builds a base-relation custom path with no child paths. Its estimated rows include SQL restrictions, but ordered startup still charges scoring all text candidates. |
| `remaining_quals`, `plan_search` | Leaves restrictions other than the selected search clause in `scan.plan.qual`. These are evaluated by PostgreSQL's normal scan executor. |
| `gather` | Calls `IndexScorer::top_k` before heap visibility or residual quals; otherwise enumerates all matching TIDs and calls `finish`. |
| `finish` | Scores every supplied TID and partially sorts the requested prefix, or fully sorts without a bound. |
| `search_access` | Fetches visible heap tuples, rechecks inexact text matches, and returns tuples for residual qual evaluation. |
| `complete` | If the executor needs more rows after consuming the pruned prefix, rebuilds the complete scored ordering, excluding already consumed TIDs. Selective SQL filters can trigger this fallback. |
| `postgres/src/score.rs::IndexScorer::score` | Scores physical posting TIDs; resolves visible HOT members back to roots when needed. Existing corpus statistics must remain independent of the SQL filter. |

Existing pieces are useful but do not constitute secondary-index pushdown:
`segment::set::{Slice, Intersection, Union, Difference}` implement ordered TID
operations; `postgres/src/stream.rs::CandidateStream` unions sources in TID/page
order and subtracts dead lists while retaining the captured storage view.
The current ranked path instead gathers, sorts and deduplicates a TID vector.

## Smallest experiment: filter before exhaustive scoring

First establish whether scoring saved exceeds heap work added. Introduce an
explicit, default-off experimental strategy for narrowly eligible filtered
ranked scans. Do not select it globally from an unmeasured selectivity cutoff.

1. Enumerate existing text candidates in root-TID order using the existing
   candidate path. Keep its inexact/recheck indication.
2. Fetch each candidate with the execution snapshot using the same table-index
   fetch protocol as `search_access`. Evaluate the eligible early SQL predicate
   against the visible tuple. Preserve the **input root TID**, not `tts_tid`, for
   surviving rows. Reset per-tuple expression memory and check interrupts.
3. Pass only surviving root TIDs to `finish`, using the unmodified corpus scorer
   and score ordering. Initially retain the final normal heap fetch, original
   text recheck and residual quals. This intentionally fetches survivors twice;
   count that cost rather than hiding it.
4. Rebuild candidate/filter state on rescan; never reuse a mask across parameter
   bindings or snapshots. Use PostgreSQL-owned expression nodes in
   `custom_exprs`, compiled at executor initialization, rather than serializing
   raw pointers into private data.

For this experiment, bypass the existing unfiltered block-max top-k attempt when
the forced early-filter strategy is selected. Otherwise a good initial top-k can
mask the behavior under study. This is an **exhaustive text enumeration** strategy
with fewer scorer calls, not a claim of index-level TID pushdown. Phrase/inexact
text candidates may still be scored unnecessarily; final rechecks preserve
correctness. A later measured optimization can move safe text rechecks earlier.

Start with exactly one conjunctive predicate: a plain local `int2`, `int4` or
`int8` user column compared with a same-type literal by an explicitly verified
built-in `=`, `<`, `<=`, `>` or `>=` operator. No casts or user-defined operators.
Use operator/function OIDs and properties, not names, to establish eligibility.
Treat NULL through PostgreSQL qualification semantics. Keep the original qual.

For the first POC, reject row-security/security-barrier relations or security
quals, row marks, joins/outer references, whole-row/system-column expressions,
subplans, functions, score-dependent predicates, OR/NOT, and volatile or
potentially error-producing expressions. Do not move a predicate across a
security boundary merely because it looks inexpensive. Additional predicates
can remain residual only after proving their evaluation semantics are unaffected;
the simplest initial rule rejects any additional residual qual. Parameterized
filter values are a separate extension after literal-predicate correctness.

This POC overlaps `customscan.rs` with runtime-LIMIT work. Land or rebase onto
that work before implementation; documentation/fixture design can proceed now.
It changes no on-disk format and need not change `score.rs`.

## Correctness gates

- SQL filtering changes candidate eligibility, never BM25 corpus size, document
  frequency, length normalization, term expansion or f32 accumulation order.
- Fetch under `es_snapshot`; a posting can be dead, invisible, or name a HOT
  root whose visible member has a different filter value. Do not match masks of
  visible-member CTIDs directly against root postings.
- Preserve score publication and root/member association used by projection.
  Cursor fetches, rescans, concurrent updates and index-view changes must retain
  existing scorer lifetime guarantees.
- Top-k thresholds may only depend on eligible rows when pruning a filtered
  result. Filtering the unfiltered top-k and returning fewer than k rows is wrong.
- Keep secondary sort keys, OFFSET, WITH TIES, unsupported scoring options and
  unsupported query shapes under their existing conservative rules. Do not
  broaden ranked-path recognition as part of this experiment.
- Compare ordered score bits and result membership with materialized exhaustive
  Stannum scoring; use the pinned approximately 15-minute Lead gate for semantic
  agreement, with tie-aware comparison where SQL leaves ties unspecified.
- Extend existing `postgres/tests/ranked_fuzz.py` scenarios and the prepared-plan
  integration regressions: empty/all-pass filters, NULL columns, filter values
  correlated and anti-correlated with score, deletes, HOT/non-HOT updates,
  rollbacks, VACUUM, repeated execution and partial cursor consumption.

## Benchmark fixture and acceptance

Use the existing local Wikipedia harness and immutable dataset checksum, first
100k rows and then one million only after a positive signal. Add a small named
workload slice rather than a separate benchmark program. Use the actual ID
distribution when selecting cutoffs; a corpus prefix need not have dense IDs.

Measure rare term, common term (`history`), broad OR (`history OR war`) and phrase
(`"united states"`) with filter selectivities approximately 1%, 10%, 25% and 100%,
LIMIT 10 and 100, plus an unbounded diagnostic. Include a deliberately
anti-correlated synthetic fixture so the highest-scoring unfiltered rows fail
the predicate. Benchmark IDs/scores first, then full-body projection separately.
Do not describe `EXPLAIN ANALYZE` without serialization as full client latency.

Compare three executions of identical queries: current automatic path, forced
current ranked path, and forced early-filter strategy. Capture plans to verify
the intended path was used. Use paired A/B/B/A measurement with enough time for
complete workload coverage; reject incomplete trials. Report per-shape medians,
tail latency and dispersion, not just an aggregate QPS. Keep image/commit,
hardware, memory limits, corpus, segment layout and SQL settings attached.

Initially VACUUM explicitly and record visible-page coverage. Repeat the winning
shapes after a reproducible update/delete workload without silently VACUUMing
between alternatives. Keep the same state for each paired run; document warm
cache operation. Network TIN observations are not the local baseline.

Existing EXPLAIN counters are insufficient for this comparison: `Heap Fetches`
counts successful visible fetches, not attempts; `Scored Candidates` is populated
for block-max pruning but not exhaustive `finish`. Add or clarify:

| Counter | Meaning |
| --- | --- |
| Candidate Strategy | Current rank-first or experimental filter-before-score |
| Text Candidates | Deduplicated posting candidates before SQL eligibility |
| Early Filter Accepted / Rejected | Visible candidates passing/failing the predicate |
| Invisible Candidates | Candidate roots with no visible tuple |
| Score Evaluations | Actual scorer calls, including completion/repeated work |
| Heap Fetch Attempts / Visible Fetches | Separate early filtering from final emission |
| Top-K Completions | Number of fallbacks from pruned prefix to exhaustive ordering |

Retain normal PostgreSQL buffer and residual-filter instrumentation. Do not emit
an idle leader's zero counters as a measurement of workers.

Promote only if exactness gates pass and repeated paired end-to-end results show
a useful selective-query win. Explicitly quantify losses on rare-term and
all-pass-filter controls. A negative result is useful: record whether heap fetches,
candidate enumeration, score calls or startup dominates before choosing a more
complex implementation. No fixed percentage cutoff is justified yet.

## Subsequent step: secondary-index eligibility

If the POC shows scoring savings but early heap probing dominates, investigate
a native PostgreSQL secondary-index/bitmap child path that produces an eligibility
set. The existing custom path has no child today; obtaining such a path safely
requires planner integration rather than running an SPI query during execution.
Account for child construction cost, memory/spill behavior, lossy bitmap pages,
SQL rechecks, HOT identity and parameter dependencies. A lossy page admits a
superset and cannot justify accepting a row without recheck.

Only after canonical root identity is established can ordered set/page-mask
intersection safely precede scoring. Existing cursor/page primitives provide a
representation to prototype; they do not solve visibility or HOT normalization.
Eventually a membership/seek interface could also restrict block-max traversal,
with thresholds updated exclusively by eligible candidates. That is a separate
proof from the exhaustive-filter POC and must preserve safe score upper bounds.

Do not hard-code TIN's strategy thresholds. Build an explicit strategy enum and
explain the choice, initially with forced alternatives; add a conservative cost
rule only after the crossover is measured on Stannum. Runtime query bounds and
predicate selectivity inform this choice; instantaneous queue load is out of
scope for the first implementation.
