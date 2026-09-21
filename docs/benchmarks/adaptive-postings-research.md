# Adaptive posting execution: primary-source research

Researched September 20, 2026. This is an architecture recommendation, not a
benchmark result or implementation. Scope: count execution over the existing
pageV2/LSG3 format. No persisted representation change is proposed.

The working observation is that default counts win smaller queries while page
bitmaps improve the tail; the earlier density-only rule did not transfer from
100k to 250k dirty data. These observations motivate investigation; aggregate
p50/p95 alone cannot identify the best strategy for an individual query. The
[existing bitmap note](page-bitmap-counts.md) documents the original four-tuples-
per-occupied-page heuristic and its narrower, vacuumed 100k evaluation.

## What the sources establish

### 1. SIMD intersection depends on operand imbalance and distribution

Lemire, Boytsov and Kurz compare several SIMD intersection algorithms and
galloping. Their measured winners change with the ratio of input lengths;
clustering also changes performance. Section 7.7 evaluates hybrids that first
intersect compressed lists and then probe the remaining candidates against
bitmaps. Their experiments partition the corpus to keep intermediate results
within cache. This supports considering cardinality ratio, access pattern and
representation together. It does not establish a crossover for PostgreSQL heap
offsets or a dirty count workload. Their published 50:1/1000:1 dispatch constants
belong to those algorithms and experiments, not to Stannum.
[Paper, §§7.6–7.7](https://arxiv.org/pdf/1401.6399).

**Proposed application:** retain a cheap scalar/seek path for a rare lead in
`rare AND common`; consider SIMD only after separating decode, seek, Boolean
algebra and visibility costs. SIMD is an implementation option for individual
kernels, not a reason to force all queries through bitmap construction.

### 2. Roaring adapts locally and accounts for representation

The CRoaring paper partitions the integer universe into 2^16-value containers
and uses arrays, bitsets or runs. Container types can change after operations.
Its 4096-element array/bitset boundary corresponds to the space tradeoff between
16-bit array entries and an 8192-byte bitset. It is not a universal latency
threshold. The paper also documents specialized operations for different
container pairs and SIMD implementations.
[Roaring implementation paper, §§3–4](https://arxiv.org/html/1709.07821v4).

**Proposed application:** use this as evidence for local physical choices, not
for importing Roaring or its constants. Stannum already has page masks; their
construction cost, five-word operations, occupied-page counts and conversion
back to candidates are the relevant units. Two operands with equal global
density can have different local page overlap and conversion costs.

### 3. Lucene uses the cheapest conjunction lead plus bitset filters

`ConjunctionDISI.createConjunction` sorts ordinary iterators by estimated cost.
Bitset iterators whose cost exceeds the minimum can become membership filters
behind the lead. Two-phase verification is ordered by `matchCost()` and stops
on the first failure. Nested conjunctions can be flattened. These are concrete
examples of mixed physical execution and separating candidate generation from
verification.
[Pinned ConjunctionDISI source](https://github.com/apache/lucene/blob/2165146d01c9d640afc61dada51e911c0234e1d8/lucene/core/src/java/org/apache/lucene/search/ConjunctionDISI.java).

**Proposed application:** represent exactness and positional/recheck needs
separately from candidate representation. A cheap conjunction lead should be
able to drive a more expensive Boolean subtree without fully enumerating that
subtree. This requires a suitable seek/probe interface; it is not achieved by
merely selecting the current whole-query bitmap evaluator.

### 4. Lucene passes parent demand into disjunction execution

`BooleanScorerSupplier` bounds `leadCost` by its own estimated cost and passes it
to child scorers, including optional subexpressions. Its physical choices also
depend on scoring mode and minimum-should-match semantics.
[Pinned supplier source](https://github.com/apache/lucene/blob/2165146d01c9d640afc61dada51e911c0234e1d8/lucene/core/src/java/org/apache/lucene/search/BooleanScorerSupplier.java).

`DisjunctionDISIApproximation` distinguishes exhaustive union traversal from
advancing a union under a selective filter. It splits clauses between a heap
and a linear list using operand costs and parent lead cost, to avoid repeated
heap reordering when many children advance together. Its bulk array loading
uses a bounded bitmap window, amortizing heap updates across matching IDs.
[Pinned disjunction source](https://github.com/apache/lucene/blob/2165146d01c9d640afc61dada51e911c0234e1d8/lucene/core/src/java/org/apache/lucene/search/DisjunctionDISIApproximation.java).

**Proposed application:** `rare AND (common1 OR common2)` and a standalone
`common1 OR common2` deserve different execution consideration. Pass expected
parent demand when estimating the OR subtree; do not infer its strategy from
the same leaf-density statistic alone. Lucene's constants are not transferable.
The source links pin the audited upstream revision, not a released-version API
promise.

### 5. Subtree alternatives need costs and physical properties

Cascades organizes equivalent expressions into groups, optimizes child
expressions with required physical properties and cost limits, and reuses
previous optimization results. This supplies an architectural basis for
comparing multiple physical implementations of the same logical subtree; the
1995 paper is not evidence that a full optimizer would pay for small Stannum
counts.
[Graefe, The Cascades Framework for Query Optimization, §2](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf).

**Proposed application:** use a small bounded alternative set, with explicit
scalar↔page conversion costs and exactness/ordering properties. Do not start
with unrestricted Boolean rewriting or a full Cascades implementation.

### 6. Runtime adaptation and learned steering are later options

Smooth Scan morphs index-like access toward sequential scanning during execution
as observed access patterns change, reducing reliance on an upfront cardinality
estimate. It was implemented in PostgreSQL. This supports investigating bounded
runtime adaptation; it does not establish correctness or speedups for switching
Stannum count representations.
[Author-hosted ICDE 2015 paper](https://www.renata.borovica-gajic.com/data/ICDE15_smooth.pdf).

Bao chooses per-query optimizer hints using a learned model and Thompson
sampling. Its useful architectural idea is steering a small set of existing
execution choices rather than replacing every optimizer component. For Stannum,
a small measured cost model should precede neural models or production bandit
exploration; short queries leave little room for selection overhead.
[Bao paper](https://arxiv.org/abs/2004.03814).

The 2023 comparison of simple adaptive processing with learned optimizers found
that LIP plus adaptive join selection could match or beat the evaluated learned
systems on several workloads. Its findings are workload-specific, and it also
measures adaptation/filter overhead. This reinforces measuring simple baselines
and total overhead rather than assuming a learned selector is necessary.
[Zhang et al., PVLDB 2023](https://www.pdl.cmu.edu/ftp/Database/p2962-zhang.pdf).

## Recommended sequence for Stannum

1. **Measure a cheap query-level selector first.** Keep current default and
   forced bitmap modes as diagnostic baselines. The current default is not a
   guaranteed scalar-only control; add a genuine forced-scalar mode if needed. Use already available metadata:
   leaf count, operator shape, minimum/sum posting counts, occupied pages,
   representation mix, source/segment count, and positional/inexact flags.
   Treat unavailable or stale estimates conservatively. Reading all postings or
   fetching visibility-map pages merely to choose a plan can erase small-query
   gains; include selector time in end-to-end latency.
2. **Estimate work, not density alone.** A candidate model is
   `setup + decode/seek + Boolean operations + representation conversion +
   visibility/recheck`. For bitmap work use occupied pages and words combined;
   for a selective AND include expected lead candidates and seek/probe work.
   Dirty heap checks may dominate both strategies. Do not assume unchanged
   visibility cost when different candidate production changes heap access
   order or deduplication. Fit a few interpretable coefficients; retain the current
   default when the predicted gain is too small or uncertain. A bounded
   decision tree is also reasonable if it is validated identically.
3. **Add subtree choices only after this baseline is understood.** Prototype
   the selective-lead/dense-filter case and mixed AND/OR first. Compare sparse
   output and page-mask output for each node, charging adapters at boundaries.
   Keep positional work on a supported exact/recheck path. Avoid repeatedly
   converting the same intermediate result. Keep ordinary and ranked search
   outside this count-only experiment.
4. **Consider segment/page adaptation later.** Local density can vary inside
   one query. Switching at a monotonic page boundary could use observed work
   and bounded scratch space, but this is an unproven Stannum proposal. It must
   preserve cursor state, deduplication and exactness across the switch; startup
   sampling and switching costs require measurement. Do not start with online
   learning or production exploration before the cheap baseline has evidence.

## Correctness and validation gates

Execution choice may change cost, never semantics. Preserve source-local dead
posting subtraction, deduplication across sources, the existing non-empty-
document universe for NOT, conservative handling of inexact complements,
positional/expansion-cap rechecks, and snapshot/HOT visibility behavior
described in the [existing implementation note](page-bitmap-counts.md).
PostgreSQL's visibility documentation explains why an index candidate alone
does not prove snapshot visibility, and why a page's all-visible status can
avoid otherwise necessary heap visits.
[PostgreSQL 18 index-only scans](https://www.postgresql.org/docs/18/indexes-index-only-scans.html).
Popcount is valid only where both candidate exactness and the existing
visibility conditions authorize it; “dense” is never such authorization.

Use independent same-snapshot correctness oracles for nested Boolean and
positional expressions, duplicates, absent terms, NOT, updates/deletes/HOT,
and all-visible versus dirty pages. If runtime switching is introduced, compare
forced switching at boundary positions with both fixed strategies.

Freeze features, coefficients and the decision rule on development data before
held-out evaluation. The already inspected 250k dirty result is no longer an
untouched holdout. Reserve fresh query identities, scales, mutation states and
workloads; do not repeatedly retune against them. Compare all three modes on
matched states with alternating/randomized run order. Record per-query
decisions, selector overhead, candidates/pages/heap fetches, and memory, plus
p50/p95/p99 and throughput. Report regressions by query class and selection
regret against the faster forced mode, accounting for measurement noise. An
oracle chosen from timings is an evaluation bound, not an executable selector.
Promote only if the protected small-query cases and independently measured tail
both meet predefined tolerances. No source above supplies those tolerances or
proves the proposed model will transfer.

## First implementation: bounded count experiment

The count node now reports `Count Selection Time` (milliseconds, cumulative
across executions/rescans) and `Count Selection Calls` under EXPLAIN ANALYZE.
This measures the existing `prefers_pages` policy plus any experimental work.
It excludes shared query parsing, opening the index view, and actual execution.
Two monotonic clock reads are added even with experimental selection disabled;
compare against the previous binary to measure that instrumentation cost too.

`SET stannum.profile_count_selection = on` enables shadow feature collection.
`Count Estimation Time` includes source-list allocation, AST traversal,
dictionary lookups, feature aggregation, the decision, and temporary cleanup.
It is a subset of selection time, not an additional execution-time component.
`Count Estimation Calls` accumulates over rescans; feature fields marked `(Last)`
represent only the most recent execution. Plain EXPLAIN emits no measured times.

Collection supports plain term disjunctions (including boosts), at most 256
visited AST nodes, 512 sources and 1,024 term/source lookups. It records input
posting counts, not output cardinality; duplicates and dead entries can inflate
these values. It performs no posting decoding, expansion or visibility probes.
Unsupported shapes retain the previous strategy. Shadow profiling leaves the
previous strategy unchanged.

`SET stannum.count_page_threshold = N` with N > 0 enables an **uncalibrated
experimental** rule: supported ORs whose summed input posting count reaches N
use page counts. Zero (default) disables the rule. This parameter exists for
paired calibration, not as a recommended production threshold or a complete
cost model. Existing page choices are preserved, and `force_count_pages` takes
precedence and bypasses estimation entirely. Execution, visibility, dead-list
handling, and on-disk formats are unchanged.

Next validation: compare previous binary, instrumentation-only, shadow and
threshold modes on identical physical snapshots; report estimator time per
call and as a fraction of total server time, paired net savings, regret and
small-query regressions. Use fixed eight-client load runs separately from
serial EXPLAIN diagnostics. Freeze a threshold on development data before
fresh held-out evaluation. No threshold is enabled by default until that gate
passes. Follow-up features (operator skew, local density, conversion costs)
need their own measured collection budgets before expanding this policy.

Initial validation: the metadata-budget/fallback unit test and PostgreSQL 18
integration test passed locally. The integration test checks shadow versus
selected execution against a text predicate through HOT updates, deletes and
indexed-text changes, verifies timing fields and forced-mode precedence, and
ensures plain EXPLAIN does not report execution timings. These are correctness
checks, not evidence of a profitable threshold or negligible overhead.
