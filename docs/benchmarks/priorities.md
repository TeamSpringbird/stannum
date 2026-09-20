# Performance priorities after the wide-disjunction optimization

This is a task order, not a percentage of TIN implemented. Correctness against
upstream Lead remains a separate gate; GIN and remote TIN timings alone cannot
establish compatibility or production capacity.

| Priority | Task | Evidence required to call it done |
|---|---|---|
| 1 | Bound merge construction memory without a batch-size regression | [Allocation profiling](build-memory-results.md) finds a 1,298 MiB merge peak. Smaller batches make the 500k build fit 1 GiB, but OOM at one million rows under 2 GiB where the default passes. Reduce retained merge inputs/output copies and design a real execution budget, including directory limits and read amplification. Keep the current default; verify larger tiers and sustained maintenance before adoption. |
| 2 | Profile hot ranked OR cursor work, retaining broad cache-pressure controls | The [focused trace](targeted-memory.md) removes the observed low-memory penalty and scales from 286 to 578 QPS with two versus four readers. The long OR remains about 36 ms while scoring only 134 candidates. Attribute cursor/bound work, then prove a candidate change with repeated hot and broad-trace controls. Keep larger-working-set and mixed-write capacity separate. |
| 3 | Close the query-shape coverage gaps | Add fuzzy/regex/proximity, deeply nested Boolean trees, large K/OFFSET and secondary sorts, partitioned ranked queries, and cross-session snapshot/locking scenarios. Keep the routine Lead oracle within its roughly 15-minute budget; use larger same-engine checks for scale. |
| 4 | Sustain wide ranked queries with mixed writes and maintenance | The [bounded concurrency diagnostic](wide-contention.md) and 31/32/33/128-term cursor fuzzer pass. Extend to larger working sets, inserts/deletes/updates and VACUUM, reporting actual writes, read tails and errors. Quiescent exact ranking and live membership checks remain separate because score statistics are not MVCC-frozen. |
| 5 | Revisit filtered top-K continuation | Preserve the fast initial top-K search; activate continuation only after filter shortfall. Prove bounded memory, multi-source correctness and concurrent behavior, and repeat both winning and regressing controls before enabling it. Draft PR #45 and the decoded-document cache prototype are not ready to merge. |
| 6 | Reproduce the published benchmark environment and prepare publication | Use exact prepared data, confirmed query traces and matching hardware/settings. Rerun open-source baselines alongside Stannum; distinguish those measurements from published TIN reference numbers. Preserve disagreements and failures. Do not assume access to a local TIN binary. |
| 7 | Consolidate benchmark entry points | Retire old harnesses only after their distinct correctness, mutation and maintenance coverage is represented in the retained tools. Keep the Lead oracle separate from large-scale throughput tests. |

Already demonstrated: PR #51 fixes ranked EXISTS/NOT EXISTS; all 44 query-shape
cases pass. PR #52 reduces the measured 128-term synthetic OR from about 147 to
57 ms, and a 32-term Wikipedia OR from about 52 to 46 ms. Those are single-client
observations, not a claim of being faster than TIN. See [the paired evidence](grouped-wand.md).
