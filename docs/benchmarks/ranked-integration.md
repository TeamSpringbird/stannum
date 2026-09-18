# Ranked-scan fix and round-three integration

## Reproduction and fix

The untouched `45d2696` release failed the Wikipedia mutation correctness check after 35 seconds: the rare-term top ten ended with score 12.357153 after score 11.649617. The original saved failure had the same inversion for an AND query. Artifacts: `/tmp/stannum-ranked-repro/`.

The write buffer is shared by retained readers and incremental refreshes. Document cursors retain an encoded TID ordering, but mutable-index length lookup used the current ordering. Adding a lower TID in reused heap space shifted ordinals, so completing a pruned ranked scan could score a retained document with another document’s length. Returning the retained encoded length array fixes that mismatch without changing the on-disk format.

The segment regression failed before the fix (length 1 instead of 4) and passes after it. A PostgreSQL regression updates an earlier heap page, refreshes the buffer, and verifies the retained scorer’s score bits are unchanged. The full original workload subsequently passed with custom scans enabled.

## Combined validation

- 427 non-extension Rust tests and 44 Python tests passed.
- 88 PostgreSQL 18 tests passed; PostgreSQL 17 and 18 Clippy passed with warnings denied.
- Release schema drift and baseline snapshot installation passed (0 retained predecessor upgrade paths).
- Lifecycle passed 320 standby snapshot checks, 39 index verifications and 13 concurrent reader checks, including crash recovery, temporary/unlogged indexes, real parallel workers and promotion.
- Deferred-merge lifecycle passed with default AUTO autovacuum and no preload.
- Lead reference oracle: 235/235 query/state comparisons passed, including HTML/ANSI highlighting; no differences.

Integration retains all five workstreams and their tests. Direct calls to internal indexed scoring now explicitly reject recovery-origin snapshots, including after promotion; ordinary planner-routed standby scoring uses the heap fallback. The lifecycle also verifies recovery via savepoint and a fresh primary snapshot.

## Mutation measurements

Each run uses 100,000 Wikipedia documents, 600 seconds of traffic, 30 seconds warmup, 50 scheduled mutations/second, two readers, 60-second VACUUM intervals and 10-second correctness-check scheduling. Custom scans remain enabled. Stress sets `write_buffer_docs=256` and `merge_tier_factor=4`. The baseline includes only the ranking fix; integrated runs include all five workstreams. There is no matching custom-scan-enabled stress baseline, so the stress run is a correctness/stability check, not a before/after comparison.

Writer latencies below exclude pgbench scheduling lag. Values are milliseconds; each cell is p99 / maximum over the full run, including drain. Runs share a local machine, so small timing changes are not statistically established.

| Run | Insert p99 / max | Delete p99 / max | Update p99 / max |
| --- | ---: | ---: | ---: |
| Baseline + ranking fix | 6.986 / 306.002 | 5.745 / 16.418 | 7.504 / 2722.168 |
| Integrated defaults | 6.704 / 146.579 | 5.138 / 13.675 | 7.581 / 112.967 |
| Integrated stress | 8.506 / 312.128 | 5.610 / 17.174 | 6.982 / 173.311 |

| Run | Count p99 | Ranked p99 | Reads/s | Segment range / final | Periodic check rounds |
| --- | ---: | ---: | ---: | ---: | ---: |
| Baseline + ranking fix | 3.559 | 6.744 | 1772.4 | 4–10 / 6 | 47 |
| Integrated defaults | 3.704 | 7.045 | 1643.2 | 4–20 / 14 | 48 |
| Integrated stress | 3.639 | 6.881 | 1690.1 | 4–18 / 10 | 48 |

At default settings, the worst observed write fell from 2,722 ms to 147 ms.
Reader p99 increased 4.1% for counts and 4.5% for ranked queries, and reader
throughput decreased 7.3% in this single comparison. These costs accompany
the reduction in rare write stalls; the measurements do not establish a
universal latency bound or a statistically significant reader regression.

All runs also pass the final post-traffic check. Those checks verify match sets against an independent regex scan and ranked output length, uniqueness, membership, finiteness and descending order. They do not constitute an exhaustive top-k oracle under every concurrent interleaving. Separate PostgreSQL tests compare pruned and full-scoring output bit for bit.

| Run | Artifact SHA256 |
| --- | --- |
| Baseline + ranking fix | `d4236287e105cf01533ab2e652e1c612a3881eab77056db648eb8acea97cf39b` |
| Integrated defaults | `d4ff30e905644a3e6c433590911c14f8e3907463e4f82e272cd81d713d96ca91` |
| Integrated stress | `d4ff30e905644a3e6c433590911c14f8e3907463e4f82e272cd81d713d96ca91` |

Raw manifests, per-query plans, ten-second reader timelines, segment samples, logs and correctness observations remain under `/tmp/stannum-ranked-fixed/`, `/tmp/stannum-integrated-default/`, and `/tmp/stannum-integrated-stress/`. Combined validation logs are `/tmp/stannum-integrated-*.log`; the reference report is `/tmp/stannum-integrated-oracle/oracle.json`.

The merge document budget still allows the documented emergency directory-overflow exception; it is not a universal wall-clock latency bound. Selective standby scans and DSM-based partial scans remain deferred for the reasons in `../architecture/recovery-and-parallel.md`.
