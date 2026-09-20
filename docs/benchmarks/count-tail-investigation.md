# Full-Wikipedia count tail: investigation starting point

Evidence: AWS baseline at commit `ae21401`, full 5,032,104-row Wikipedia corpus,
302 OR forms, 8 CPU / 32 GiB query limits, 24 GiB shared buffers, 600-second
measurement. No production changes or extra queries were made on the active AWS
sweep to obtain these findings.

## Observations, not yet a CPU profile

- Two clients: 65.51 QPS, p50 7.428 ms, p95 138.436 ms, p99 212.652 ms.
- Four clients: 132.57 QPS, p50 7.363 ms, p95 137.366 ms, p99 209.792 ms.
- Sampled CPU usage scales from 1.97 to 3.94 cores. Two-client samples show
  zero block-device read bytes, zero CPU throttling, and no major page faults.
- The top 30 of 302 query forms account for 56.9% of accumulated query duration.
  Query 302 averages 743.8 ms; query 88 averages 254.5 ms. These are repeated
  shape costs, not merely isolated scheduler outliers.
- All six captured custom-plan examples use scalar counting with zero heap
  fetches. They are not the worst query forms. Nine index segments existed after
  construction. The generic-plan captures are a separate sequential-scan problem;
  their approximately 65-second latency is absent from measured samples.

## Ranked hypotheses and discriminating experiments

1. Materialization/sort and per-posting work dominate broad scalar counts.
   Prediction: streaming ordered segment results lowers allocations and CPU for
   broad queries at identical counts. Compare existing scalar versus streaming.
2. Choosing strategy from individual term density misses dense unions.
   Prediction: forced page execution wins for some broad unions even when every
   term fails the current four-tuples-per-page heuristic. Compare strategies at
   the same query without changing encoding.
3. Linear scanning of union inputs dominates wide queries.
   Prediction: a heap/tournament selector reduces cursor-selection CPU as input
   count increases, but may lose for small unions. Vary width and overlap.
4. Page visibility checks dominate after candidate work is reduced.
   Prediction: profiles remain concentrated in visibility processing despite zero
   heap fetches. Optimize locality while retaining snapshot behavior; do not skip
   checks merely because the fixture is read-only.

The offline analysis in `benchmarks/results/aws-count-tail-investigation/` holds
per-query costs and `replay.sql` selecting six fast-to-slow shapes. These are
ignored local artifacts derived from the retained raw baseline samples. The replay
must run outside measured AWS traffic; it has not yet been executed there.

Follow [the batch-execution decision](../adr/0002-batched-posting-execution-before-format-migration.md)
for the contiguous bitmap / SIMD path. Start with current bytes, compare an
end-to-end scalar baseline, and introduce format migration only after evidence
justifies it. Fix the GIN duration export separately before using its reported QPS;
that defect does not explain Stannum's retained per-query p95 samples.
