# Performance snapshot: GIN and remote TIN

The same-host count observations show lower p95 for Stannum than stored-vector
GIN on the measured historical build. The TIN panel compares two strategies
**within TIN on one remote server**. There is no shared Stannum/TIN timing axis:
the earlier version's shared axis invited an unsupported engine ranking.
Server-only timing already excludes network, but hardware, settings and query
execution differ. These observations cannot establish that Stannum is faster
than TIN, performance parity, or current-head performance.

![Recorded Stannum, GIN and TIN observations](performance-snapshot.svg)

Download the [PNG](performance-snapshot.png) for sharing or [SVG](performance-snapshot.svg)
for scalable export. The graphic carries its limitations so a detached image
still identifies the different metrics, hardware and historical builds.

## A. Same-host count latency

The [published-trace results](published-trace-results.md) and
[source JSON](published-trace-results.json), run `tin-count-100k-02`, supply:

| Query family | Stannum client p95 | Postgres stored-vector GIN client p95 |
| --- | ---: | ---: |
| AND counts | 0.841 ms | 1.412 ms |
| OR counts | 2.610 ms | 14.551 ms |

This is the second integration observation, with GIN measured first. Both
engines used PostgreSQL 18.6, native ARM64 Docker, a four-CPU / 4-GiB server limit,
1 GiB shared buffers, 100,000 normalized Wikipedia articles, two clients,
ten seconds of warmup and sixty seconds of measurement. Other local services
remained running. The server CPU limit is not a hardware equivalence claim.
Client and Docker host CPU models are not recorded in this compact evidence.

There were 302 base queries, each executed as AND, OR and phrase forms. These
are family p95 values from the mixed workload, not dedicated AND/OR-only runs.
All 604 AND/OR full-corpus counts agreed; equal counts do not establish equal
membership on unsampled rows. Eleven phrase full counts differed, so phrase
latencies and aggregate mixed-workload throughput are intentionally omitted.
GIN uses stored tsvectors; total relation storage therefore differs. This is a
specific PostgreSQL FTS configuration, not every possible GIN deployment.

The measurement predates later planner improvements. Frozen source SHA-256:
`04d27d986bb5291d0c84276beee645006753df6666aea9ffe808fdfb0fb54985`.
Image and driver identities remain in the source JSON. A fresh paired run on
current main is required before describing these as current release numbers.

## B. Remote TIN: same-server strategy observations

The selected query returns IDs and scores for `history OR war`, restricted to
`id <= 25000`, ordered by descending score with literal LIMIT 10 and OFFSET 0.
The filter selects 25% of the corpus before text matching. Both datasets have
100,000 rows and the same `documents.csv` SHA-256:
`d01f490c802313190924a1edd1722d9b1c543793a8ccd0950108d813cb8b7da1`.

| Observation | Server execution time | Sampling |
| --- | ---: | --- |
| TIN 1.0.2 automatic | 29.338 ms | Three observations; 29.006–30.750 ms |
| TIN 1.0.2 forced TID pushdown | 18.700 ms | Three observations; 18.669–18.785 ms |

For audit continuity only, the removed local marker was 5.178 ms, from
`or_p25_literal` in
[filtered-prefix-results.json](filtered-prefix-results.json). It is the baseline,
**not the rejected retry candidate**. TIN comes from run `visible-s4-m16` in
[tin-expanded-results.json](tin-expanded-results.json), groups
`forced:wiki:0.25:10:r*:{}` and
`forced:wiki:0.25:10:r*:{'tin.debug_force_conjunction_mode': 'Pushdown'}`.
Raw TIN automatic SQL/plans are artifacts `00294`, `00303`, `00309`; pushdown
artifacts are `00293`, `00300`, `00308` under the retained local directory
`benchmarks/results/tin-visible-s4-m16`. The dataset hash was also verified
against that directory's `experiment.json` manifest. No live service was queried
for this graphic.

| Condition | Stannum local baseline | Remote TIN |
| --- | --- | --- |
| Server | Native ARM64 Docker, four-CPU / 4-GiB limit | PS-160 ARM, EBS, 2 vCPU / 16 GiB (user-reported) |
| PostgreSQL | 18.6 | 18.6 |
| Shared buffers / work_mem | 1 GiB / 16 MiB | 2 GiB / 2 MiB for these probes |
| Planning | Forced generic prepared text parameter; literal bound | Literal SQL |
| Timing | EXPLAIN ANALYZE with node timing | EXPLAIN ANALYZE, TIMING OFF |
| Visibility | 10,467 / 10,496 heap pages all-visible | 9,801 / 9,856 heap pages all-visible |
| Complete relation | 362,110,976 bytes | 392,290,304 bytes |
| Layout | Baseline build; segment target 32,768 documents | Four immutable segments, 16 MiB build memory |

The normalized corpus's bodies before its mutable suffix have median 1,296 bytes,
p95 10,121 bytes and maximum 58,221 bytes, per the dataset manifest. Physical
layout differs despite identical input bytes. The TIN disk configuration was
10–4,096 GiB; this is a provisioning range, not observed disk use.

Both timings exclude client network transfer and are warm observations; neither
establishes cold-storage behavior. They differ in CPU resources, memory,
PostgreSQL settings, planning, instrumentation and physical layout. No
cross-engine score or relevance equivalence is established here. Stannum is
checked against exhaustive same-engine scoring in that campaign; TIN's forced
strategies are checked against its own score sequences. Lead remains the
independent compatibility oracle.

The remote panel answers which of these two TIN strategies was faster for
this shape on that server. It does not rank Stannum against TIN. For broader
remote observations, including one million articles, see
[the expanded experiment report](tin-expanded-experiments.md).

## Server-only audit and a better comparison

The six remote raw JSON files above contain these `Execution Time` values:
automatic **29.338, 29.006, 30.750 ms**; pushdown **18.785, 18.669,
18.700 ms**. Their SQL uses `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, FORMAT JSON,
TIMING OFF)`, with no `SERIALIZE`. All six call `tin.full_score(ctid)` and use the
literal query/filter/bound described above. Only the pushdown observations set
`tin.debug_force_conjunction_mode='Pushdown'`; all use 2 MiB work_mem.

The local raw `plans.json` files in
`benchmarks/results/filtered-prefix-gated-100k/r01-baseline` and `r04-baseline`
contain retained execution times **4.951, 5.352, 5.246, 5.036, 5.415, 5.065,
5.100 ms** and **5.691, 5.286, 5.769, 4.900, 5.256, 5.194, 5.199 ms**.
Their medians reproduce 5.100 and 5.256 ms. `cases-baseline.json` records the
forced-generic prepared `stannum.full_score(ctid)` SQL. Local IDs are bigint;
TIN fixture IDs are integer. That contributes to different row widths and
physical layout. Both scoring calls request their engine's full score; there
is no cross-engine score-equivalence check in this experiment.

The plotted TIN times, and the removed local point, were **already server-only
execution times**, not remote client round trips. Planning time is separate.
`TIMING OFF` disables node-level clocks while PostgreSQL still measures overall
execution. `SERIALIZE` can measure output conversion, but EXPLAIN does not send
the query's result rows across the network. See
[PostgreSQL EXPLAIN documentation](https://www.postgresql.org/docs/18/sql-explain.html).
Thus subtracting network latency would not change these measurements.

For a concrete example, TIN artifact `00294` records **29.338 ms execution**,
**0.610 ms planning** and **62.407 ms client elapsed** in `experiment.json`.
Pushdown artifact `00293` records **18.785 / 1.260 / 51.827 ms**, respectively.
The client/server difference includes planning, EXPLAIN output generation and
transfer, client overhead and network; it is not an isolated network estimate.
The figure uses the execution field only.

A search of the retained remote SQL artifacts and both TIN collector sources
found no GIN/tsvector benchmark measurements to use as a same-host anchor.
A new remote session could answer a better-defined question:

1. On the remote server, load the pinned corpus once and compare TIN with a
   stored-vector GIN baseline. Locally, compare the current Stannum build with
   the same GIN baseline. Pin PostgreSQL version, text configuration, input
   column types, normalization and query semantics as closely as possible.
2. Start with AND/OR count queries whose full counts **and membership** agree.
   Use identical literal/custom/generic plan modes as separate experiments,
   matched `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, TIMING OFF, FORMAT JSON)`,
   identical projection and ID types, explicit VACUUM, before/after visibility
   snapshots, a documented warmup,
   interleaved repeated runs and retained raw SQL/plans. Time output conversion
   separately with `SERIALIZE TEXT` if that matters to the application. Store
   `Execution Time`, `Planning Time` and client elapsed as separate fields;
   never silently substitute a client measurement for a server timer.
3. Compare each search engine with GIN **on its own host**, reporting per-query
   distributions, plan choices, CPU/memory/storage and index/heap sizes.
   Ratios relative to GIN are a useful reference, not a hardware correction:
   different engines can scale differently with CPU, caches, I/O and parallelism.
4. Keep BM25 ranked queries separate. GIN plus `ts_rank_cd` is not an identical
   scorer, so ranking ratios would conflate execution and scoring algorithms.
   A direct Stannum/TIN ranking comparison still needs matched scoring/query
   semantics and a matched environment; otherwise publish separate observations.

This protocol makes a temporary remote session useful without presenting an
uncontrolled raw latency ratio as an engine speedup. It has not been run.
Prepare and validate the collector locally before provisioning another paid
database; the recorded timing audit does not require recreating one.

## Re-render

The generator reads existing checked-in source JSON directly; it does not embed
copied timing constants or run benchmarks. Use Python with Matplotlib 3.11.2:

```sh
python3 docs/benchmarks/render_performance_snapshot.py
```

It writes the PNG and SVG beside this document. Before reusing it for a release,
refresh the same-host GIN comparison on the release revision with equal corpus,
query set, visibility, settings and offered load. Keep remote TIN observations
separate unless an actually matched experiment becomes available.
