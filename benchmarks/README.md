# Recording search performance

For the repeatable, containerized Mac Studio workflow, start with
[local campaigns](LOCAL.md). Benchmark execution and analysis are local-only.

Use **commit hashes for source identity, unique run directories for measurements,
and tags for human milestones**. A commit can have many measurements. Never replace
an old result with a newer one, and never infer a speedup from a tag alone.

`run.py` is the first executable measurement loop: standard-library Python plus
`psql` and `pgbench`. It captures normal SQL execution with concurrent updates,
checks fixture match sets before/after, and stores raw transaction logs. It does not
yet constitute a representative competitive benchmark or a production soak test.

## Run

Build/install Lead in release mode first (see the repository README for toolchain
setup). Use a dedicated server and a **fresh database for every repetition**:

```sh
export PATH="$(brew --prefix rustup)/bin:$HOME/.cargo/bin:$(brew --prefix postgresql@18)/bin:$PATH"
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
cargo pgrx install --package tin --no-default-features --features pg18 --release
cargo pgrx start pg18

export PGHOST=localhost PGPORT=28818
createdb lead_bench_baseline_01
python3 benchmarks/run.py run \
  --engine lead --database lead_bench_baseline_01 \
  --output benchmarks/results/baseline-01 \
  --environment local-arm64-pgrx \
  --build-id "$(git rev-parse HEAD):cargo-pgrx-release" \
  --artifact "$(pg_config --pkglibdir)/tin.dylib" \
  --profile mixed --rows 10000 --seconds 60 --warmup 10 \
  --clients 2 --write-rate 20 --label initial-baseline
```

The installed library location varies by packaging. Prefer
`$(pg_config --pkglibdir)/tin.dylib` on macOS, or `tin.so` on Linux, if that differs
from the example. `--artifact` hashes the binary; `--build-id` is a required build
attestation or immutable container-image digest. Source checkout identity alone
does not prove which binary the server loaded. Restart existing sessions after
installing a new binary. Do not change builds while a benchmark is running.

Connection credentials use normal libpq environment variables or `.pgpass` and
are not recorded. The runner requires a `lead_bench_*` database name, creates its
own `documents` table, and refuses to overwrite it. It leaves the database for
inspection; explicitly drop only benchmark databases when finished. It does not
change server-wide settings. An existing `PGOPTIONS` is honored; relevant effective
settings are saved. Query timeout is explicitly 60 seconds.

Use `--profile count` for equivalent cross-engine count queries, `ranked` for
full-scoring top-10, or `mixed` for both. `--write-rate 0` selects read-only.
Rows must be a multiple of 1,000. A 1,000-row / 5-second run is only a smoke test.
For useful latency distributions increase duration, scale, and repetitions.

## Engine adapters and semantic limits

| Adapter | Installed extension | SQL contract | Status |
| --- | --- | --- | --- |
| `lead` | `tin` | TINQL; `tin.full_score` | Locally exercised |
| `gin` | Built-in | `simple` tsvector/tsquery; counts only | Locally exercised |
| `paradedb` | `pg_search` | Current `USING paradedb`, `|||`, `&&&`, `###`, `pdb.score` | Smoke checked on 0.25.9 |
| `pg_textsearch` | `pg_textsearch` | Current text `@@` Boolean filtering, `USING bm25`, `<@>` scoring | Smoke checked on pinned 1.5.0-dev; not validated on released 1.4.0 |

The latter two adapters follow current primary documentation, **not** the older
versions used in TIN's launch article. An older extension may reject their SQL;
that is an adapter/version mismatch, not proof of a missing capability or poor
performance. The runner preserves failure status rather than reporting a speedup.
Provision engines separately, pin their exact versions and image digests, and
record server hardware/CPU/memory/storage constraints in `--environment` and the
experiment notes. Do not compare native macOS against a resource-limited VM.

Sources: [ParadeDB match](https://www.paradedb.com/docs/reference/full-text/match),
[phrase](https://www.paradedb.com/docs/reference/full-text/phrase),
[score](https://www.paradedb.com/docs/reference/full-text/score),
[index example](https://www.paradedb.com/docs/reference/full-text/top-k),
[pg_textsearch](https://github.com/timescale/pg_textsearch).

This fixture uses simple ASCII words without stemming-sensitive tokens; all engines
must return exactly the expected ID sets. Ranked probes additionally check result
cardinality, membership, uniqueness, finite scores, and descending order. They do
**not** prove score arithmetic, globally optimal top-k, or equivalent relevance.
GIN ranking is deliberately excluded: `ts_rank` is not BM25. Different BM25
statistics, quantization, and phrase scoring can change rank/tie groups. Therefore
the comparison command blocks cross-engine ranked/mixed speedup claims until an
independent ranking-quality/contract suite is added. Lead explicitly uses
`full_score` to avoid default dense-term elision.

For pg_textsearch the projection negates its negative BM25 value for a common
descending-score result convention, but the actual ORDER BY retains the native
`body <@> query ASC` expression. Negating the ORDER BY expression could hide its
index ordering. The Boolean filter is retained to guarantee exact match semantics;
the current engine documents Boolean and ranked retrieval as separate scan modes.
Audit plans rather than assuming this combined query uses both optimizations.

See [validated smoke environments](validated-environments.md) for exact versions
and reproduction commands. Those environments validate adapters, not fair relative
performance: a controlled campaign must align their resource and platform budgets.

## Artifacts and history

Every directory contains:

* `manifest.json`: exact commit/tags, engine-source fingerprint, dirty status,
  binary hash/build ID, harness hash, environment label, client host description,
  PostgreSQL/extension versions, selected effective settings, workload/seed,
  fixture/SQL hashes, index DDL/build time, traffic timestamps, completion status.
* `source.patch`: tracked engine changes relative to HEAD. Untracked engine files
  have hashes but are not archived: commit source before publishing a result.
* `fixture.sql`, `index.sql`, `query-*.sql`, `writer.sql`: actual generated workload.
* `correctness-*.json`, `ranked-*.json`, `plan-*.json`: result checks and diagnostic
  plans collected outside the timed load.
* `reader-log.*`, `writer-log.*`, `reader.txt`, `writer.txt`, `warmup.txt`: raw
  pgbench records/output. Failures and skipped transactions never become successful
  latency samples. Process errors make a run incomplete.
* `summary.json`: per-query completions, throughput, p50/p95, and p99 only with at
  least 1,000 samples. Percentiles use nearest rank. Writer metrics include schedule
  lag and achieved rate, rather than assuming the requested rate was met.
* `before.json`, `after.json`: relation sizes, heap/index/TOAST I/O counters and
  server WAL insertion positions. Statistics can lag; these are snapshots, not an
  assertion of physical-device reads. WAL is server-wide and includes maintenance.

The fixture and plans warm data; this is a **warm-cache** experiment. Readers run
closed-loop at fixed concurrency. A separate rate-scheduled writer toggles one
indexed token without changing tested match sets or document lengths. Traffic
starts are close but not synchronized; timestamps expose startup/end skew. This
is not an open-loop reader latency-SLO test and does not eliminate coordinated
omission under overload. Keep the distinction in published results.

```sh
python3 benchmarks/run.py compare benchmarks/results/baseline-01 benchmarks/results/candidate-01
python3 benchmarks/run.py compare --cross-engine benchmarks/results/lead-count-01 benchmarks/results/gin-count-01
python3 benchmarks/run.py history benchmarks/results > benchmarks/results/history.csv
python3 -m unittest discover -s benchmarks -p 'test_*.py'
```

Comparison rejects incomplete runs and differences in workload, harness,
environment, recorded hardware, PostgreSQL version/settings, or client version.
Cross-engine comparisons require count-only and matching core settings; engine
settings remain visible for review. A successful comparison is a descriptive
single-run ratio, **not statistical evidence or automatic acceptance**. Inspect
write throughput and reader/writer tails before interpreting it.

`history.csv` has one row per query/run, including commit, label, fingerprints,
latency and achieved read/write throughput. Its cohort identifies comparable
conditions. Chart each query within a cohort; separate cohorts when fixtures,
hardware, or harness change. Tags such as `perf/selective-postings-v1` can label a
reviewed milestone later; this harness does not create tags or commits.

Raw results are ignored by Git. Keep code, workload definitions, protocol, and
small reviewed milestone summaries in Git. Archive complete run directories as
local archives with a retention policy; milestone summaries
should link to immutable artifacts and checksums. No upload occurs automatically.

## Evidence ladder

1. **Local iteration:** smoke correctness plus short latency runs to spot large
   effects. These cannot justify competitive claims.
2. **Controlled regression:** dedicated machine, identical budgets and fresh
   databases; at least five independent runs per candidate, alternating baseline
   and candidate order. Compare per-run metrics, not pooled transactions as if
   independent. Report medians and confidence intervals before setting gates.
3. **Competitive campaign:** pinned real corpus/query traces, semantic audits,
   tuned and default configurations as separate lanes, data fitting/exceeding RAM,
   ranking-quality checks, and readers plus inserts/updates/deletes. Fix a read p99
   ceiling and required achieved write rate before claiming throughput wins.
4. **Sustained operation:** long enough to reach spills, compaction and VACUUM;
   capture CPU/RSS/device I/O, maintenance backlog, index growth, recovery, and
   post-load drain time. The current short token-toggle writer does not cover this.

Keep CPU/allocation profiles as diagnostic attachments to the same run identity.
Run profiling separately from uninstrumented timing; use profiles to explain
results, not replace end-to-end measurements. This initial harness does not yet
automate host profiling, confidence intervals, real-corpus import, competitor
provisioning, inserts/deletes, or maintenance-backlog collection.

See [Tin configuration research](../docs/tin-configuration-research.md) for which
knobs change semantics versus execution and why build/maintenance state matters.
The [pgbench documentation](https://www.postgresql.org/docs/18/pgbench.html)
defines the retained log fields and scheduling behavior.

For the nested 100,000 / 1,000,000-document Wikipedia datasets and sequential
background baseline campaigns, see [the real-corpus protocol](LOCAL.md#wikipedia-baselines-100000-and-1000000-documents).
Use `dataset.py` to prepare checksummed corpora and `baselines.py` to run both sizes
with five repetitions (five minutes at 100k; thirty minutes at 1m). These are separate cohorts from the synthetic
10,000-document benchmark.
