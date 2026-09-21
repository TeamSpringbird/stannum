# Performance and benchmark burndown

Updated 2026-09-21. This is an ordered work list, not a percentage of TIN
implemented. Lead remains the independent semantic oracle. Published TIN,
managed-service observations, and local measurements are separate evidence.

## Profile-driven execution priorities (2026-09-21)

The completed AWS campaign matches the published instance class/resource target,
not a controlled execution of TIN on the same machine. It profiles Stannum only.
These findings explain measured Stannum costs; they do not attribute every part
of the published TIN gap. This execution list supersedes the broader ordering
below for the next optimization iterations.

### Evidence

Source: `benchmarks/results/aws-crossover-controller-r4/export/checkpoint/`.
Each phase contains 302 OR queries with nine server timings per mode. CPU
profiles cover queries 302, 88, 139 and 146 separately, not the whole workload.
Percentages below are top-level sampled-cycle attribution, not summed call-tree
percentages or precise wall-time fractions. Some mutated profiles contain
unresolved symbols; use the resolved 139/146 profiles for named attribution.

- Clean query 302: default 730.847 ms versus page 275.692 ms; zero heap fetches.
  Default profile: union advancement 44.64%, posting cursor current 30.40%,
  sparse load-next 8.36%, varint decode 4.58%, sort 0.55%.
- Page query 302: scalar-to-page Rows advancement 35.84%, sparse load-next
  15.00%, page union selection 12.86%, Rows current 9.92%, varint decode 8.54%.
  Actual offset-mask union accounts for only 2.94% of sampled cycles.
- Mutated query 139: 570.727 ms default versus 760.233 ms page;
  2,066,993 heap fetches in each. Default profile: heap_hot_search_buffer
  33.39%, hash_search_with_hash_value 16.15%, sort 8.65%. Query 146 likewise
  attributes 32.15% to HOT search and 16.97% to hash lookup. Buffer/visibility
  work is a different bottleneck from the clean index-only case.
- Immutable segments rise from 9 to 128 after mutations, then return to 9 after
  vacuum. Visibility and segment layout change together; these are confounded.
- Retrospectively choosing the faster mode for each query gives a zero-estimation-
  cost upper bound of 1.520x on summed clean query medians, 1.004x mutated,
  1.657x revacuumed. Only 49/302 clean and 1/302 mutated queries favor pages.
  This is an oracle bound for these samples, not a throughput prediction.

### Ordered implementation experiments

| Rank | Work / cost mechanism | Smallest useful experiment | Evidence and promotion gate |
|---|---|---|---|
| 1 | Reduce repeated cursor-head inspection and union selection | Cache input heads; specialize tiny unions; compare linear, heap and tournament selection only where width warrants it. Preserve duplicate handling, seek and cancellation. | Two scalar hotspots consume about 75% in query 302. Compare widths, overlaps and 9/128 sources; require a real-query CPU/latency reduction, not only a synthetic win. The old heap prototype already lost on dense cases, so no unconditional heap replacement. |
| 2 | Decode directly into batches/page masks | Prototype sparse-posting page/batch decoding from current bytes, bypassing repeated Tid/current/advance/Rows adapter calls. Fuse grouping with decode; benchmark scalar batch and SIMD kernels separately on ARM and x86. | Rows advancement alone is about 36% in forced-page query 302. Verify byte-boundary/offset corruption checks and exact memberships. New disk format is a later choice, not a prerequisite. SIMD mask OR alone targets only about 3% here. |
| 3 | Validate the cheap default/page selector | Freeze the 100k trial rule, evaluate larger/dirty snapshots and fresh query identities, then replace raw posting-count thresholds with measured features only where needed. Charge all feature/decision time. | Best-choice ceiling is limited. Prioritize low regret, default fallback and cheap estimation; do not repeatedly tune against a supposedly held-out set. Compare against the same instrumented binary and forced modes. |
| 4 | Reduce repeated work for non-all-visible heap pages | Instrument fetches, buffer lookups and HOT work per page; prototype page-local visibility processing using supported PostgreSQL access methods. | Millions of heap checks and about one-third HOT-search attribution in resolved dirty profiles. Preserve snapshots, HOT, row locks, predicate locking and AM semantics. Never infer visibility from index membership or cache it across snapshots. |
| 5 | Reduce segment read amplification | Hold logical data and visibility state fixed while varying segment count; test merge policy and pruning of sources irrelevant to a query. | 9 to 128 sources is a strong lead, not an isolated proof. Track read gains against write latency, WAL, merge memory and maintenance cost. Prioritize only if the controlled experiment confirms material cost. |
| 6 | Cut materialization, sorting and allocations where profiles justify it | Compare bounded ordered merge and batch output with the existing collect/sort path on fragmented states. Retain simple materialization for sparse queries. | Sort is only 0.55% in clean 302 but 8.65% in dirty 139. Previous unconditional streaming was not a win. Require end-to-end benefit and reduced peak memory. |
| 7 | Extend selection within query subtrees and to ranked work | Profile mixed AND/OR, selective-lead/dense-filter, phrase and filtered top-K workloads; prototype one subtree adapter/strategy at a time. | Current evidence is OR counts and does not diagnose the other three published ranked workloads. Bound conversion cost and verify exact scores/rechecks before promotion. |
| 8 | Optimize memory layout/encoding once executor costs shrink | Profile cache misses, bytes decoded and memory pressure after batching; compare block codecs/containers with a migration-compatible prototype. | Potential architectural upside, currently less quantified than the cursor hotspots. Require whole-query gains before accepting format migration complexity. |

### Tight loop and current selector result

1. Pin binary, snapshot and query trace. Use four representative profiled queries
   plus sparse controls for diagnosis, then the full 302-query corpus and a fresh
   validation set. Change one mechanism per candidate.
2. Check exact counts/membership and maintenance semantics before timing. Separate
   profiled runs from unprofiled measurements; no builds/browser checks or other
   heavy jobs during the promotion runs.
3. Measure paired server execution, estimator time, CPU, allocations/bytes where
   available, heap fetches and source count. Report per-query regressions and
   total weighted work alongside p50/p95/p99. Evaluate a chosen query weighting
   explicitly; a tail percentile alone is not throughput.
4. Repeat promising candidates in alternating order on quiet clean/dirty snapshots,
   then at eight clients. Promotion requires repeatable end-to-end improvement
   with no material throughput or protected sparse-query regression. Freeze the
   numerical tolerances before examining fresh validation outcomes.
5. Re-profile after each win. Use AWS to confirm x86/full-scale transfer of local
   winners, not for every exploratory iteration. Keep default behavior until a
   candidate passes the gate.

The first actual selected execution finished: on 100k, median-round p95 is
0.941 ms versus 1.333 ms for the same instrumented binary; summed per-query
medians are 76.349 versus 86.502 ms (about 12% lower). However p50 is 0.078
versus 0.068 ms. This is a promising tail tradeoff, not a promotion result.
The rule was retrospectively calibrated on this corpus and round spread is
substantial. Repeat under quiet conditions and validate fresh data.

The million-row dirty shadow run also finished, with estimation median 3.834 us,
p95 7.917 us, 0.0088% of summed server execution. Its shadow p95 is 165.421 ms
versus 161.696 ms instrumentation-only, so the internal timer alone cannot
establish negligible end-to-end overhead. These were development runs; marketing
preview/lint activity overlapped part of the dirty run. Repeat without that
activity before claiming an overhead bound. Correctness and strategy-decision
checks passed; detailed reports are retained under
`benchmarks/results/selector-threshold-100k-r1/` and
`benchmarks/results/selector-overhead-million-mutated-r1/`.

## Current focus

Query performance is the number-one priority: explain and reduce the slow tail
using the full-corpus AWS count-strategy comparison, then validate improvements
against broad query coverage. Snapshot reuse and candidate compatibility checks
support those comparisons. Parallel index construction is deferred setup-time
work; it must not displace query optimization or interrupt the current baseline.

## Earlier campaign context (2026-09-20)

- PR #77 is merged: ranked correctness is checked again after concurrent updates.
- Both published corpora are downloaded and checksum-verified. The confirmed
  Stack Exchange trace contains 1,254 samples / 3,762 AND/OR/phrase forms.
- The 1,000-document Stack Exchange Lead run passed all 3,762 forms. Larger
  same-engine membership and exhaustive ranked checks also have retained receipts.
- A four-workload local campaign is running: each engine gets 600 seconds on
  100,000-document prefixes, with four CPUs, 2 GiB memory and two clients.
  It checks 90 selected forms; that is not full-trace correctness verification.
  Builds and engines are serialized, but this is a shared development machine.
  Interactive site work during the campaign is a source of uncontrolled load.
- The article preserves published series and explicitly synthetic local fixtures.
  Completed measurements are imported separately; fixtures are not benchmark data.
- Draft PRs #61, #63 and #69 remain experimental merge-spill work. Draft #45
  remains a filtered top-K experiment with known regressions. None is a release gate
  for measuring current main, and none should merge simply to clear the queue.

## Ordered priorities

| Rank | Work | Evidence required to call it done |
|---|---|---|
| 1 | Profile and improve query latency, especially p95 | Use the completed per-family/per-query evidence to choose targets, starting with the full-corpus OR-count strategy comparison and then expensive ranked families. Validate candidate binaries on fresh copies of the baseline snapshot before timing. Attribute time to cursor advance, block bounds, decoding, scoring, filtering and heap work in separate diagnostic runs. Implement one change at a time, prove it against repeated broad-trace controls, and check AND/phrase regressions. Keep draft #45 disabled until its losing cases are understood. |
| 2 | Finish and audit the four ten-minute local workloads | Both engines complete each workload; retain failures, query errors, actual durations, distinct query-form coverage, achieved update counts, validation scope and resource samples. Check the mixed AND/OR/phrase, AND+phrase, OR+updates and Wikipedia OR-count configurations. Verify exported charts against receipts before replacing fixtures. Never pad a short run or silently omit a failed engine. |
| 3 | Establish repeatability and complete workload coverage | Reuse existing repeated/paired measurement support. Run at least three quiet repetitions of the workloads that will support claims, alternating engine order. Report run-to-run spread, p50/p95/p99, throughput and coverage. If a 600-second window misses forms, keep the time-limited result and add a separately labeled coverage-complete run; do not substitute its aggregate silently. Measure timing without profilers or concurrent builds. |
| 4 | Move beyond the resident 100k subsets | Start with the verified 5,032,104-row Wikipedia corpus, keeping construction and query limits separate. Record load/build time, heap/index/WAL sizes, build/query memory, reads, OOMs and temporary/spill usage where available. Vary memory around the measured working set. Then use increasing Stack Exchange prefixes, with full 150-million-row construction reserved for appropriately sized infrastructure. Retain the broad trace, not only hot queries. |
| 5 | Finish bounded merge memory where scale measurements justify it | Compose the existing streaming dictionary/postings/payload readers with whole-segment and document ownership validation; then integrate incremental output and account for document maps, source caches and retained inputs/output. Demonstrate a measured peak-memory bound, cancellation, rollback, orphan cleanup and crash recovery. Output-buffer spilling alone does not bound the entire operation. Reuse drafts #61/#63/#69 rather than promote them prematurely. |
| 6 | Expand semantic and concurrent-maintenance coverage | Keep the routine Lead gate near 15 minutes and pinned to an upstream revision. Add focused fuzzy/regex/proximity, nested Boolean, large K/OFFSET, secondary-sort and partition cases; extend current whitespace updates with inserts, deletes, indexed/non-indexed updates and VACUUM. Test snapshot/locking behavior separately. Require no incorrect rows or crashes; use quiescent ranked checks where concurrent corpus statistics prevent a stable score oracle. Track shared Lead bugs as diagnostics rather than silently changing compatibility. |
| 7 | Run the controlled AWS comparison | Freeze candidate image/source, driver, data/query hashes and settings. For comparison with published TIN, use the documented published hardware/resource target and open-source baselines; this does not require a live TIN server. A fresh live TIN comparison is a separate managed PlanetScale experiment. Preserve server execution time separately from client time, repeat measurements, export raw evidence, then verify teardown. State remaining CPU/storage/settings differences; do not scale TIN numbers by a GIN ratio. |
| 8 | Finish the article and consolidate tooling | Replace fixtures only with audited measured series and unambiguous provenance. Show original published results separately from local/AWS runs, including unsupported workloads and failures. Retire harnesses only after mapping and preserving their distinct correctness, mutation, maintenance and recovery coverage. Keep Lead checks separate from throughput tests. |
| 9 | Parallel index construction (deferred behind query performance) | Profile the serial build first. Prototype PostgreSQL parallel workers building independent segments with a shared memory budget and coordinated publication; preserve the page/segment encoding. Compare 1/2/4/8 workers for build time, peak memory and I/O, and verify correctness, cancellation, recovery, index size and subsequent query performance because segment boundaries can change. Worker settings alone cannot parallelize the current builder. Do not assume linear speedup. |

## Parallel work and decision rules

While the current timed campaign runs, work on the evidence inventory, review
existing profiles, plan coverage and audit documentation. Do not launch another
benchmark, compilation or heavy verification job on this host.

After it finishes, use the query profiles and repeated measurements to choose
the next query optimization. Construction and merge-memory work moves ahead only
when it blocks trustworthy query experiments. Parallel construction remains
deferred while the serial build and snapshot reuse are sufficient. A confirmed
correctness failure takes precedence over performance work.
The measurement and correctness tracks may have independent code owners, but
their local execution still shares the benchmark/test lock.

No remaining optimization is a prerequisite to taking an honest AWS baseline
of current main. Repeated local measurements and an explicit resource/run plan
reduce wasted cloud time; they are not reasons to withhold existing results.

See [AWS readiness](aws-next-run-readiness.md),
[raw scale/update receipts](raw-scale-and-updates.md),
[published-trace protocol](published-trace.md), and
[verified datasets](published-datasets.md).

## Iteration 1: cached union heads

Implemented on `perf/cached-union-heads` at `104ff38`. The union caches input
heads and refreshes only moved inputs; advance selects the next head in the
same pass. No encoding or strategy-selection change. A deterministic probe
went from 33 head reads to at most 9. Segment tests (123 passed, 2 ignored),
all 310 tinql tests and four PostgreSQL count/visibility tests passed.

Three balanced million-row clean rounds, 302 queries, five timings per query:
median-round p95 13.828 -> 11.790 ms, p50 0.773 -> 0.739 ms; summed per-query
medians 922.593 -> 821.437 ms. These are serial diagnostics, not concurrent
request percentiles. All counts and strategy comparisons passed. Optimized
control/candidate binaries are frozen with SHA checks; native profiles for
queries 302 and 88 were collected after timed trials. Evidence:
`benchmarks/results/cached-union-r1/million-clean/`. Eight-client clean/dirty
validation is running before any promotion decision.
