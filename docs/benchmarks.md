# Benchmarks

Stannum is measured on the workloads PlanetScale published for TIN, at a scale
where the index does not fit in memory. Every run also checks its answers: a
faster query that changes the result is not an improvement.

## Benchmarks v2

From 2026-10-09 the reference setup is **benchmarks v2**: TIN v1.0.6's
published setup (8 pinned vCPUs, 64 GB), with the four published scenarios and
Stannum's default and full scoring, set beside PlanetScale's published TIN,
TIN_FULL and ParadeDB 0.26.0 numbers. Locally, and optionally, ParadeDB 0.26.0
can be measured as a one-off calibration anchor. It is the default for every 150M run
(`tin.py --profile v2`, `benchmarks/local/workload.sh 150m`,
`benchmarks/aws/v2-campaign.sh`). The setup of the results below, the launch
post's 32 GB without pinning, stays selectable as `legacy`
(`--profile legacy`, `workload.sh 150m-legacy`); those results are legacy and
are not comparable with v2 numbers. See
[Matching TIN v1.0.6's published setup](#matching-tin-v106s-published-setup).

## Legacy results

### AWS, 150 million rows

Run r8 at commit `313a696`, 2026-09-26, on the legacy protocol (below):

| Workload | Queries/s | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: |
| mixed | 269.9 | 15 ms | 114 ms | 172 ms |
| conjunction-phrase | 437.9 | 12 ms | | 91 ms |
| disjunction-updates | 127.6 | 40 ms | | |

The disjunction-updates run applied 254,224 updates in 600 seconds (p50
1.1 ms, p99 5.5 ms) with 0 errors. The count and ranked checks were clean
before and after the updates.

PlanetScale's TIN announcement reports 199 queries a second for the mixed
workload on the same instance type and corpus. That figure is PlanetScale's own
measurement, not one taken beside Stannum in the same run.

### Local, 150 million rows

On an Apple Silicon laptop (arm64 Docker under OrbStack), with the container's
device reads capped at 20,000 IOPS, 2026-09-26, commits `33e6c22` and
`a5cd99f`: the mixed workload ran at 373 to 377 queries a second, p50 12 ms,
p99 116 ms, with 0 ranked mismatches.

The local database answers the same queries over the same corpus, so pages
touched and candidates scored carry over to the AWS host directly. Throughput
does not: in the two AWS runs of the current format, the host ran the mixed
workload at 0.81 and 0.75 of the same build's local figure.

## Datasets

All corpora are PlanetScale's prepared datasets, fetched and checksum-verified
by `benchmarks/published_dataset.py` from the pinned
[paradedb-benchmarker](https://github.com/planetscale/paradedb-benchmarker)
revision `f487fbaa`. No row is truncated, normalized, remapped or reordered.

| Dataset | Rows | Used for |
| --- | ---: | --- |
| Stack Exchange | 150,000,000 (84.5 GB of CSV) | Headline runs, on AWS and locally |
| Stack Exchange, 15 million row prefix | 15,000,000 | Fast local iteration ("the mock") |
| Wikipedia | 5,032,104 | Count workloads |

The Stack Exchange trace has 1,254 query records. Each expands to a
conjunction, a disjunction and a phrase form of the same words, 3,762 forms
in all, run as ranked top-ten queries (`ORDER BY` score `LIMIT 10`).

## Workloads

The driver is PlanetScale's benchmarker (Go and k6), pinned and run through
`benchmarks/tin.py`, which adds the Stannum adapter, isolated containers, the
correctness gates and provenance.

- **mixed**: every form of the trace, conjunctions, disjunctions and
  phrases, read-only.
- **conjunction-phrase**: only the conjunction and phrase forms, read-only.
- **disjunction-updates**: the disjunction forms while a separate connection
  updates rows at a paced rate. The run fails if any update errors or
  misses the driver's deadline.

The legacy protocol, used on AWS through r8: an i7i.8xlarge instance in us-east-1,
PostgreSQL 18 in a container with 8 CPUs, 32 GB of memory and 24 GB of shared
buffers, 8 clients, 600 seconds per workload after warmup. The local 150
million row runs use the same container sizes and clients. Because Docker's
virtual disk is served from the host's page cache, local runs cap device
reads to what the instance's NVMe delivered, and drop the VM's page cache as
measurement starts so the index is read cold except for shared buffers. The
15 million row mock scales the same regime down: 2 GB of shared buffers in a
5 GB container, eight clients on eight CPUs.

## How correctness is checked

Speed and correctness are measured in the same run, outside the timed
interval:

- **Membership and counts.** Before timing, a sample of query forms is
  checked for exact result sets on a sample of rows: the indexed candidate
  set against the same operator evaluated without the index. Full-corpus
  counts of every checked form are recorded. Update runs repeat the checks
  after measurement.
- **Ranked results.** Sampled ranked forms compare the scores of the top
  ten with exhaustive scoring of every match on the same engine, allowing
  any selection among tied rows. A **ranked mismatch** is a form whose pruned
  top ten differs from that exhaustive answer. This proves the pruning exact,
  not the BM25 formula itself.
- **Differential oracle.** `benchmarks/oracle.py` runs 47 query shapes over
  five mutation states against a reference engine: upstream Lead in CI on
  every push, and PlanetScale TIN by hand. See
  [compatibility](compatibility.md#reference-oracle-against-lead).
- **Conformance.** The [TIN conformance suite](../conformance/README.md)
  checks Stannum against TIN 1.0.3's and 1.0.4's recorded answers.

The pgrx suite also compares the pruned top k against exhaustive scoring bit
for bit, and the ranked-scan fuzzer checks it under concurrent writes; see
[testing](testing.md).

## Reproducing

Run from the repository root. [Testing](testing.md) covers the correctness
suites; the benchmark tools live in `benchmarks/`, each with `--help`.

1. Fetch a corpus:
   `python3 benchmarks/published_dataset.py --corpus stackexchange --output DIR`.
2. Build an image from the working tree:
   `python3 benchmarks/tin.py build-image --image TAG --output DIR`
   (`--base postgres:18-trixie` builds a native image for local runs).
3. Run a workload:
   `python3 benchmarks/tin.py run --published-corpus stackexchange --dataset DIR --image TAG --workload topk --style mixed ...`.
   `--save-database` keeps the built, vacuumed and checked database so later
   runs start from it with `--load-database`; `--validation-queries` and
   `--ranked-validation-queries` size the correctness sample.
4. For local runs that do not fit in memory, `benchmarks/local/` wraps these
   steps with the read caps and cache drops described above; see
   [its README](../benchmarks/local/README.md).
5. `benchmarks/aws/` holds the CloudFormation stack and host scripts for the
   AWS protocol.
6. To set one query beside another engine's plan rather than measure
   throughput, `benchmarks/compare/compare.py` loads the same N rows into a
   TIN database and a Stannum database and runs trace queries in eleven
   shapes (term, conjunction, disjunction and phrase, ranked and counted;
   filtered by id; with an id tiebreaker) under `EXPLAIN (ANALYZE, BUFFERS)`
   on both. It reports times, shared blocks, plan nodes, TIN's page touches
   by area and Stannum's bytes fetched by area, and checks that the answers
   agree; see its `--help`.

Each run writes a manifest (source and image identity, corpus hashes,
settings), the correctness outputs, per-query and per-family latency, and
resource counters to its output directory. Keep results and connection
credentials out of Git.

## Profile-guided builds

`build-image --pgo 1` (the Dockerfile's `STANNUM_PGO=1`) compiles the
extension with `-C profile-use` and the committed profile for the image's
architecture, `benchmarks/pgo/<arch>.profdata` (`uname -m`: `aarch64`
today). Without it, builds are as before. A profile changes code layout,
inlining and branch weights only; answers and formats do not change.

The profile has to be trained by the extension itself, in PostgreSQL. The
offline replay (`bench --bin tnsreplay`) cannot stand in: its hot walk is
`Walk<T>` instantiated over the replay's page type, its `engine` is built
with other features, and so every function the extension compiles has
another symbol and no profile. A replay profile does speed the replay up,
which is what `script/pgo-build replay` is for.

To retrain, on a host of that architecture with a copy of a saved 150M
database (`--save-database`; the server writes to it):

```sh
STANNUM_PYTHON=/path/to/python-with-psycopg \
  script/pgo-build image --data COPY_OF_DB --trace trace.tsv
python3 benchmarks/tin.py build-image --pgo 1 --image TAG --output DIR ...
```

`trace.tsv` is `script/replay-oracle.py trace` over the published queries.
`pgo-build image` builds the instrumented image (`--pgo generate`), runs
every query of the trace through the benchmark's ranked statement once with
`stannum.score` and once with `stannum.full_score`, stops the server so
that each backend writes its counts, and merges them with the toolchain's
`llvm-profdata` (`rustup component add llvm-tools`) into
`benchmarks/pgo/<arch>.profdata`. The run takes about 15 minutes. Commit
the new profile with the change that made it stale.

**Staleness.** Each function's counts are keyed by its symbol name and a
hash of its control flow. A function whose code changes, or whose symbol
changes (a new crate hash after a dependency, feature or toolchain change),
loses its profile silently: the build stays correct and that function is
optimized as without PGO. The merge is `--sparse`, so code the training
never ran (counts, writes, VACUUM) also has no profile, rather than a
profile that marks it cold. Retrain after a change to the walk, the
decoders or the toolchain, and after anything that moves the hot path; an
A/B of the old and new profile shows whether it was needed. An x86-64
profile has to be trained on x86-64.

## Matching TIN v1.0.6's published setup

Benchmarks v2 reproduces the setup of PlanetScale's
[TIN v1.0.6 post](https://planetscale.com/blog/tin-v106) as closely as the
post allows, so Stannum's numbers sit beside its TIN, TIN_FULL and ParadeDB
0.26.0 numbers. The published tables live in
`benchmarks/published/tin-results.json`, versioned and with source URLs,
together with the launch post's tables and, as a separately labeled
third-party dataset, the figures from ParadeDB's
[Opening a closed TIN](https://www.paradedb.com/blog/opening-a-closed-tin).
`benchmarks/v2.py report` reads them from there.

### Checklist

| Setting | Ours (v2) | TIN v1.0.6 | Source |
| --- | --- | --- | --- |
| x86 host | i7i.8xlarge (`stack.json`) | i7i.8xlarge, 5th-gen Xeon | post |
| ARM host | i8g.8xlarge: 32 vCPU, 256 GiB, instance-store NVMe, the i7i's size class | i8g (Graviton4), **size not stated** | post |
| Benchmarker | same instance; k6 pinned with `taskset` to the cores the server does not use | same instance | post |
| Server CPUs | `--cpus 8` and `--cpuset-cpus` from the host's `lscpu -e` | 8 vCPUs, CPU pinning; **cores and method not stated** | post |
| "8 vCPUs" reading | `siblings` by default (4 cores x 2 threads on i7i: `0-3,16-19`); `distinct-cores` (one thread on each of 8 cores: `0-7`, siblings `16-23` left idle) also measured | not stated | ambiguity, below |
| Server memory | 64g for the build and the queries | 64 GB | post |
| shared_buffers | 24GB | 24 GB | post |
| maintenance_work_mem | 24GB | 24 GB | post |
| max_parallel_workers | 8 | 8 | post |
| Other server settings | `max_parallel_maintenance_workers=8`, `max_parallel_workers_per_gather=2` (ParadeDB), shm 16g | the benchmarker's defaults | `benchmarks/compose.yml` at `f487fba` |
| work_mem, JIT | Stannum: 16MB, `jit=off` (the harness's, as in every earlier run); ParadeDB: its image's auto-tuning and `jit` on, as the benchmarker starts it | benchmarker defaults; TIN's image is not public | deviation, below |
| PostgreSQL | 18.6 (`18.6-1.pgdg13+2`) in both images; recorded per run (`postgres_version`) | not stated; the launch post said 18.6 | post, launch post |
| Corpus | Stack Exchange, 150,000,000 documents, the benchmarker's pinned dataset | Stack Exchange, 85 GB, 150M documents | post |
| Trace | 1,254 queries, each a conjunction, a disjunction and a phrase | the same | post |
| Scenarios | conjunction, disjunction, phrase, mixed (`--style`) | the same four | post |
| Ranking | `ORDER BY stannum.score(ctid) DESC LIMIT 10` | `ORDER BY tin.score(ctid) DESC LIMIT 10` | post |
| Elision | `dense_ratio` 0.10: a term with `df >= 0.1 x N` (immutable segments, f64) is not scored, as Lead's `DenseRatio::elides` | terms in more than 10% of documents are not scored | post; `engine/src/bm25.rs` |
| TIN_FULL | `--score-function full_score`: `stannum.full_score(ctid)` | `tin.full_score(ctid)` | post |
| ParadeDB | published numbers only. Optional local calibration: `paradedb/paradedb:0.26.0-pg18@sha256:52fc9c95…` (official, multi-arch with a native arm64 image; PostgreSQL 18.6 inside); `body &&& $1`, `body ||| $1`, `body ### $1` on the query's plain text through the benchmarker's paradedb backend, `pdb.score(id)`; index `USING bm25 (id, body) WITH (key_field=id, target_segment_count=8)`. Never on AWS | the final 0.26.0, `|||`, `&&&`, `###`; **image and index flags not stated** | post; the benchmarker's `post.sql` |
| Clients | 8 | not stated (launch post: at most 8) | |
| Duration, warm-up | 600 s after 10 s | not stated (benchmarker default 60 s and 10 s) | |
| Table state | static; built once, `VACUUM ANALYZE`, saved; every run restarts from a copy | static, one-shot build, clean VACUUM | post |
| Cache state | AWS: the copy leaves the OS cache cold; local: the VM's page cache is dropped as measurement starts | not stated | |
| MB/query | `index_mib_per_query` in `comparison.json` (below) | MB/query; **method not stated** | |
| Index size | Stannum TNS1 35 GiB (1.5x shared buffers); ParadeDB 0.26.0 about 63 GiB (67.3 GB published) | about 2x shared_buffers | post |

### Ambiguities and deviations

- **"8 vCPUs".** On an i7i.8xlarge (16 cores, 32 threads, Linux numbering
  each core's second thread 16 higher), 8 vCPUs as AWS and PlanetScale sell
  them are 8 hyperthreads, 4 cores x 2. That is the default (`siblings`).
  The other reading, 8 distinct cores, gives the server whole cores instead of
  hyperthread pairs, so noticeably more compute (a second hyperthread adds only
  a fraction of a core). A v2 campaign measures both and records the
  topology (`lscpu -e`) and the chosen CPUs (`cpu_pinning` in each
  manifest). On Graviton4 (no SMT) and on a Mac's Docker VM the two readings
  are the same CPUs and run once. On the Mac the cpuset names VM vCPUs,
  which macOS schedules on its 12 performance and 4 efficiency cores as it
  likes, and k6 runs on macOS itself, so it cannot be pinned.
- **Server settings beyond the three named.** "Benchmarker defaults" are
  ParadeDB-shaped: the ParadeDB image tunes `work_mem` and
  `effective_cache_size` from the container's memory at first start, and
  leaves JIT on. Our optional ParadeDB runs keep exactly that. Stannum keeps the
  harness's `work_mem=16MB` and `jit=off`, which every earlier run used: a
  ranked scan's plan cost can cross `jit_above_cost`, and TIN's own image and
  configuration are not public. Recorded in each manifest's `docker_run`.
- **ParadeDB's index flags** (for the optional local calibration). The post says neither. We use the
  benchmarker's `post.sql` verbatim (`key_field` is a no-op since 0.26.0;
  `bm25` is 0.26.0's alias of the `paradedb` access method and the name the
  pinned driver's I/O counters select).
- **ParadeDB's build memory** (local calibration). 64 GB for the build, as the benchmarker's
  `create` target. If a 150M build needs more, the run fails with the
  container's `memory.events` in `after-build-cgroup.json`; record it as a
  deviation rather than raising the limit silently.
- **Duration, warm-up, clients and cache state** are not stated; ours are
  above.

### MB per query

The benchmarker's dashboard shows `PER QUERY` = (`idx_blks_read` +
`idx_blks_hit`) x 8 KiB / completed queries, from `pg_statio_user_indexes`
restricted to the engine's index access method, with `pg_size_pretty` units
(1024-based, so MB means MiB). Its counters are reset before warm-up, so
warm-up traffic is in the numerator; the denominator is the measured
queries. TIN's MB/query is that number. Ours is `index_mib_per_query` in
`comparison.json` (and "Index MiB/query" in `report.md`), computed the same
way from the same export: compare it with TIN's MB/query. It counts logical
block accesses, shared-buffer hits included, not disk reads. The local
`report.py` line also prints "disk read ... MB/query", the container's
physical reads from cgroup `io.stat`; that one is not comparable with TIN's.

### The benchmarker revision

We pin `f487fba` (branch `datasets`, 2026-09-16), the newest commit there,
which added the TIN query forms to the traces. Branch `ps-mods` (`fbad65c`)
was checked and not adopted. Its delayed-worker, generic-backend and
telemetry commits (`7533661`, `72c6e9c`, `a24217f`) are already ancestors of
`f487fba`. What `ps-mods` has beyond them is one merge of upstream `main`:
dependency bumps (gRPC, AWS SDK, ClickHouse; k6 and pgx unchanged), CI,
untimed configuration capture (every `paradedb.*` setting, two-column
queries), dashboard ordering, and the ParadeDB I/O counters selecting the
`paradedb` access method instead of `bm25`. None touches timing, query
forms or the Stannum and PostgreSQL I/O counters, and `ps-mods` lacks the
`datasets` workloads and the traces with TIN's forms altogether.
Re-pinning would lose the trace and gain nothing measured.

Our adapter patch adds two driver options beside the Stannum backend:
`STANNUM_SCORE_FUNCTION` (`score` or `full_score`) and, for the optional
local ParadeDB calibration, `PARADEDB_QUERY_FORM` (`operators` for `|||`,
`&&&`, `###`; the upstream `parse` form, `@@@ pdb.parse`/`pdb.match`, stays
the default). Drivers prepared before this change are refused; prepare a
fresh one.

### Optional local calibration against ParadeDB 0.26.0

PlanetScale's published ParadeDB numbers are taken as published. Locally,
and only there, `v2.py campaign --paradedb-database DIR` adds one ParadeDB
0.26.0 run per scenario, under the same v2 setup, as a one-off calibration
anchor: our ParadeDB QPS / their ParadeDB QPS is a per-scenario hardware
factor against their i8g table (the closer architecture to this Mac) and
their i7i table, and the report scales Stannum's QPS by it. That is a rough
approximation: it holds only if both engines move alike from their machine
to ours. The report shows how far the factor moves across the four
scenarios (min, max, max/min, coefficient of variation): a factor that holds
still is worth something, one that swings is not. AWS runs never include
ParadeDB.

The official image is multi-arch, with a native arm64 build, so nothing is
built from source. It runs as the benchmarker starts it: the image's own
bootstrap and auto-tuning, under the same overrides.

`v2.py report CAMPAIGN` writes `v2-report.md` and `v2-report.json`: per
scenario, Stannum (score and full_score) with QPS, p99 and MiB/query beside
the published TIN, TIN_FULL and ParadeDB figures for i8g and i7i, then the
calibration if ParadeDB ran.

### Local v2 campaign (this Mac, 150M)

The builds take hours; schedule them deliberately. The VM has 100 GiB, which
holds a 64g container; nothing needs resizing.

```sh
export PATH=/opt/homebrew/opt/postgresql@18/bin:/opt/homebrew/bin:$PATH
LAB=/Users/uri/stannum-lab/v2-150m
DATASET="$HOME/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange"
git fetch stannum
# The harness branch is perf-integrate2 plus harness changes: its extension sources
# are perf-integrate2's, so its image is perf-integrate2's Stannum.
SHA=$(git rev-parse --short stannum/bench/tin106-alignment)
SRC=$LAB/src-$SHA
git worktree add --detach "$SRC" "$SHA"
cd "$SRC"
python3 benchmarks/tin.py --driver "$LAB/driver" prepare     # a fresh driver: the adapter patch changed
python3 benchmarks/tin.py --driver "$LAB/driver" build
python3 benchmarks/tin.py build-image --image "stannum-bench:v2-$SHA" --output "$LAB/image-$SHA"
export STANNUM_DRIVER=$LAB/driver STANNUM_DATASET="$DATASET" STANNUM_SOURCE=$LAB/image-$SHA/source.json

# Build and save the Stannum database (v2 sizing: 64g, 8 pinned CPUs, 8 maintenance workers).
STANNUM_MOCK=$LAB/stannum bash benchmarks/local/mock-build.sh 150m "stannum-bench:v2-$SHA"
# Optional: the ParadeDB 0.26.0 calibration database.
STANNUM_MOCK=$LAB/paradedb STANNUM_ENGINE=paradedb bash benchmarks/local/mock-build.sh 150m "stannum-bench:v2-$SHA"
rm -f "$LAB"/stannum/build/*/input.csv "$LAB"/paradedb/build/*/input.csv    # 79 GiB each

# Every scenario x {score, full_score} (and ParadeDB, if built), NVMe read caps, the VM
# page cache dropped as each measurement starts; then the report.
STANNUM_DOCKER_RUN_ARGS="--device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb" \
python3 benchmarks/v2.py campaign --driver "$LAB/driver" \
  --image "stannum-bench:v2-$SHA" --source-manifest "$LAB/image-$SHA/source.json" \
  --database "$LAB/stannum/db" --paradedb-database "$LAB/paradedb/db" \
  --output "$LAB/campaign-$SHA" --drop-caches
```

Add `--dry-run` to the campaign to print every server and driver command
first; leave out `--paradedb-database` to skip the calibration. A single run:
`STANNUM_SCORE_FUNCTION=full_score` (or `STANNUM_ENGINE=paradedb`) with
`workload.sh 150m ...`.

Estimates, from the local TNS1 build and 1M-row builds of both engines:

| | Stannum | ParadeDB 0.26.0 (optional) |
| --- | --- | --- |
| Import | 12 min | 12 min |
| Index build | about 2.5 h (TNS1 took 2 h 29 min; 1M rows 34 s) | about 0.5 to 2 h (1M rows: 3.8 s into 8 segments; merges grow with size) |
| VACUUM, checks, save | about 1 h | about 1 h |
| Saved database | 106 GiB (heap 69 GiB + index 35 GiB) | about 135 GiB (heap 69 GiB + index about 63 GiB, the post's 67.3 GB) |
| Peak disk during the build | about 300 GiB (CSV prefix 79 GiB + volume + saved copy) | about 360 GiB |
| Campaign | 8 runs x about 25 min (copy, checks, 10 min measured) | 4 runs x about 25 min |

Free space under /Users/uri/stannum-lab was 2.2 TiB on 2026-10-09. Run the
builds one after the other; each campaign run copies its database into a
fresh volume (up to 135 GiB more) and removes it afterwards. If ParadeDB's
build needs more than its 64 GB, it fails with the container's
`memory.events` in `after-build-cgroup.json`; record that as a deviation
rather than raising the limit.

### AWS v2 session (not yet run)

One short, decisive session, to confirm that the local results hold on the
published hardware. It never runs ParadeDB. `benchmarks/aws/v2-campaign.sh`
does the whole session on the host: driver, native image, database, every
scenario and scoring back to back, the optional x86-64-v3 and v4 builds
(built while the database is restored, measured last, `score` on one CPU
reading by default), evidence uploaded to the stack's artifact bucket after
each campaign. It is rerunnable, and `DRY_RUN=1` prints the sequence.

**The database: restore a short-lived snapshot of the local build.** Building
on the instance costs about 5 instance-hours (corpus download, import, a 2.5
to 3.5 hour index build, VACUUM and checks): about $15. Uploading the local
v2 database (106 GiB, about 114 GB) to S3 just before the session and deleting
it right after costs cents: S3 Standard is $0.023 per GB-month, about $2.60 a
month for this archive, so about $0.30 for a few days (transfer into S3 and
from S3 to an instance in the same region is free; the upload's requests cost
under a cent). A restore takes about 6 to 15 minutes, and with a restored
database the runs need no corpus (`tin.py` takes the corpus identity from the
saved database), which also skips the 85 GB download. Earlier sessions (r6 to
r8) restored a database built on this Mac onto the i7i: PostgreSQL's files
moved between little-endian 64-bit Linux hosts without trouble, though that
is not a supported guarantee. If the restore is refused, the fallback is the
build on the instance's NVMe (leave `STANNUM_SNAPSHOT` unset). The bucket's
30-day expiry rule bounds a forgotten snapshot at about $2.60.

Cost estimate, i7i.8xlarge at about $3.02 an hour (us-east-1, on demand):

| Phase | Snapshot (recommended) | Build on the instance |
| --- | ---: | ---: |
| Provision and bootstrap (stack, packages, NVMe) | 0.2 h | 0.2 h |
| Driver and baseline image (restore runs alongside) | 0.4 h | 0.4 h |
| Corpus download and verification | none | 0.6 h |
| Import | none | 0.2 h |
| Index build | none | 2.5 to 3.5 h |
| VACUUM ANALYZE, checks, save | none | 1.0 to 1.5 h |
| Scenario runs, 4 scenarios x 2 scorings, about 0.33 h each (copy, checks, 10 s + 600 s), one CPU reading | 2.7 h | 2.7 h |
| Second CPU reading (distinct cores), 8 more runs | 2.7 h | 2.7 h |
| Export and teardown | 0.2 h | 0.2 h |
| **x86 session, one reading** | **3.5 h, about $11** | **8 to 9 h, about $25** |
| **x86 session, both readings** | **6.2 h, about $19** | **11 to 12 h, about $35** |
| Optional x86-64-v3 and v4 (images built during the restore; 4 runs each, `score`, one reading) | +2.7 h, about $8 | +2.7 h, about $8 |
| S3 | snapshot, about $0.30 if deleted within days | none |

`V2_SECONDS=300` halves the measured time and saves about 0.7 h per reading.
ARM is an optional extra: an i8g.8xlarge session from the same snapshot runs
one reading only (Graviton4 has no SMT), about 3.5 h; at about $2.7 an hour
(check current pricing) that is about $10. Set `LifetimeHours` (default 6)
to the session's estimate plus an hour: 5 for one reading, 8 for both, 10
with v3 and v4, 13 for a build on the instance.

x86, i7i.8xlarge:

```sh
SHA=<full commit SHA>; CORPUS=springbird-dev-stannum-corpus-cache-860510875764
SNAP=postgres-snapshots/stackexchange-150m-v2-${SHA:0:7}
# 1. From this Mac, just before: upload the local v2 database (about 40 minutes).
tar -C /Users/uri/stannum-lab/v2-150m/stannum/db -cf - . | \
  aws --profile springbird-development s3 cp - "s3://$CORPUS/$SNAP.tar" --expected-size 120000000000
# 2. The stack, with a short lifetime.
aws --profile springbird-development --region us-east-1 cloudformation create-stack \
  --stack-name stannum-v2-x86-$(date +%Y%m%d) --template-body file://benchmarks/aws/stack.json \
  --capabilities CAPABILITY_IAM --parameters \
  ParameterKey=InstanceType,ParameterValue=i7i.8xlarge \
  ParameterKey=Ami,ParameterValue=/aws/service/canonical/ubuntu/server/noble/stable/current/amd64/hvm/ebs-gp3/ami-id \
  ParameterKey=AvailabilityZone,ParameterValue=us-east-1a ParameterKey=LifetimeHours,ParameterValue=8 \
  ParameterKey=DiskGiB,ParameterValue=80 ParameterKey=CorpusBucket,ParameterValue=$CORPUS
# 3. On the host (SSM; setsid nohup ... < /dev/null & with a long executionTimeout, as
#    in the r5 to r8 runbook), once /opt/stannum-benchmark/bootstrap-ready exists:
git clone https://github.com/TeamSpringbird/stannum.git /opt/stannum-benchmark/repo
cd /opt/stannum-benchmark/repo && git checkout --detach "$SHA"
python3 -m venv --system-site-packages /opt/stannum-benchmark/venv
/opt/stannum-benchmark/venv/bin/pip install 'psycopg[binary]==3.3.6'
export PATH=/opt/stannum-benchmark/venv/bin:$PATH
BENCH_BUCKET=<ArtifactBucket output> STANNUM_SNAPSHOT="s3://$CORPUS/$SNAP.tar" \
  STANNUM_TARGET_CPUS="x86-64-v3 x86-64-v4" bash benchmarks/aws/v2-campaign.sh
# 4. From this Mac: fetch the evidence, then clean up (below).
aws --profile springbird-development s3 sync s3://<ArtifactBucket output> ./aws-v2-export
```

ARM, i8g.8xlarge: the same with
`ParameterKey=InstanceType,ParameterValue=i8g.8xlarge`, the default (arm64)
`Ami`, `LifetimeHours` 5 and no `STANNUM_TARGET_CPUS`. The image builds on
`postgres:18-trixie` (the Dockerfile's ParadeDB base image is amd64-only)
with the same pinned PostgreSQL 18.6 packages. Check that the zone offers
i8g.8xlarge first (`aws ec2 describe-instance-type-offerings
--location-type availability-zone --filters
Name=instance-type,Values=i8g.8xlarge`).

**Cleanup**, right after the evidence is safe:

```sh
aws --profile springbird-development s3 rm --recursive s3://<ArtifactBucket output>
aws --profile springbird-development --region us-east-1 cloudformation delete-stack --stack-name <stack>
aws --profile springbird-development s3 rm "s3://$CORPUS/$SNAP.tar"
# Confirm: the instance terminated and its volumes gone, no stack left, and only what
# is still wanted under s3://$CORPUS.
aws --profile springbird-development s3 ls "s3://$CORPUS" --recursive --summarize
```

**What is in S3 now** (listed read-only on 2026-10-09; nothing deleted).
The corpus-cache bucket `springbird-dev-stannum-corpus-cache-860510875764`
holds 557.6 GiB in 16 objects, about $13.80 a month, under a 30-day expiry
rule, and no Stannum stack is running:

| Object | Size | Uploaded | Use now |
| --- | ---: | --- | --- |
| `postgres-snapshots/stackexchange-150m-lsg5-7372fc6.tar` (+ .json) | 300.1 GiB | 2026-09-23 | none: LSG5 format, unreadable by current builds |
| `postgres-snapshots/stackexchange-150m-stn3-b961ded.tar` (+ .json) | 117.1 GiB | 2026-09-24 | none: STN3, pre-36355bc meta layout |
| `postgres-snapshots/stackexchange-150m-stn3-f099667.tar` (+ .json) | 117.1 GiB | 2026-09-25 | none: STN3, unreadable by TNS1 builds |
| `postgres-snapshots/wikipedia-bba903d-20260921-r4/` | 13.2 GiB | 2026-09-20 | none for v2 (an old Wikipedia COUNT database) |
| `f487fba…/wikipedia/` (corpus cache) | 10.0 GiB | 2026-09-20 | the Wikipedia COUNT crossover only |

All of it expires on its own by 2026-10-25 (about $6 more if left). Deleting
the three Stack Exchange snapshots now saves most of that; that is the
owner's call.
