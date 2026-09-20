# Reusing document ordinals in ranked continuation

Status: experimental, disabled by default; not ready to enable or merge as a production optimization.

The previous frontier repeatedly decoded the frozen document stream for each best-first range. Profiling attributed about 42 ms of a 45 ms filtered OR/AND query to the first document lookup in each range. This experiment decodes document TIDs once into a query-local vector, then binary-searches the first ordinal in a range and advances monotonically within it. The stored index format is unchanged.

## Measurement

Measured source: `4346977dd6993e3b049b77d5806235403aa454bb`. Diagnostic phase timers were removed before this run. Baseline and candidate use the same frozen image with `stannum.experimental_ranked_frontier` off/on. The baseline path is unchanged from main `f641dbe`.

The existing server-times harness executes EXPLAIN ANALYZE with TIMING OFF. Local Docker: four CPUs, 4 GiB RAM, 1 GiB shared buffers, 16 MiB work_mem, JIT off. A fixed 100,000-document Wikipedia fixture is built into **one immutable source** using `build_segment_docs=100000`; ordinary segment sizing produces four sources and does not exercise this prototype. Results are warm-cache, single-client measurements, not a comparison with PlanetScale TIN or a production throughput claim.

Four restarted rounds run off/on/on/off. Each case has nine interleaved observations per round, dropping the first two. Values below average the two retained round medians. These descriptive ratios are not statistical significance estimates.

| Query/filter | Baseline ms | Candidate ms | Candidate / baseline |
|---|---:|---:|---:|
| or_p25_literal | 5.179 | 3.654 | 0.71x |
| common_p25_literal | 2.636 | 0.822 | 0.31x |
| and_p25_literal | 2.745 | 3.017 | 1.10x |
| or_p100_literal | 1.914 | 2.917 | 1.52x |
| common_p100_literal | 0.685 | 0.541 | 0.79x |
| and_p100_literal | 1.222 | 2.393 | 1.96x |
| or_adversarial_literal | 7.029 | 6.575 | 0.94x |
| common_adversarial_literal | 5.397 | 6.018 | 1.12x |
| and_adversarial_literal | 3.967 | 4.950 | 1.25x |
| rare_p25_literal | 0.310 | 0.497 | 1.60x |

All 268 benchmark comparisons passed exact score-bit ordering, membership, uniqueness, and cardinality checks against materialized ranking. Tied result IDs may differ at an unordered tie boundary. The 126 PostgreSQL tests also passed, including active-frontier continuation, deletes/HOT updates, rescans, and frozen scores across inserts. These are existing Stannum correctness oracles, not a new LED compatibility run.

## Limits and next decision

- Document caching uses 1,048,576 allocated bytes for this fixture, in addition to ranges and exact candidates. It scales with all source documents. Loose bounds can buffer almost all matching rows; this is not a bounded-memory top-k implementation.
- The filtered OR case still expands 257 of 261 ranges and peaks at 27,679 buffered rows. Reducing document decoding does not solve loose bounds or repeated posting seeks.
- Rare terms and all-pass OR/AND expose the cost of starting the frontier eagerly. Preserve the ordinary fast top-k prefix; investigate continuation only after that prefix fails the SQL filter. AND should retain its efficient intersection path unless a separate measured win justifies changing it.
- Replace the full document vector with a bounded seek/checkpoint structure or an explicit memory-limited fallback before broad rollout. Multi-source support remains unproven.
- Concurrent reader/writer testing follows a strategy that wins end to end. Mutation tests must verify that the frontier actually ran, since extra sources currently force fallback.

Raw runs are retained under `benchmarks/results/frontier-document-reuse-clean-matrix`, including source/image manifests, SQL, fixture hash, per-observation plans, and correctness records. The diagnostic probe and instrumented 67-case run are retained separately. The JSON companion contains every case and both round medians.

Evidence archive: `benchmarks/results/frontier-document-reuse-evidence.tar.gz` (2,413,111 bytes), SHA-256 `e0d8f5b15c11877d8a06b29a95d902508872bc4b61b6006380c60d8f5ca53e87`.
