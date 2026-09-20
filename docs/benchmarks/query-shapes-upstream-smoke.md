This is a 1,000-document integration smoke test: one client, 2 seconds warmup,
10 seconds measurement per engine, mixed count queries, no writes. It is not
a capacity comparison. Both engines exercised all 906 trace forms. One count
disagreement remains explicitly reported below.

# Published-trace local run

Diagnostic measurements, not a capacity claim. Each engine uses its native query semantics.
GIN ranks with ts_rank_cd; Stannum ranks with BM25. Phrase membership can also differ.
Percentiles use nearest rank; p99 is omitted below 1,000 samples. No speedup ratio is inferred.

| Engine | QPS | p95 ms | p99 ms | Measured query forms | Index MiB | Total relation MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| stannum | 5716.8 | 0.229 | 0.409 | 906 | 3.00 | 5.88 |
| postgres | 6795.1 | 0.202 | 0.380 | 906 | 4.03 | 12.49 |

See comparison.json for query-family and individual-query distributions,
semantic differences on the validation sample, and completed updates.
Index read/hit bytes are block accesses, not physical disk traffic.
Resource summaries in comparison.json and resource-summary.json use samples wholly inside the measured window; boundary gaps are reported.
The full pinned trace may not be traversed during short or slow runs.

Workload-state snapshots (after VACUUM / before driver / after restart):
- stannum after_vacuum: 105/105 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.
- stannum before_driver: 105/105 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.
- stannum after_restart: 105/105 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.
- postgres after_vacuum: 122/128 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.
- postgres before_driver: 122/128 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.
- postgres after_restart: 122/128 heap pages all-visible; 0 estimated dead tuples; 0 autovacuums.

Before-driver capture precedes upstream warmup. After-restart capture follows container shutdown/restart and is not an exact end-of-traffic snapshot. These observations do not establish cold-cache or pristine-heap conditions.

Full-corpus count disagreements: 1 checked forms, 1 in the timed query mix. See manifest.json.
