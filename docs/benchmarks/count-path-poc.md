# Count-path POC: first measured result

This is a local ARM64 codec/execution experiment, not a PostgreSQL p95 or TIN comparison.
The AWS measured build is unchanged. Nine alternating-order repetitions were run
at 12,000 and 36,000 synthetic heap pages with nine encoded sources. Construction,
BTreeSet reference generation, exact-ID validation and strategy selection are untimed.
Results include cursor construction, decoding, union and final count/page grouping.

| Shape (36,000 pages) | Existing selected path | Median ms | Best tested path | Median ms | Relative gain |
|---|---|---:|---|---:|---:|
| rare-short | materialize-sort | 0.118 | materialize-sort | 0.118 | 1.00x |
| sparse-wide | materialize-sort | 3.914 | stream-heap-terms | 3.071 | 1.27x |
| common-short | materialize-sort | 3.981 | page-bitmaps | 3.096 | 1.29x |
| common-wide | materialize-sort | 56.939 | page-bitmaps | 26.247 | 2.17x |
| dense-wide | page-bitmaps | 12.726 | page-bitmaps | 12.726 | 1.00x |
| overlapping-segments | materialize-sort | 17.900 | page-bitmaps | 7.979 | 2.24x |

## Decision from this experiment

- Do not replace materialization with heap-streaming unconditionally. It usually
  loses here, despite avoiding the complete result vector.
- The existing heuristic chooses scalar execution for common-wide and overlapping
  cases where page counting wins by about 2.2x. This is the strongest next target.
- Dense-wide already selects page bitmaps: its large gain over forced scalar is
  not an available improvement over current production behavior.
- Heap-based term selection helps sparse-wide, but loses badly for common/dense
  unions. Consider it only with an appropriate cost model.
- Rare-short benefits from retaining the simple scalar path.

## Validation and limitations

Every strategy is checked against the exact sorted IDs from an independently
constructed BTreeSet before timing, including duplicated TIDs across sources.
Timed iterations check both tuple count and populated-page count. Additional checks
cover empty inputs, duplicate maximum valid heap offsets/blocks, and heap-cursor seeks.
Fixture generation checks achieved term frequency; an initial biased generator was
rejected and none of its timings are used in this report.

The source data are synthetic, all entries live, and there are no PostgreSQL
visibility checks, rechecks, writes, scoring payloads or buffer-manager costs.
The workstation is shared, so inspect min/max dispersion in the CSVs. The temporary
heap selector is confined to the example, not a production cursor. No new on-disk
format or explicit SIMD kernel is introduced.

## Next controlled experiment

Capture plans and a CPU profile for actual slow Wikipedia queries after measured
traffic. Compare default selection against forced page execution in an isolated
build at the same concurrency. Confirm the strategy currently used by the slowest
queries before tuning selection; do not extrapolate this synthetic gain to the
whole workload. Preserve the scalar path and require sparse-query regression checks.

Reproduce: `cargo run -p segment --release --example count_paths -- 36000 9`.
See [12,000-page measurements](count-path-poc/pages-12000.csv),
[36,000-page measurements](count-path-poc/pages-36000.csv), and
[provenance](count-path-poc/provenance.json).

PlanetScale methodology clarification relayed by the user: none of their tests
used more than eight clients, except the update workload with nine. This establishes
a concurrency ceiling, not an exact per-workload count. Our eight-client Stannum
run reached 253.39 QPS and averaged 7.90 CPU cores; client underutilization no longer
explains most of the remaining difference from published TIN.
