# Performance priorities after the wide-disjunction optimization

This is a task order, not a percentage of TIN implemented. Correctness against
upstream Lead remains a separate gate; GIN and remote TIN timings alone cannot
establish compatibility or production capacity.

| Priority | Task | Evidence required to call it done |
|---|---|---|
| 1 | Validate wide ranked OR under concurrent readers and match-changing writes | Repeated baseline/candidate runs, identical initial data and resources, verified ranked path, actual completed writes, read throughput and latency tails, and snapshot-consistent correctness during traffic. The [initial bounded diagnostic](wide-contention.md) passes; follow it by extending the existing cursor concurrency fuzzer to 32/128-term queries, then run sustained mixed inserts/deletes/updates and maintenance. Exact cross-statement scores require quiescent index statistics; live membership checks alone do not prove ranking. |
| 2 | Measure memory and larger working sets | Scale the verified Wikipedia corpus and vary memory limits; record backend/container peak memory, disk reads, spills, errors and latency under concurrent load. Establish whether gains survive outside warm cache before another algorithm change. |
| 3 | Close the query-shape coverage gaps | Add fuzzy/regex/proximity, deeply nested Boolean trees, large K/OFFSET and secondary sorts, partitioned ranked queries, and cross-session snapshot/locking scenarios. Keep the routine Lead oracle within its roughly 15-minute budget; use larger same-engine checks for scale. |
| 4 | Revisit filtered top-K continuation | Preserve the fast initial top-K search; activate continuation only after filter shortfall. Prove bounded memory, multi-source correctness and concurrent behavior, and repeat both winning and regressing controls before enabling it. Draft PR #45 and the decoded-document cache prototype are not ready to merge. |
| 5 | Run the next controlled TIN comparison and prepare publication | Freeze passing builds, identical data and SQL semantics, record server-side timings and plans, and show machine/storage differences. Use the existing PlanetScale/ParadeDB driver and retain disagreement/error cases. Provision paid machines only for the bounded comparison window. |
| 6 | Consolidate benchmark entry points | Retire old harnesses only after their distinct correctness, mutation and maintenance coverage is represented in the retained tools. Keep the Lead oracle separate from large-scale throughput tests. |

Already demonstrated: PR #51 fixes ranked EXISTS/NOT EXISTS; all 44 query-shape
cases pass. PR #52 reduces the measured 128-term synthetic OR from about 147 to
57 ms, and a 32-term Wikipedia OR from about 52 to 46 ms. Those are single-client
observations, not a claim of being faster than TIN. See [the paired evidence](grouped-wand.md).
