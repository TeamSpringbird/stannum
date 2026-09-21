# Local experiment comparison

Single-client server timings. Percentiles are over 302 per-query medians; not production request p95. Three balanced rounds per completed experiment. No cross-machine speedup claims.

| Documents / state | Variant | p50 ms | p95 ms (range) | p99 ms | Regressed queries vs main |
|---|---|---:|---:|---:|---:|
| 100,000 / clean | main | 0.066 | 1.295 (1.216–1.389) | 1.900 | 0 |
| 100,000 / clean | candidate-default | 0.067 | 1.375 (1.337–1.386) | 2.035 | 10 |
| 100,000 / clean | candidate-bitmaps | 0.110 | 0.803 (0.800–0.895) | 1.237 | 103 |
| 1,000,000 / clean | main | 0.803 | 13.876 (13.277–14.137) | 20.274 | 0 |
| 1,000,000 / clean | candidate-default | 0.827 | 13.900 (13.828–14.312) | 23.296 | 15 |
| 1,000,000 / clean | candidate-bitmaps | 1.820 | 9.598 (9.493–9.657) | 14.313 | 252 |

Pending/failed: benchmarks/results/local-million-campaign/latency-mutated (running); no aggregate published.


Regression threshold: median across rounds is slower by both 10% and 0.05 ms. Count default and forced paths separately; forced bitmaps are not an enabled selector.

Use the inventory to select workloads: ranked frontier → filtered top-K; spill output → read/write contention and memory; recovery/test PRs → correctness gates; decoding → microbenchmarks.
