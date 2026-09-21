# Local 100k snapshot latency comparison

Forced bitmap counting improves the upper tail of this read-only trace but
regresses many small queries. It should not become the universal strategy.

| Build / strategy | p50 across rounds (ms) | p95 across rounds (ms) | p99 across rounds (ms) |
|---|---:|---:|---:|
| Main 750872d | 0.056–0.069 | 1.216–1.389 | 1.816–2.440 |
| Candidate 54c2d9e, default | 0.062–0.067 | 1.337–1.386 | 1.928–2.289 |
| Candidate, forced bitmaps | 0.107–0.138 | 0.800–0.895 | 1.218–1.381 |

These are nearest-rank percentiles over **302 per-query median server execution
times**, giving each published Wikipedia OR query equal weight. They are not
request-weighted production p95 or concurrent-load p95. The range shows three
rounds, not a confidence interval. EXPLAIN ANALYZE instrumentation is included.

The baseline snapshot contains 100,000 documents and is restored separately for
every variant in every round. Each query receives one untimed execution and nine
timed executions, on one persistent connection. Query order is randomized per
round and shared across variants. A three-round Latin-square schedule balances
each variant's first/second/third position. PostgreSQL shared buffers restart;
OS caches are uncontrolled on this shared macOS ARM development machine.
Settings: PostgreSQL 18.6, 256MB shared buffers, 16MB work_mem, JIT/autovacuum off,
forced custom plans, sequential scans discouraged. No mutations occur here.

All 2,718 warmup result checks matched the baseline counts previously verified
against the independent full-corpus token oracle. All 24,462 timed plans used the
custom count path; bitmap trials asserted the forced bitmap strategy. Per-query
medians across rounds identify 99 bitmap regressions relative to candidate
default using a threshold of both 10% and 0.05ms. This supports investigating a
selective policy, but does not validate the earlier density-only rule, which
failed transfer to the larger mutated local corpus. No selector is enabled.

Next experiments: repeat on mutation-heavy/low-visibility snapshots, test larger
working sets and memory limits, and validate any proposed selector on unseen
queries/corpora. Concurrent p95 and capacity belong in a separate load experiment.
AWS full-corpus evidence remains necessary before making performance claims.

[Machine-readable receipt](local-snapshot-latency.json). Raw plans, immutable
harness copy, source manifest and query-level measurements are retained under
`benchmarks/results/local-snapshot-latency-100k/`. Run with
`benchmarks/snapshot_latency_local.py --snapshot-run <validated-native-run>
--queries <queries.json> --output <new-directory>` and the PostgreSQL tools in PATH.
