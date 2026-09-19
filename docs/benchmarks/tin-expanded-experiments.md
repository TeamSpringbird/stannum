# Expanded remote TIN experiments

This follow-up uses the existing `benchmarks/tin.py experiment` entry point to
probe real TIN databases with persistent sessions and immutable Wikipedia
corpora. It extends the [small-fixture catalog](tin-plan-catalog.md) and
[two-machine comparison](tin-machine-comparison.md).

## First completed wave: 100,000 Wikipedia articles on both machines

Both runs completed 828 EXPLAIN observations, eight concurrency windows and
mutation/VACUUM phases. Each has two recorded fixture syntax errors: `common
NOT rare` is invalid; the later collector uses `common AND NOT rare`. Every
one of the 84 forced-plan score-sequence checks per machine passed. These
checks compare ordered scores while allowing tied row choices; they do not
establish comprehensive TIN/Lead semantic equivalence.

The [derived results](tin-expanded-results.json) preserve grouped timings,
plan-provider sequences, errors, source hashes and artifact IDs. Ten
[complete plan examples](tin-expanded-plan-examples.json) retain the SQL and
full counters for representative comparisons. The
[offline analysis](../research/tin-100k-experiment-analysis.md) traces the
findings to exact local SQL/JSON artifacts and explains their limits.

| Observation | Small database | Larger database |
| --- | ---: | ---: |
| Wikipedia index build, one observation | 22.48 s | 10.53 s |
| Complete relation, including indexes and TOAST | 392,282,112 bytes | 395,083,776 bytes |
| `history OR war`, 25% ID filter, LIMIT 10, automatic median | 31.57 ms | 29.16 ms |
| Same query, forced TID filter pushdown median | 19.20 ms | 18.53 ms |
| `count(history)` median | 26.11 ms | 10.40 ms |

Forced strategy timing uses three repetitions with permuted order and matched
2 MiB work_mem. The automatic and pushdown plans can share the same node tree
while reporting different **Execution Mode** fields. In the larger database's
25% filter example, pushdown emitted 6,974 text-search rows versus 27,946 for
the automatic generic conjunction. It touched more index pages but ran faster.
Page-touch minimization alone would therefore be a misleading objective.

The two default builds had four immutable segments with identical document
partition sizes, but different page layouts. Build memory was 16 MiB on the
small database and 690 MiB on the larger one. Query defaults and cache sizes
also differ. These are deployment comparisons, not isolated causal estimates
of CPU or RAM improvements.

At 100k rows, `count(history)` selected parallel text scanning and aggregation
on the smaller server, but `Tin Count` on the larger server. A broad six-term
OR count also chose different provider trees. The tiny fixtures' identical
plans did not generalize to all larger workloads.

## Million-document follow-up

The larger database loaded one million articles in 69.73 seconds and built its
index in 116.32 seconds. The complete relation occupied 3,803,242,496 bytes;
its indexes occupied 1,643,405,312 bytes. The build produced four immutable
segments. This exceeds shared_buffers, but does not imply that the full working
set was absent from the operating-system cache.

The run completed **639 query observations without query errors**, including
reversed first-five prepared-query histories, dynamic LIMIT, joins and LATERAL,
and multiple TIN indexes. All 42 forced-ranking score-sequence checks and all
30 multi-index membership/count comparisons passed. TIN remains an observed
implementation, not Stannum's correctness oracle; that role stays with Lead.

The 25% filter/LIMIT 10 result reproduced at this scale: automatic generic
conjunction had a 378.96 ms median; forced TID pushdown had a 254.64 ms median.
These are three instrumented observations per strategy, not confidence bounds.

The mixed remote-client workload reached approximately 20 queries/sec with
four clients and 23 with eight; p95 increased from about 0.47 to 0.88 seconds.
Ascending and descending client-count passes gave similar results. This is
neither a hardware-independent capacity limit nor a TIN-versus-Stannum benchmark.

### Mutation, maintenance and recovery

After deleting 100,000 rows and updating 50,000 surviving rows, VACUUM completed
in 7.24 seconds. The post-VACUUM snapshot reported 1,050,000 current indexed
documents and 150,000 dead documents. `segment_info` also exposes retired
entries during maintenance; totals across every returned row can double-count
retired contents. The derived summary separates current and retired entries.

The initial REINDEX **failed with a lock timeout**, and the three-second cleanup
wait failed too. The original run remains marked failed in its raw manifest;
its successful earlier observations have not been discarded or relabeled.
A subsequent lock snapshot found no remaining blockers, consistent with
transient contention but insufficient to identify its original holder.

Recovery used a 30-second lock wait. REINDEX completed in 99.10 seconds; the
resulting four current segments contained 900,000 documents and no dead entries.
`tin.fsck(index, true)` returned no errors in 8.14 seconds. Three post-rebuild
plans were captured. Cleanup was verified with zero remaining owned schemas.
The sidecar `recovery.json` records this separately from the failed original run.

A live 100-row lock regression reproduced the timeout with a four-second lock
holder: the original three-second wait failed with SQLSTATE 55P03, while the
new maintenance helper succeeded after 4.09 seconds and restored the session's
original lock timeout. Artifacts are under
`benchmarks/results/tin-lock-wait-regression`. The helper is now used for
REINDEX and cleanup. No claim is made that the original blocker was identified.

## Controlled layout and output projection

Follow-up runs vary initial/target segment count and build memory, and compare
output serialization. Final results will be added once those runs finish.

## Replay and bounds

Use standard libpq environment variables supplied outside the repository. The
collector never persists the environment or connection strings. On macOS,
`PGSSLROOTCERT=/etc/ssl/cert.pem` may be needed with the binary libpq package;
keep `PGSSLMODE=verify-full`.

The experiment command requires `psycopg[binary]==3.3.6` in the interpreter's
environment; other benchmark commands do not require it.

```sh
python benchmarks/tin.py experiment \
  --dataset /path/to/wikipedia-100000 \
  --rows 100000 --minutes 45 --seconds 15 --max-clients 8 \
  --output benchmarks/results/tin-expanded-new

python benchmarks/tin.py experiment \
  --dataset /path/to/wikipedia-100000 --rows 100000 \
  --stages queries forced projection \
  --index-segments 4 --build-memory-mb 16 \
  --output benchmarks/results/tin-layout-new
```

Use the full 100k or 1m corpus for quantitative filter comparisons. A prefix
used for a smoke test retains original IDs, so `id <= rows * fraction` need
not select the nominal fraction. Corpus files are checksum-verified before
loading. Setup SQL, queries, complete plans, source snapshots, segment records,
concurrency samples and summary events are saved under the output directory.

Each run creates one unique schema, changes only its own session settings,
and drops its own schema in `finally`, verifying cleanup. Query timeouts are
60 seconds; exclusive maintenance and cleanup allow a 30-second lock wait.
Builds, loading and selected maintenance operations allow up to
300 seconds. The wall-clock budget is checked between operations, not a hard
interrupt inside an operation. Concurrency is capped at eight persistent
connections. A failed run or failed cleanup is retained and returns nonzero.
Query-probe errors are recorded individually so one unsupported shape does not
discard the rest of the catalog; inspect their count before claiming success.

Concurrency results are client-observed, include network and connection setup,
and are not an isolated server saturation measurement. EXPLAIN server timings
are separately recorded and include instrumentation. No run is labeled cold
cache: index building and earlier probes already access the data. A buffer
read need not be physical disk I/O. Upper-level buffer counters include their
children; do not sum them across the tree.

Output projection probes use PostgreSQL 18 `SERIALIZE TEXT` to include output
conversion and TOAST fetches, while retaining `TIMING OFF`. Overall execution
time includes serialization, but a separate serialization timer is not
available with per-node timing disabled. Network transfer remains excluded.
[PostgreSQL EXPLAIN](https://www.postgresql.org/docs/18/sql-explain.html)

The [research notes](../research/tin-probe-interpretation.md) distinguish
published architecture, installed inspection functions, and hypotheses.
