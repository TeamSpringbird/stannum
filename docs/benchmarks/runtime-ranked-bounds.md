# Runtime bounds in generic ranked plans

PR #36 extends the parameter binding in #32: a simple generic ranked query can
bind bigint LIMIT/OFFSET parameters when execution starts and use the existing
block-max top-k path. On the controlled 100k Wikipedia fixture, the final change
reduces broad disjunction latency by about 46% for LIMIT 10 and 33% for LIMIT 100
OFFSET 20. It does not change the storage format or introduce a new scoring algorithm.

## Measurement

Baseline: `7d441c14fe0000b5d7ab62cb48637b0259911015`.
Final candidate: `ebe84b038b891f228ad3733bf73179db2fdaf6e7`.
Both are native ARM Docker images built with the same pinned PostgreSQL/runtime
recipe. The test uses four CPUs, 4 GiB memory, 1 GiB shared buffers, 16 MiB work_mem,
512 MiB maintenance_work_mem, JIT disabled, and forced generic plans.

Each campaign builds the complete 100,000-row Wikipedia fixture once with the
baseline and reuses that volume across clean container restarts in A/B/B/A order.
The full ID range is 1–100,000; `id <= 25000` selects exactly 25,000 documents before
text filtering. The heap has 10,496 pages. Actual visibility-map measurements
before and after every measured round stayed at **10,467 all-visible pages and
zero all-frozen pages**; the heap is not entirely all-visible.

Each round runs 15 prepared statement variants: literal bounds, runtime bounds,
and an unsupported `$2 + 0` bound expression across five shapes. Nine executions
per case alternate forward/reverse case order; the first two per case are discarded.
Reported times are server-side `EXPLAIN ANALYZE` execution times, with normal node
timing instrumentation. They are warm single-session measurements, not throughput,
cold-cache measurements, or an end-to-end network comparison.

| Runtime-bound shape | Baseline round medians (ms) | Final candidate round medians (ms) | Interpretation |
| --- | --- | --- | --- |
| `history OR war`, LIMIT 10 | 3.388 / 3.525 | 1.821 / 1.907 | About 46% lower |
| Same, LIMIT 100 OFFSET 20 | 3.297 / 3.630 | 2.287 / 2.336 | About 33% lower |
| Same, `id <= 25000`, LIMIT 10 | 3.231 / 3.581 | 3.212 / 3.655 | Approximately unchanged; pair ratios 0.994 / 1.021 |
| `"united states"`, LIMIT 10 | 3.603 / 3.747 | 3.399 / 3.592 | Small difference, about 5%; not a strong optimization claim |
| `quasar`, LIMIT 10 | 0.285 / 0.321 | 0.232 / 0.265 | Tiny absolute difference; treat cautiously |

The broad OR runtime plan acquires `Top K: 10` and block-max pruning, reporting
570 scored candidates and ten heap fetches. The unbounded baseline reports 27,946
candidates. Phrase queries may still enumerate their candidates because the scorer
cannot use the same bound everywhere. Every full plan remains available in raw
artifacts; representative plans and all round medians are in the checked-in
[result JSON](runtime-ranked-results.json).

## A regression found and removed

The first implementation (`076a05f`) bound filtered queries too. That improved
unfiltered OR latency by about 44%, but made the filtered OR case about **54% slower**:
3.443 / 3.339 ms became 5.247 / 5.167 ms. The residual filter rejected enough early
candidates to require exhaustive completion after the initial top-k pass.

The final candidate conservatively leaves generic ranked scans with residual SQL
quals unbounded. This removes that regression in the repeated measurements above.
Existing literal-limit behavior is unchanged. Filter-aware candidate selection is
a separate next step; copying the literal path indiscriminately would have made
some applications slower.

## Correctness and scope

Both campaigns passed all 60 case checks: exact ordered float32 score bits match
an exhaustive MATERIALIZED scoring reference, each returned ID has its expected
score, and no duplicate IDs appear. Boundary ties may choose different IDs with
the same score. This validates the optimization against existing Stannum execution;
upstream Lead remains the independent semantics oracle.

The original candidate passed the full ARM/x86 PostgreSQL 17/18 CI matrix and the
Lead reference gate. Tests exercise changing prepared parameters, constant query
text, NULL/zero/negative bounds, addition overflow, true nested-loop rescans,
WITH TIES, selective-filter fallback, and SRF/aggregate exclusion. The final gate
updates the filtered-query test to require no runtime bound; its CI must pass on
the final PR head before merging.

Runtime binding accepts only bigint constants/external parameters in a simple
ranked SELECT with no residual quals. Joins, aggregates, DISTINCT, window functions,
set-returning targets, and arbitrary bound expressions retain existing behavior.
NULL LIMIT is unbounded; NULL OFFSET means zero; overflow disables the hint.
PostgreSQL's Limit node still owns SQL error handling. The hint never truncates
results: visibility checks and WITH TIES can exhaust the first candidates and
request complete ranking. Rescans rebind the parameters and discard old ranking state.

## Reproduction and evidence

Use the existing benchmark image builder for each frozen source revision and the
same pinned recipe. Load the canonical `wikipedia-100000/documents.csv` into
`documents(id bigint PRIMARY KEY, body text NOT NULL)`, create its Stannum index,
and `VACUUM ANALYZE` once. Start each image sequentially on that baseline-built
volume with the settings above; do not write during measurement.

The existing server-time collector now accepts prepared SQL cases and interleaved
repetitions. Connection settings come from normal libpq environment variables:

```sh
python3 benchmarks/server_times.py --engine stannum \
  --sql-cases docs/benchmarks/runtime-ranked-cases.json --interleave \
  --repetitions 9 --discard 2 --label baseline --output /path/to/round
```

Repeat A/B/B/A, preserving each round separately. The report records setup SQL,
execution order, all case SQL, raw plans, and summaries. The local raw artifacts
also retain the exact orchestration source, immutable image identities, fixture
settings, visibility measurements, and correctness outcomes:

- `benchmarks/results/runtime-ranked-bounds/`: first complete campaign.
- `benchmarks/results/runtime-ranked-bounds-gated/`: final complete campaign.
- `benchmarks/results/runtime-ranked-bounds-initial-reference-failure/`: an earlier
  attempt whose validator failed because its score subquery was inlined into JSON
  aggregation. Its timings were excluded; the successful validator materializes
  scoring before aggregation.

Both complete campaigns removed their temporary containers and volumes. No remote
TIN database or cloud resources were used.

The local evidence archive is
`benchmarks/results/runtime-ranked-evidence-20260919.tar.gz` (153,875 bytes), SHA-256
`d068d5af720f2ebbcde301f8c3d30998d2281f98d894cc896201ae868e0729f3`.
