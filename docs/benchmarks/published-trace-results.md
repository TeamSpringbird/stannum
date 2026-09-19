# Published-trace integration results

September 19, 2026. Local native ARM64 Docker, PostgreSQL 18.6, one engine at
a time, four-CPU/4-GiB server limit, 1 GiB shared buffers, two query workers.
Durability and autovacuum remained enabled. Other local services remained
running. These are diagnostic observations, not maximum capacity measurements
or a reproduction of the published TIN numbers.

The [run guide](published-trace.md) documents the pinned driver and adapters.
[Compact results and identities](published-trace-results.json) retain the
measured values, source/image/driver hashes, workload configuration, correctness
coverage and full-count disagreements. Raw artifacts remain under the ignored
`benchmarks/results/` directories named below.

## 100k Wikipedia, mixed counts

Each engine ran the 302 published base queries as 906 AND/OR/phrase forms,
with ten seconds of query warmup and sixty seconds of measurement. All 906
forms were timed in both observations.

| Observation / engine order | Engine | QPS | p95 ms | p99 ms |
| --- | --- | ---: | ---: | ---: |
| `tin-count-100k-01`, Stannum first | Stannum | 3313.4 | 1.991 | 3.661 |
| same | Stored-vector GIN | 930.7 | 11.117 | 16.394 |
| `tin-count-100k-02`, GIN first | Stored-vector GIN | 914.8 | 11.322 | 16.483 |
| same | Stannum | 3396.5 | 1.973 | 3.592 |

Stannum's index was 158.12 MiB versus GIN's 110.22 MiB. Total relation storage,
including the primary key, was 345.34 MiB versus 658.53 MiB: GIN's stored
vectors add heap storage. Both working sets are small relative to the memory
budget; this does not test storage pressure.

The two observations are not pooled into a formal repeated-run statistic:
between them we hardened the validation and fixed the upstream write picker.
The timed read SQL and traversal stayed unchanged, but the driver and wrapper
fingerprints differ and are retained separately.

The second observation found **11 phrase forms with different full-corpus
counts**, despite no disagreements on its 1000-row validation sample. For
example, `"the movement"` returned 588 rows from Stannum and 555 from PostgreSQL.
The position-limit reproduction explains why PostgreSQL cannot be a raw-text
phrase oracle; individual full-corpus discrepancies have not all been minimized.
All 604 conjunction/disjunction forms had equal full counts. No mixed-query
speedup ratio is asserted, and equal counts do not prove equal membership on
the unsampled rows.

## Concurrent updates

`tin-updates-02` ran all 302 disjunction count forms on a 1000-document prefix,
with a 100 updates/sec requested ceiling for thirty seconds. The live-page
fallback was enabled for both engines.

| Engine | Completed updates | Actual updates/sec | Failed updates | Read QPS | Read p95 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Stannum | 2857 | 95.24 | 0 | 6716.9 | 0.597 |
| Stored-vector GIN | 2847 | 94.90 | 0 | 3794.7 | 1.137 |

Both preserved the heap row count and all 906 pre-update query counts.
Exact membership checks passed before and after the updates. All timed
disjunction counts agreed across engines; one untimed phrase form differed.
These rates are completion observations below an offered ceiling, not write
capacity estimates. This fixture is too small for a scale claim.

The unpatched attempt, `tin-updates-01`, failed with 69 zero-row updates after
randomly selecting empty heap pages. It remains retained as a failed run.
A deterministic real-server test first reproduced the empty-page failure,
then passed forward selection, wraparound, and empty-relation behavior after
the fix. The exactly-one-row assertion was preserved.

## Ranked smoke and next measurement

The initial `tin-topk-01` smoke on 1000 documents passed exact membership and
all 906 exhaustive same-engine top-10 score checks. It observed approximately
143.5 Stannum queries/sec versus 2286.9 GIN queries/sec; the respective ranking
algorithms are BM25 and `ts_rank_cd`, so this is not a relevance-equivalent
comparison. Several Stannum disjunctions took roughly 70–80 ms, while count
queries were much faster. This identifies a useful profiling workload, not
the cause of the cost. The final `tin-topk-final` run passed the hardened
validation and source guards: 147.6 Stannum queries/sec and 2236.6 GIN
queries/sec, with all 906 forms timed and all 906 exhaustive score checks
passing for each engine.

Validation also passed 98 Python harness tests, eight JavaScript query-adapter
tests, and seven Go driver tests against the isolated server (including real
query cancellation and the empty-page regression). A fresh upstream checkout
plus the committed patch reproduced identical driver source fingerprints.

The next performance investigation should profile ranked disjunctions and
establish a larger-corpus ranked baseline before choosing a codec or SIMD
optimization. The next harness work is repeated/paired-image orchestration,
then deletion of the redundant launchers listed in the run guide. Exact
published-corpus loading, other engines, and memory-pressure measurements
remain separate, explicit gates.
