# Stannum / TIN on comparable AWS configurations

Measured 2026-09-20 UTC. This replaces the historical cross-machine graphic as the
useful comparison for these query shapes. It does **not** establish an overall
engine ranking or claim identical hardware.

## Conclusions

* Stannum is in the same order of magnitude as TIN for the tested warm ranked
  queries. TIN is materially faster on common-term counts and several filtered
  ranking cases. Results vary by query shape; there is no universal winner.
* The old filtered-OR comparison hit a TIN planner sensitivity: adding `OFFSET 0`
  disables its observed top-k plan. On the same new TIN fixture, the otherwise
  identical query changes from 3.217 ms to 30.141 ms. Stannum stays near 9.8 ms.
  The old 29.338 ms observation was server execution time, not network latency.
* Stannum beats the same-host GIN baseline for the five matching count shapes;
  the miss case is essentially tied at tens of microseconds. TIN's same-host
  advantage is especially large on common-term counts.
* GIN is faster on EC2 than on PlanetScale even with matching physical GIN layouts
  and observed plan shapes. Cross-machine differences remain, and these controls
  must not be used as a universal normalization multiplier.

## Configuration and validation

EC2: r8g.large in us-east-1, verified AWS Graviton4 / Neoverse-V2, two physical
cores, 16 GiB advertised memory, encrypted 100 GiB gp3 at 3,000 IOPS / 125 MiB/s.
PlanetScale: requested PS-160 ARM / EBS in us-east-1 (2 vCPU / 16 GB); exact CPU
model and actual allocated storage were not independently exposed by SQL.

Both servers: PostgreSQL 18.6, 2 GiB shared_buffers, 26 MiB work_mem, 8 GiB
effective_cache_size, random_page_cost=1.1, JIT off, max_parallel_workers=4,
max_parallel_workers_per_gather=2, max_worker_processes=4, worker I/O,
effective_io_concurrency=32, and LZ4 TOAST compression. Maintenance memory was
706560 kB. The benchmark uses explicit simple text configuration for GIN.
PlanetScale's Debian 12 / GCC 12 build differs from EC2's Debian 13 / GCC 14 build;
CPU generation, managed-service overhead and OS details remain uncontrolled.

Stannum source: `ab1e6db87e7c5bafbfc5c121ac66d879d48bbd3b`, clean release build,
experimental frontier disabled. TIN reports version 1.0.2; that extension version
alone is not a binary build identifier. Neither engine's index layout was forced
to match the other's implementation: normal default index builds were used.

Data: identical verified 100,000-row Wikipedia CSV, SHA-256
`d01f490c802313190924a1edd1722d9b1c543793a8ccd0950108d813cb8b7da1`.
Both search tables use bigint IDs and the same text. Both GIN tables store the
same generated simple tsvector. VACUUM ANALYZE preceded measurements. Each GIN
heap has 12,434 pages and each GIN index 14,008 pages on both hosts. The native
search heaps use 9,849 populated pages; EC2 additionally has seven empty trailing
pages (maximum live CTID block 9,848). The Stannum index has 20,232 pages; TIN 21,552.

All 54 per-host validation checks passed: exact membership against an independently
computed corpus oracle for six count queries on both index types, and 15 ranked
cases per engine against unlimited scored results filtered/sorted in Python.
Across five scored query shapes, **55,741 document-score pairs match TIN/Stannum
bit-for-bit**, with identical membership. Ranked validation handles score ties by
checking the top score sequence and valid distinct document IDs. This is evidence
for these shapes, not full TINQL compatibility or the separate Lead oracle suite.

Timing: one client per server, literal queries, three rounds of 25 executions per
case, first five discarded in each round. Thus each table value below is the
median of 60 retained samples. Query order reverses between repetitions. All
4,050 primary raw EXPLAIN plans are retained. Every retained primary plan reports
zero shared reads: these are warm-buffer results, not disk/cold-cache benchmarks.
No builds, data loads or verification queries overlapped a host's measured runs.

Both collectors use `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, TIMING OFF, FORMAT JSON)`.
Execution Time is measured on the server; planning time and client elapsed time
are stored separately. No network subtraction is used. This instrumentation
does not time result delivery/serialization as a normal SELECT would. The runs
were sequential across hosts, so time-of-day/noisy-neighbor variation is not fully
controlled. No concurrent-throughput or replicated-write comparison was performed.

## Count queries

Milliseconds, lower is better. GIN is a same-host control in each pair.

| Query | Stannum / EC2 | GIN / EC2 | TIN / PlanetScale | GIN / PlanetScale |
|---|---:|---:|---:|---:|
| history | 0.526 | 12.008 | 0.170 | 18.604 |
| quasar | 0.038 | 0.051 | 0.164 | 0.111 |
| history AND war | 0.774 | 5.418 | 0.479 | 7.387 |
| history OR war | 1.140 | 14.196 | 0.342 | 22.212 |
| telescope OR astronomy | 0.071 | 0.536 | 0.276 | 0.698 |
| absent term | 0.031 | 0.029 | 0.126 | 0.088 |

The substantial GIN controls (`history`, AND, OR, selective OR) are about
1.30–1.57x slower on PlanetScale. Tiny rare/miss queries show different ratios.
That variation is why we report the raw controls rather than rescaling results.

## Ranked top ten

Identical `full_score(ctid)` projection, `ORDER BY score DESC LIMIT 10`, **without
OFFSET**. Filter percentages refer to ID ranges, not a measured term-selectivity
percentage. All table values are server milliseconds.

| Query | ID filter | Stannum / EC2 | TIN / PlanetScale |
|---|---|---:|---:|
| history | none | 1.103 | 1.252 |
| history | id <= 25000 | 4.881 | 2.438 |
| history | id <= 1000 | 2.128 | 1.431 |
| quasar | none | 0.477 | 0.398 |
| quasar | id <= 25000 | 0.472 | 0.415 |
| quasar | id <= 1000 | 0.475 | 0.412 |
| history AND war | none | 2.080 | 2.494 |
| history AND war | id <= 25000 | 5.007 | 3.031 |
| history AND war | id <= 1000 | 1.896 | 1.355 |
| history OR war | none | 3.553 | 2.463 |
| history OR war | id <= 25000 | 9.844 | 3.246 |
| history OR war | id <= 1000 | 3.379 | 2.369 |
| telescope OR astronomy | none | 0.695 | 0.719 |
| telescope OR astronomy | id <= 25000 | 1.452 | 1.015 |
| telescope OR astronomy | id <= 1000 | 0.284 | 1.159 |

## OFFSET 0 reproducer

The historical filtered query used `OFFSET 0`. This additional paired experiment
alternates equivalent queries on the same fixture/session: 25 repetitions per
form, five warmups discarded, 20 retained per form. Returned ID/score sets agreed.

```sql
SELECT id, tin.full_score(ctid) AS score
FROM stannum_aws_20260920.docs
WHERE body ==> 'history OR war' AND id <= 25000
ORDER BY score DESC LIMIT 10;          -- fast top-k plan
-- Add OFFSET 0 before the semicolon for the slower observed plan.
```

| Engine / host | No OFFSET | OFFSET 0 |
|---|---:|---:|
| tin | 3.216 | 30.141 |
| stannum | 9.820 | 9.808 |

TIN without OFFSET uses Projector nodes above a Text Search Scan with Top K=10
and Candidate Filter=Parent Projector. With OFFSET 0 it instead uses a Sort above
Projector / Conjunction Scan / Text Search Scan / Index TID Probe. The historical
slow observation used that same structural family. This controlled reproduction
explains the direction and scale of the old anomaly; it does not prove every past
millisecond was caused solely by OFFSET or identify TIN's internal implementation.

Separate alternating 2 MiB / 26 MiB work_mem probes retained ten samples per value:
without OFFSET, 3.228 / 3.202 ms; with OFFSET 0, 30.141 / 30.045 ms. Work memory alone
did not explain the plan/timing gap. Do not present the slow OFFSET form as TIN's
general ranking performance.

## Next optimization justified by evidence

For Stannum's filtered OR25 case, the representative plan reports 548 initially
scored candidates, then **27,946 Exhaustive Score Calls**, despite only **34 Heap
Fetches**. Eliminating the exhaustive restart remains a concrete target. The
failed experimental resumable frontier must not be promoted just because it
reduces score calls: its prior measurements found repeated first document lookup
cost overwhelming the saved work. Improve/reuse that lookup state, then rerun this
exact end-to-end case with an unchanged baseline and the correctness gates.

Common-term count execution is another measurable gap to TIN. Keep rare/miss and
selective-OR cases in the regression matrix; their behavior differs from the
common-term workloads. Do not optimize Stannum around exploiting TIN's OFFSET 0
plan sensitivity.

## Evidence and reproduction

[Infrastructure and protocol](aws-comparable.md),
[all summarized cases and per-round medians](aws-comparable-results.json), and
[representative paired OFFSET plans and SQL](aws-comparable-plan-examples.json).
Raw plans, SQL, timing samples, correctness rows/score bits, source/image provenance,
configuration and the one-off orchestration scripts are retained locally under
`benchmarks/results/aws-comparable`. The orchestration reuses the existing
`tin_experiments.Experiment.probe` collector; it is not a replacement load harness.
The build/provisioning change was merged as PR #47. Raw evidence archive and cleanup
verification are recorded below after export.

Verified evidence archive: `benchmarks/results/aws-comparable-evidence.tar.gz`,
2,836,675 bytes, 8,449 verified files, SHA-256
`76d364ec3a5d4bddf3f86e674a95a47527f45c8a30c647b98c1b5df3dd045f63`.
The archive is retained locally, not uploaded to the public repository.
