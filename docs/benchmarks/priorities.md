# Performance and benchmark burndown

Updated 2026-09-20. This is an ordered work list, not a percentage of TIN
implemented. Lead remains the independent semantic oracle. Published TIN,
managed-service observations, and local measurements are separate evidence.

## Current state

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
| 1 | Finish and audit the four ten-minute local workloads | Both engines complete each workload; retain failures, query errors, actual durations, distinct query-form coverage, achieved update counts, validation scope and resource samples. Check the mixed AND/OR/phrase, AND+phrase, OR+updates and Wikipedia OR-count configurations. Verify exported charts against receipts before replacing fixtures. Never pad a short run or silently omit a failed engine. |
| 2 | Establish repeatability and complete workload coverage | Reuse existing repeated/paired measurement support. Run at least three quiet repetitions of the workloads that will support claims, alternating engine order. Report run-to-run spread, p50/p95/p99, throughput and coverage. If a 600-second window misses forms, keep the time-limited result and add a separately labeled coverage-complete run; do not substitute its aggregate silently. Measure timing without profilers or concurrent builds. |
| 3 | Move beyond the resident 100k subsets | Start with the verified 5,032,104-row Wikipedia corpus, keeping construction and query limits separate. Record load/build time, heap/index/WAL sizes, build/query memory, reads, OOMs and temporary/spill usage where available. Vary memory around the measured working set. Then use increasing Stack Exchange prefixes, with full 150-million-row construction reserved for appropriately sized infrastructure. Retain the broad trace, not only hot queries. |
| 4 | Finish bounded merge memory where scale measurements justify it | Compose the existing streaming dictionary/postings/payload readers with whole-segment and document ownership validation; then integrate incremental output and account for document maps, source caches and retained inputs/output. Demonstrate a measured peak-memory bound, cancellation, rollback, orphan cleanup and crash recovery. Output-buffer spilling alone does not bound the entire operation. Reuse drafts #61/#63/#69 rather than promote them prematurely. |
| 5 | Profile the expensive ranked query families | Use the completed per-family/per-query evidence to choose targets, with wide OR the current candidate. Attribute time to cursor advance, block bounds, decoding, scoring, filtering and heap work in separate diagnostic runs. Implement one change at a time, prove it against repeated broad-trace controls, and check AND/phrase regressions. Keep draft #45 disabled until its losing cases are understood. |
| 6 | Expand semantic and concurrent-maintenance coverage | Keep the routine Lead gate near 15 minutes and pinned to an upstream revision. Add focused fuzzy/regex/proximity, nested Boolean, large K/OFFSET, secondary-sort and partition cases; extend current whitespace updates with inserts, deletes, indexed/non-indexed updates and VACUUM. Test snapshot/locking behavior separately. Require no incorrect rows or crashes; use quiescent ranked checks where concurrent corpus statistics prevent a stable score oracle. Track shared Lead bugs as diagnostics rather than silently changing compatibility. |
| 7 | Run the controlled AWS comparison | Freeze candidate image/source, driver, data/query hashes and settings. For comparison with published TIN, use the documented published hardware/resource target and open-source baselines; this does not require a live TIN server. A fresh live TIN comparison is a separate managed PlanetScale experiment. Preserve server execution time separately from client time, repeat measurements, export raw evidence, then verify teardown. State remaining CPU/storage/settings differences; do not scale TIN numbers by a GIN ratio. |
| 8 | Finish the article and consolidate tooling | Replace fixtures only with audited measured series and unambiguous provenance. Show original published results separately from local/AWS runs, including unsupported workloads and failures. Retire harnesses only after mapping and preserving their distinct correctness, mutation, maintenance and recovery coverage. Keep Lead checks separate from throughput tests. |

## Parallel work and decision rules

While the current timed campaign runs, work on the evidence inventory, review
existing profiles, plan coverage and audit documentation. Do not launch another
benchmark, compilation or heavy verification job on this host.

After it finishes, prioritize #2 and #3 over speculative query optimizations.
If larger construction fails due to retained merge memory, #4 becomes the next
implementation task. If the working set fits comfortably and ranked OR dominates,
#5 moves ahead. A confirmed correctness failure takes precedence over either.
The measurement and correctness tracks may have independent code owners, but
their local execution still shares the benchmark/test lock.

No remaining optimization is a prerequisite to taking an honest AWS baseline
of current main. Repeated local measurements and an explicit resource/run plan
reduce wasted cloud time; they are not reasons to withhold existing results.

See [AWS readiness](aws-next-run-readiness.md),
[raw scale/update receipts](raw-scale-and-updates.md),
[published-trace protocol](published-trace.md), and
[verified datasets](published-datasets.md).
