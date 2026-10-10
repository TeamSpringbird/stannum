# Benchmarks

Stannum is measured on the workloads PlanetScale published for TIN, at a scale
where the index does not fit in memory. Every run also checks its answers: a
faster query that changes the result is not an improvement.

## Benchmarks v2

From 2026-10-09 the reference setup is **benchmarks v2**: TIN v1.0.6's
published setup (8 pinned vCPUs, 64 GB), with the four published scenarios,
Stannum's default and full scoring, and ParadeDB 0.26.0 measured beside it as
a calibration anchor. It is the default for every 150M run
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
| ParadeDB | `paradedb/paradedb:0.26.0-pg18@sha256:52fc9c95…` (multi-arch; PostgreSQL 18.6 inside); `body &&& $1`, `body ||| $1`, `body ### $1` on the query's plain text, `pdb.score(id)`; index `USING bm25 (id, body) WITH (key_field=id, target_segment_count=8)` | the final 0.26.0, `|||`, `&&&`, `###`; **image and index flags not stated** | post; the benchmarker's `post.sql` |
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
  leaves JIT on. Our ParadeDB runs keep exactly that. Stannum keeps the
  harness's `work_mem=16MB` and `jit=off`, which every earlier run used: a
  ranked scan's plan cost can cross `jit_above_cost`, and TIN's own image and
  configuration are not public. Recorded in each manifest's `docker_run`.
- **ParadeDB's index flags.** The post says neither. We use the
  benchmarker's `post.sql` verbatim (`key_field` is a no-op since 0.26.0;
  `bm25` is 0.26.0's alias of the `paradedb` access method and the name the
  pinned driver's I/O counters select).
- **ParadeDB's build memory.** 64 GB for the build, as the benchmarker's
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
`STANNUM_SCORE_FUNCTION` (`score` or `full_score`) and `PARADEDB_QUERY_FORM`
(`operators` for `|||`, `&&&`, `###`; the upstream `parse` form,
`@@@ pdb.parse`/`pdb.match`, stays the default). Drivers prepared before
this change are refused; prepare a fresh one.

### Calibration

ParadeDB 0.26.0 runs on our hardware under the same setup. For each
scenario, our ParadeDB QPS / PlanetScale's published ParadeDB QPS is a
hardware factor (separately against i7i and i8g), and Stannum's QPS / that
factor estimates Stannum on their machine. `v2.py report CAMPAIGN` writes
`v2-report.md` and `v2-report.json`: per scenario, Stannum (score and
full_score) and our ParadeDB with QPS, p99 and MiB/query, beside the
published TIN, TIN_FULL and ParadeDB on x86 and ARM, the factors and the
estimates. The factor is only as good as the assumption that both engines
scale alike from their machine to ours; compare the x86 and ARM factors for
a sense of it.

### Local v2 campaign (this Mac, 150M)

Both builds take hours; schedule them deliberately. The VM has 100 GiB, which
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

# Build and save each database (v2 sizing: 64g, 8 pinned CPUs, 8 maintenance workers).
STANNUM_MOCK=$LAB/stannum bash benchmarks/local/mock-build.sh 150m "stannum-bench:v2-$SHA"
STANNUM_MOCK=$LAB/paradedb STANNUM_ENGINE=paradedb bash benchmarks/local/mock-build.sh 150m "stannum-bench:v2-$SHA"
rm -f "$LAB"/stannum/build/*/input.csv "$LAB"/paradedb/build/*/input.csv    # 79 GiB each

# Every scenario x {Stannum score, Stannum full_score, ParadeDB}, NVMe read caps,
# the VM page cache dropped as each measurement starts; then the report.
STANNUM_DOCKER_RUN_ARGS="--device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb" \
python3 benchmarks/v2.py campaign --driver "$LAB/driver" --dataset "$DATASET" \
  --image "stannum-bench:v2-$SHA" --source-manifest "$LAB/image-$SHA/source.json" \
  --stannum-database "$LAB/stannum/db" --paradedb-database "$LAB/paradedb/db" \
  --output "$LAB/campaign-$SHA" --drop-caches
```

Add `--dry-run` to the campaign to print every server and driver command
first. A single run: `STANNUM_ENGINE=paradedb` or
`STANNUM_SCORE_FUNCTION=full_score` with `workload.sh 150m ...`.

Estimates, from the local TNS1 build and a 1M-row ParadeDB build:

| | Stannum | ParadeDB 0.26.0 |
| --- | --- | --- |
| Import | 12 min | 12 min |
| Index build | about 2.5 h (TNS1 took 2 h 29 min) | about 0.5 to 2 h (1M rows: 3.8 s, 8 segments; merges grow with size) |
| Saved database | 106 GiB (heap 69 GiB + index 35 GiB) | about 135 GiB (heap 69 GiB + index about 63 GiB) |
| Peak disk during the build | about 300 GiB (CSV prefix 79 GiB + volume + saved copy) | about 360 GiB |
| Campaign | 8 runs x about 30 min (copy, checks, 10 min measured) | 4 runs x about 30 min |

Free space under /Users/uri/stannum-lab was 2.2 TiB on 2026-10-09. Run the
builds one after the other; each campaign run copies its database into a
fresh volume (up to 135 GiB more) and removes it afterwards.

### AWS v2 campaign (not yet run)

The stack and the host script are ready; launching is the owner's call. On
both hosts `benchmarks/aws/v2-campaign.sh` prepares the driver, builds the
image natively, builds (or restores, `STANNUM_SNAPSHOT` / `PARADEDB_SNAPSHOT`)
both databases, runs the campaign and writes the report. `DRY_RUN=1` prints
the sequence. Launch the host script with `setsid nohup ... < /dev/null &`
and a long SSM `executionTimeout`, as in the r5 to r8 runbook.

x86, i7i.8xlarge (runtime-dispatch build, then x86-64-v3 and v4):

```sh
aws --profile springbird-development --region us-east-1 cloudformation create-stack \
  --stack-name stannum-v2-x86-$(date +%Y%m%d) --template-body file://benchmarks/aws/stack.json \
  --capabilities CAPABILITY_IAM --parameters \
  ParameterKey=InstanceType,ParameterValue=i7i.8xlarge \
  ParameterKey=Ami,ParameterValue=/aws/service/canonical/ubuntu/server/noble/stable/current/amd64/hvm/ebs-gp3/ami-id \
  ParameterKey=AvailabilityZone,ParameterValue=us-east-1a ParameterKey=LifetimeHours,ParameterValue=24 \
  ParameterKey=DiskGiB,ParameterValue=80 ParameterKey=CorpusBucket,ParameterValue=<corpus bucket>
# On the host (SSM), once /opt/stannum-benchmark/bootstrap-ready exists:
git clone https://github.com/TeamSpringbird/stannum.git /opt/stannum-benchmark/repo
cd /opt/stannum-benchmark/repo && git checkout --detach <full SHA>
python3 -m venv --system-site-packages /opt/stannum-benchmark/venv
/opt/stannum-benchmark/venv/bin/pip install 'psycopg[binary]==3.3.6'
export PATH=/opt/stannum-benchmark/venv/bin:$PATH
bash benchmarks/aws/v2-campaign.sh                                   # baseline: runtime SIMD dispatch
STANNUM_TARGET_CPUS="x86-64-v3 x86-64-v4" bash benchmarks/aws/v2-campaign.sh   # adds v3 and v4 (Stannum only)
```

ARM, i8g.8xlarge: the same with
`ParameterKey=InstanceType,ParameterValue=i8g.8xlarge`, the default (arm64)
`Ami`, and no `STANNUM_TARGET_CPUS`. Graviton4 has no SMT, so the two CPU
readings coincide and run once. The image builds on `postgres:18-trixie`
(the Dockerfile's ParadeDB base is amd64-only), with the same pinned
PostgreSQL 18.6 packages. Check that the zone offers i8g.8xlarge first
(`aws ec2 describe-instance-type-offerings --location-type availability-zone
--filters Name=instance-type,Values=i8g.8xlarge`).

Time: building both databases on the host adds about 4 to 7 hours; 24 runs
(two readings) at about 25 minutes each take about 10 hours, so a single
24-hour stack fits either the builds and one reading or both readings from
snapshots. Earlier runs restored a database built on this Mac onto the i7i
(r6 to r8): PostgreSQL's files are portable between little-endian 64-bit
Linux hosts in practice, though not a supported guarantee.
