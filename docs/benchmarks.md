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
architecture, `benchmarks/pgo/<arch>.profdata` (`uname -m`: `aarch64` and
`x86_64`; an x86-64-v4 build takes `x86_64-v4.profdata` when there is one,
else `x86_64.profdata`). An architecture without a profile builds as
without PGO, with a warning. `--pgo strict` fails instead, and also fails on a stale
profile (below). Without `--pgo`, builds are as before. A profile changes
code layout, inlining and branch weights only; answers and formats do not
change.

Measured at 43d99b1 on the local 150M database (8 clients, the first 400
records), with `benchmarks/local/vm_ab.py`, a profile trained on the other
854 records cut the VM's instructions per query by 13% for conjunctions, 4
to 5% for disjunctions and 11 to 12% for phrases. Cycles per query fell 3
to 8%, and on the mixed trace 6%. Two runs of one binary differ by up to 2%
in instructions and 3% in cycles. The committed profile, trained on the
whole trace, gains the same. Offline, the replay's own profile gains more
(11 to 20% of instructions), because the replay spends less of its time in
PostgreSQL's buffer manager.

The profile has to be trained by the extension itself, in PostgreSQL. The
offline replay (`bench --bin tnsreplay`) cannot stand in: its hot walk is
`Walk<T>` instantiated over the replay's page type, and its `engine` is
built with other features. So every function the extension compiles has
another symbol, and no record from a replay profile applies (0 of the
extension's 1,041 records). A replay profile does speed the replay up,
which is what `script/pgo-build replay` is for.

**Retraining.** Retrain after changes to the walk (`rank.rs`, `walk.rs`),
the decoders or the toolchain. Do it on a host of the profile's
architecture, with a copy of a saved 150M database (`--save-database`; the
server writes to it):

```sh
STANNUM_PYTHON=/path/to/python-with-psycopg \
  script/pgo-build image --data COPY_OF_DB --trace trace.tsv
python3 benchmarks/tin.py build-image --pgo strict --image TAG --output DIR ...
```

`trace.tsv` is `script/replay-oracle.py trace` over the published queries.
`pgo-build image` builds the instrumented image (`--pgo generate`) and runs
every query of the trace through the benchmark's ranked statement, once
with `stannum.score` and once with `stannum.full_score`. It then stops the
server so that each backend writes its counts, and merges them with the
toolchain's `llvm-profdata` (`rustup component add llvm-tools`) into
`benchmarks/pgo/<arch>.profdata`. Beside it goes `<arch>.hot.txt`, the
profile's 40 hottest functions. The run takes about 15 minutes. Commit both
files with the change that made the old ones stale.

An x86-64 profile is trained the same way on an x86-64 host: the AWS
benchmark instance, with the saved database. The committed `x86_64` profile
was trained on an i7i.8xlarge (Xeon Platinum 8559C) with a restored copy of
the local 150M database; the training took about 10 minutes there. An emulated run (an amd64
image under Rosetta) records the same branch counts, since counts do not
depend on speed, except in the AVX-512 kernels, which Rosetta does not
run. Such a profile is good for checking function coverage, but not as the
measured one.

**Staleness.** Each function's counts are keyed by its symbol and a hash of
its control flow. A function whose code changed, or whose symbol changed (a
new crate hash after a dependency, feature or toolchain change), silently
loses its profile. The build stays correct, and that function is optimized
as without PGO. So every `--pgo` build runs `script/pgo-check`, which
checks the hot list against the built library's symbols and the compiler's
hash-mismatch warnings. It prints `PGO profile: N of 40 hot functions
matched`, then each stale one as `changed` or `gone`. With `--pgo strict`,
any stale function fails the build. Use strict for measured images, and
retrain when it fails. The merge is `--sparse`, so code the training never
ran (counts, writes, VACUUM) has no profile at all, rather than one that
marks it cold.

## BOLT and CPU levels

Two more build-only options were measured on x86-64, both without source
changes, on the 150M offline replay: post-link layout with LLVM BOLT
(`script/bolt-build`), and builds for the x86-64-v3 and x86-64-v4 CPU levels
(which would ship as one source and a small loader choosing the library).
BOLT gains nothing beyond PGO and is not used. v3 and v4 gained 1.4% and
2.9% beyond PGO in single-thread cycles, first judged below what a loader
and three libraries would cost. The hyperthread-paired gate later measured
v4 at +3.0% of a core's throughput (2026-10-16), and x86-64 Linux now ships
the baseline and v4 with the loader ([releasing](RELEASING.md#x86-64-builds-and-the-loader));
v3 is not built.

Measured on 2026-10-12 at 1e01d81 on one r7i.2xlarge (Xeon 8488C, Sapphire Rapids), with
`benchmarks/aws/devbox-bolt-variants.sh` (`devbox.sh --dump --on-demand`;
the box costs about $0.60 for its hour). Each build is tnsreplay over the
first 400 records of each style, three timed passes each, two rounds with
the order reversed in round 2. The counters are user-space `perf stat`
minus a load-only run of the same build. Every replay answered all 1188
queries exactly (`--expect`). The two rounds agree within 0.8% in cycles
for every build and within 0.4% in time for all but v3 (up to 1.5%).

| build | instructions | cycles | IPC | branch misses | L1i misses | iTLB misses | conj ms | disj ms | phrase ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| plain | 1428.8 G | 470.5 G | 3.04 | 3452 M | 1219 M | 3.1 M | 9.02 | 40.48 | 13.18 |
| PGO | 1331.2 G | 440.6 G | 3.02 | 3457 M | 549 M | 4.0 M | 8.49 | 37.84 | 12.43 |
| v3 + PGO | 1251.6 G | 433.5 G | 2.89 | 3465 M | 565 M | 4.0 M | 8.37 | 37.40 | 12.17 |
| v4 + PGO | 1225.2 G | 427.8 G | 2.86 | 3406 M | 561 M | 2.7 M | 8.44 | 36.30 | 12.29 |
| PGO + BOLT | 1325.5 G | 440.3 G | 3.01 | 3529 M | 378 M | 9.0 M | 8.52 | 37.87 | 12.34 |

PGO is the replay's own profile (`script/pgo-build replay` does the same).
The committed `benchmarks/pgo/x86_64.profdata` is the extension's, and its
records do not apply to the replay (see above). Each CPU level trained its
own profile, because a target CPU changes which code `cfg(target_feature)`
compiles, on the same 1200 records the replay then measured (BOLT too). Against plain, PGO cuts 6.3% of cycles and 6 to 6.5% of each
style's time, as on the i7i.

**BOLT** (22.1.8, matching rustc 1.96's LLVM 22; `-reorder-blocks=ext-tsp
-reorder-functions=cdsort -split-functions -split-all-cold -split-eh
-icf=all`, applied to the PGO build linked with `--emit-relocs`). The VM
exposes no LBR, so the profile came from BOLT's instrumentation, which also
works in containers. BOLT laid 298 functions out again, moved 436 KB of hot
code together and 314 KB of cold code out, and cut taken branches by 22%
and L1i misses by 31%. Cycles moved by 0.1% and times by -0.7 to +0.4%:
the replay is not bound by its instruction cache. With PGO it misses 0.4
times per thousand instructions. On the i7i, the extension inside
PostgreSQL misses 2 to 4 times per thousand, but instruction cache stalls
there take only about 1% of cycles. Most of its 22% frontend share is
elsewhere, and BOLT on `stannum.so` would not touch PostgreSQL's own code.
So BOLT was not tried on the extension. If it ever is, nothing changes for a
pgrx cdylib: link with `-C link-arg=-Wl,--emit-relocs`, then
`script/bolt-build stannum.so -- <training>`. The image build would need two
passes, as for PGO: an instrumented image whose backends each write a
profile, a training run on the saved database, then `llvm-bolt` with the
merged profile. Unlike a PGO profile, a BOLT profile belongs to one exact
binary, so it would have to be trained and applied in the same build rather
than committed.

**CPU levels.** v4 + PGO cuts 2.9% of cycles beyond PGO (disjunctions 4.1%,
conjunctions 0.6%, phrases 1.1%), and v3 + PGO cuts 1.6% (1.2 to 2.1%). They
run 6 to 8% fewer instructions at a lower IPC. The +13.5% that v4 once
showed was the branchy f32 select that `or_zero` has since fixed for every
level. In the server, a quarter to a third of the cycles are PostgreSQL's
and the kernel's, which these builds do not change, so v4 would gain about
2% there. A loader makes that portable, and was first judged not worth 2%. The
paired gate (two hyperthreads of one core busy, as a server's backends
are) measured v4 at +3.0% (five of six styles at +2% or more), and so the
loader now ships, with two builds rather than three: `stannum.so` is a
small C library that reads CPUID in `_PG_init`, `dlopen`s
`stannum-x86-64-v4.so` or `stannum-x86-64.so` and calls its `_PG_init`,
and forwards by trampoline every symbol PostgreSQL resolves by name
(generated from the build's `nm -D`). See
[releasing](RELEASING.md#x86-64-builds-and-the-loader).

Measured again on 2026-10-16 with the paired gate (one m7i.4xlarge, Xeon
8488C, the layout-15 dump, three rotated rounds, non-PGO on every side; run
`20261016-030601-575d`, $0.20 with the loader's checks). The v4 build as
shipped compiles the workspace's crates for v4 and leaves the dependencies'
own code at the baseline; a build of every crate for v4 was measured
beside it:

| style | v4 build (shipped) | every crate v4 | instructions |
| --- | ---: | ---: | ---: |
| conjunction | +0.1% | +0.4% | -2.1% |
| phrase | +0.9% | +1.2% | -3.2% |
| disjunction | +0.4% | +1.9% | -5.6% |
| conjunction, full | +2.3% | +2.8% | -5.2% |
| phrase, full | +2.1% | +2.3% | -5.1% |
| disjunction, full | +4.7% | +6.0% | -9.6% |
| geomean | **+1.7%** | +2.4% | |

Paired queries per gigacycle against the baseline build, the median of the
per-round ratios. The two v4 builds run the same instructions on every
style, so the 0.65 points between them are code layout, not the
dependencies' code. The shipped arm was built through a
`RUSTC_WORKSPACE_WRAPPER`, which renames every function (see
[releasing](RELEASING.md#x86-64-builds-and-the-loader)); the release
build's `RUSTC_WRAPPER` compiles the same crates for v4 under the
baseline's names, and so lays them out otherwise again. In the server, the
build is also profile-guided, which decides the layout of the hot code.

## The hyperthread-paired replay

A server's backends share each core with a sibling hyperthread. In the
150M server run at layout 14, a query ran about as many instructions as in
the offline replay (0.88 to 1.03 times), but 1.69 to 2.05 times its cycles
per thread: the core retired about 3.0 to 3.4 instructions a cycle for its
two threads, against 2.8 to 3.2 for the replay's one. So the server's
throughput follows instructions, and a single-thread replay A/B
under-credits a change that removes instructions without removing cycles
(cheap, high-IPC work the sibling would have used) and over-credits one that
only hides latency (stalls the sibling would have filled).

`tnsreplay --cpus C/C,S` measures the core instead: over one load of the
dump, the plan's slices run on CPU C alone, then on C and its sibling S at
once, a pinned thread each with its own segment readers, each thread over
the whole slice from its own offset. Each query takes an untimed pass, then
`--repeat` timed ones (`--forget` before each), counted by the thread's own
`perf_event_open` group. A thread done with its slice keeps replaying it,
uncounted, until its sibling is done too, so every counted query ran beside
a busy sibling. Per slice it prints instructions, cycles and branch misses
per query, the core's IPC, its queries per gigacycle (each thread's queries
over its own cycles, summed: the core's throughput at a fixed clock) and a
digest of the answers, which every set and build of a correct walk shares.
A set can be any list of CPUs (`2,10,3,11,4,12,5,13`: four cores' eight
hyperthreads, as the i7i server runs eight backends). Sibling CPUs are in
`/sys/devices/system/cpu/cpuN/topology/thread_siblings_list`; pinning and
per-thread counts are Linux only.

`benchmarks/aws/devbox-htpair-ab.sh` runs it for several builds on one
devbox: each arm a tree (`--branch`), a dump directory and optionally an
x86-64 level for the whole build (through
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS`, which keeps the link
flag a plain `RUSTFLAGS` drops), built in its own target directory; one
process per arm and round, rounds rotated and reversed in turn;
`benchmarks/aws/htpair_summary.py` then prints the pair-vs-single table per
arm and, per compared pair, the paired core's throughput change beside the
single thread's cycles and instructions.

```sh
export AWS_PROFILE=springbird-development DEVBOX_DUMP_PREFIX=replay-dumps/<prefix>/
nohup benchmarks/aws/devbox.sh --branch BASE --branch CANDIDATE --dump --disk 120 --hours 2 \
  --type m7i.4xlarge --spot-only --env ARMS=A:1:dump150m,B:2:dump150m,Bv4:2:dump150m:v4 \
  --env COMPARE=B:A,Bv4:B --cmd-file benchmarks/aws/devbox-htpair-ab.sh > devbox.log 2>&1 &
```

`benchmarks/aws/htpair_gate.py OUT --compare B:A [--target B=conjunction/phrase]`
is the keep gate over the same files. Per style it takes the median of the
per-round ratios (a round's arm and base run close together, while the
host drifts up to 4% between rounds), for the paired core and the single
thread. A build is kept if no style's paired change is below −0.5%, the
answers agree, and either the geomean over the six styles is at least
+1.5% with every round's geomean above zero, or one style the change aims
at gains at least +2.0% with every round at least +1.0% and the geomean is
at least +0.5%. In the first session (nine arms, three rounds, 2026-10-16)
the paired change followed the single thread's closely (slope 0.89,
correlation 0.93 over 42 arm-styles; against the instruction cut, 0.86),
and it was about a third quieter between rounds (range 1.0 against 1.5
points). Cutting cheap, high-IPC work gained 1–2.5 points more in the pair
than alone on conjunctions, but no candidate that the single-thread gate
had rejected became a keep on the strength of that alone.

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
