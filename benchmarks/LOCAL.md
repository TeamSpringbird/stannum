# Local Mac Studio campaigns

Benchmarks run locally, not in CI. No results are uploaded. The recommended command
builds one Linux ARM64 image with all engines and runs them sequentially through
OrbStack/Docker on this Mac:

```sh
export PATH="$(brew --prefix postgresql@18)/bin:$PATH"
python3 benchmarks/campaign.py --build \
  --output benchmarks/results/mac-baseline-01 \
  --repetitions 5 --rows 10000 --seconds 30 --warmup 5 \
  --clients 2 --write-rate 20
```

The default campaign has 35 runs: five count trials for each of four engines, plus
five mixed count/ranked trials for Lead, ParadeDB and pg_textsearch. GIN is excluded
from BM25 ranking. A campaign with `--profiles count` has 20 runs. A short preflight
can use `--repetitions 1 --rows 1000 --seconds 5 --warmup 1`.

For an existing up-to-date image omit `--build`; the runner verifies that its
embedded Lead source fingerprint matches the checkout. Use a new output directory
for every campaign. Each run also gets a new database volume/container; the runner
removes only its own resources on completion or failure. Logs and results survive.
The image/build cache stays local for fast subsequent rebuilds.

## Controlled conditions

* Native `linux/arm64`, never x86 emulation. One pinned ParadeDB base supplies the
  exact same PostgreSQL 18.6 runtime for all engines. Lead is built with Rust 1.96.0
  and pgrx 0.19.1 in release mode. pg_textsearch remains the explicitly recorded
  development commit used by the validated adapter.
* Four virtual CPUs (`--cpus 4`, `--cpuset-cpus 0-3`), 4 GiB memory, no container
  swap, and 1 GiB shared-memory capacity per server. These limits include all
  PostgreSQL processes and engine workers. The memory limit also includes shared
  memory; `--shm-size` does not add a separate memory allowance.
* Data and WAL use fresh Docker named volumes inside the Linux VM, rather than a
  bind mount of a macOS directory. Results are written by the native host client.
* The same explicit PostgreSQL settings, including 1 GiB shared buffers, 16 MiB
  work memory, 512 MiB maintenance memory, and identical parallel-worker limits.
  fsync, synchronous commits, full-page writes and autovacuum stay enabled.
* The distribution's startup auto-tuner and unrelated extension bootstrap are
  removed. pg_search, pg_textsearch and pg_stat_statements are preloaded for every
  engine, so process startup is consistent. This is a common test distribution,
  not an assertion that every engine needs these other extensions in production.
* One server under test at a time; a lock prevents overlapping invocations of this
  campaign runner. Engine order rotates between rounds. Five rounds do not perfectly
  balance four positions, but avoid always running one engine first.
* Each round uses a different fixed seed, shared across engines within that round.
  Every run gets the same explicit warmup and fresh fixture. This is warm-cache
  measurement: neither host nor VM caches are forcibly purged. Warmup warms shared
  data caches; the timed pgbench process opens new backend sessions, so backend-local
  caches are not carried over from the warmup clients.
* `caffeinate -i` prevents idle sleep for the campaign's lifetime on macOS. Native
  macOS psql/pgbench use the same loopback TCP route for every engine. Client CPU
  is outside the server budget and is recorded separately.

Docker's CPU/memory controls impose ceilings; they do not reserve physical CPU
capacity. The cpuset identifies Linux VM CPUs, not guaranteed Mac performance
cores. Other containers, host jobs, thermal behavior and VM scheduling still
introduce variation. The runner records background container usage before each
trial and cgroup CPU throttling, memory pressure/OOM counters, limits and I/O
before/after timed traffic. It does not stop unrelated applications or change
OrbStack settings. [Docker resource controls](https://docs.docker.com/engine/containers/resource_constraints/),
[Docker volumes](https://docs.docker.com/engine/storage/volumes/).

For a deliberate baseline, avoid builds/downloads or other heavy work during the
run. Repeat the entire campaign if resource counters or dispersion show contention;
do not quietly remove whichever sample looks inconvenient. Default budgets fit the
observed 16-CPU/16-GiB OrbStack VM with headroom, but that headroom is shared with
other running services. The Mac's physical RAM size is not the Docker VM budget.

## Five-run summaries

`report.md` shows the median, trimmed mean, minimum, maximum and coefficient of
variation of **run-level throughput**. `aggregate.json` also retains all five values
and reports per-query p50/p95/p99 distributions across runs. The trimmed mean sorts
the five values, removes one minimum and one maximum, and averages the remaining
three. It is computed separately per metric. No raw run or transaction is deleted.

Use the median as the primary local trend and the trimmed mean as a second view.
Keep min/max and dispersion visible. This dampens the effect of one anomalous run
without pretending it never happened. It does not prove a small difference is real:
if improvement is similar to run-to-run spread, collect more repetitions and inspect
resource counters before drawing a conclusion.

Failed/missing runs make a group incomplete; they are not outliers to trim away.
Fewer than five samples produce no trimmed mean. A p99 without enough transactions
in every repetition remains unavailable. Never average raw transactions across
runs and call them independent experiments; the independent sample is the run.
Never average per-query p99 values into an overall workload p99.

```sh
python3 benchmarks/campaign.py --report-only --output benchmarks/results/mac-baseline-01
python3 benchmarks/run.py history benchmarks/results/mac-baseline-01 > benchmarks/results/mac-baseline-01/history.csv
python3 -m unittest discover -s benchmarks -p 'test_*.py'
```

Run IDs, seeds, raw artifacts and source/build identities remain available for
later commit-to-commit comparison. The summary table does not calculate ranked
cross-engine speedups: scoring/relevance equivalence has not been established.
The 10k-document synthetic baseline is for local iteration and detecting large
structural changes. It does not yet substitute for representative corpora, data
exceeding memory, or sustained insert/delete/compaction workloads.

## First recorded campaign

The first five-round campaign uses `--seconds 20 --warmup 3`, with other workload
arguments matching the example. Its workspace output is
`benchmarks/results/mac-studio-baseline-01`. Use those exact settings to compare
against it; the 30-second example above creates a different measurement cohort.
Its report and raw logs are retained together, including a snapshot of the protocol
scripts. A checksummed copy is archived under
`~/Library/Application Support/LeadBenchmarks/archives/` so deleting this worktree
does not delete the baseline. For future campaigns, `--output` may point directly
to a persistent directory outside the repository.

The 2026-09-17 campaign completed all 35 trials successfully. Run-level throughput
CV ranged from 1.6% to 6.8%; there were no observed memory-limit events, OOMs, or
CPU-quota throttling during timed traffic. Median achieved writes were about
19.4–19.5/s for the requested 20/s. Nine local measurement tests passed. These
results support using the setup to detect substantial changes; they do not establish
small percentage improvements or production-scale competitive performance.

The archive is `mac-studio-baseline-01-3fcf441ac7c3.tar.gz`, SHA-256
`940f775165f22fa0f0ec4a1dfd12e9523ef6fbf968c0ec9b5d6225b36015c792`.
Its immutable local image ID is
`sha256:028b8e940aa33846aeb537e9097532e76e42b58dc3a03b32b2f8cba453728532`.
The archive includes manifests, generated SQL, raw logs, plans, repeated-run
summaries and snapshots of the protocol scripts. A readable report and metadata
sidecar are stored next to it. All temporary campaign containers and volumes were
removed after their evidence was collected.

## Wikipedia baselines: 100,000 and 1,000,000 documents

`dataset.py` creates nested, immutable samples from the English November 2023
[Wikimedia Wikipedia dataset](https://huggingface.co/datasets/wikimedia/wikipedia),
pinned to revision `b04c8d1ceb2f5cd4588862100d08de323dccfbaa`. It downloads all 41
English Parquet shards (11.6 GB compressed), verifies their published SHA-256
checksums, and samples across all 6,407,814 articles using seed 1729. The first
100,000 sample identities form a subset of the million-document sample. Documents
have the same IDs and contents in both. This avoids selecting only early articles
or repeating short synthetic text to reach a target row count.

The preparation environment needs **pyarrow 23.0.1**; the benchmark runners remain
Python standard-library programs. Keep data and results outside temporary worktrees:

```sh
python3 -m venv benchmarks/results/data-venv
benchmarks/results/data-venv/bin/pip install pyarrow==23.0.1
benchmarks/results/data-venv/bin/python benchmarks/dataset.py \
  --output "$HOME/Library/Application Support/LeadBenchmarks/datasets"
```

Downloads can resume after interruption. Complete datasets are verified and reused.
If preparation stops while writing a dataset, the builder refuses to overwrite that
partial directory: inspect and move it aside before retrying; downloaded source
shards remain reusable. Allow disk space for sources, normalized CSVs, and fresh
index builds. No corpus or generated results are committed to Git or uploaded.

Each dataset contains:

* `documents.csv`: stable numeric ID and searchable body, loaded with streaming COPY.
* `attribution.csv`: original article ID, URL and title. Retain this with the corpus;
  source licensing is CC BY-SA 3.0 / GFDL as documented on its dataset card.
* `matches.csv`: independently computed exact document membership for every query.
* `manifest.json`: source revision/checksums, sampling and normalization rules,
  builder fingerprint, document lengths, query match counts and output checksums.

This is a **normalized English article workload**, not a test of raw multilingual
text. Bodies concatenate title and text, extract lowercase ASCII letter sequences,
keep at most 8,192 tokens, and append the controlled mutable token. This preserves
natural vocabulary/frequency variation while avoiding analyzer and positional-limit
differences across engines. Long articles are truncated; punctuation, digits and
non-ASCII letters are discarded/split. Article URLs/titles preserve attribution to
the originals. There are no relevance judgments; ranked validation checks result
membership, cardinality, finite scores and score ordering, not relevance or a shared
BM25 formula. Query labels such as `rare` are descriptive: consult actual per-corpus
match counts rather than assuming an exact selectivity.

Queries cover `history`, `telescope`, `quasar`, AND/OR, `united states`, `computer
science`, `quantum mechanics`, and definite misses. Every engine must pass the
independent exact-match oracle before and after traffic. Writers still toggle the
reserved suffix: content is rewritten and indexed, but tested match sets remain
constant. Inserts/deletes, changing match membership and natural search traffic are
future **separate workload versions**, not claimed capabilities of this baseline.

Run both scales sequentially with the existing verified ARM64 image:

```sh
export PATH="$(brew --prefix postgresql@18)/bin:$PATH"
caffeinate -i python3 benchmarks/baselines.py \
  --datasets "$HOME/Library/Application Support/LeadBenchmarks/datasets" \
  --output "$HOME/Library/Application Support/LeadBenchmarks/campaigns/wiki-v1"
```

Use a new output directory each time. `baselines.py` freezes harness scripts, engine
source provenance and the Docker image ID before running either scale. Each scale
has 35 fresh-container trials: five repetitions, 30 seconds warmup, two readers
and a requested 20 updates/second. Measurement windows are 300 seconds at 100,000
documents and 1,800 seconds at 1,000,000 documents, giving slow full-scan engines
more opportunity to cover the query mix. All engines use the same duration at
each size. Resource limits remain four
VM CPUs / 4 GiB per server. Nominal measured traffic alone takes about 20.4 hours for
both scales; loading, indexing, correctness checks, plans and slow queries add time.
Index creation and loading have no statement timeout. Individual query statements
have a generous 30-minute limit, explicitly recorded in PostgreSQL settings. A
query already running at the end of pgbench's interval can extend a trial; actual
elapsed time is used in throughput calculations.

A failed trial is retained and the campaign continues. Final container counters and
server logs are captured even for setup/query failures. Failed trials never become
successful zero-latency samples or disappear through trimming. `status.json` at the
series root and each campaign's `campaign.json` report progress; individual runner
and server logs explain failures. `report.md`, `aggregate.json`, and `history.csv`
are generated for each scale. An `incomplete` campaign is a capacity/correctness
observation, not a valid five-run speed comparison. Inspect it before making claims.

To detach from a terminal, redirect stdout/stderr to a durable log and use `nohup`
with the above caffeinate command. A detached job survives terminal closure, not a
reboot. The runner does not change system sleep settings or stop other applications.

### Prepared corpus v1

Both datasets were generated and checksummed locally on 2026-09-17. Normalized
searchable text, excluding the mutable suffix, totals 278,979,934 bytes at 100k and
2,803,277,365 bytes at 1m. These are input text sizes, not PostgreSQL heap/index sizes
or proof that the working set exceeds RAM. Database sizes are measured per trial.
The builder used Python 3.14.7 and pyarrow 23.0.1; `verification.json` alongside the
corpora records an independent cardinality, ID uniqueness and nested-body check.

The real-text 1,000-article adapter preflight passed all seven before/after match
set checks and all three before/after ranked membership/order checks. Its 15-second
measurement windows left some query types without samples for GIN, Lead mixed and
pg_textsearch mixed; those runs remain explicitly incomplete. Preflight throughput
is not a baseline (corpus preparation was running concurrently). Thirteen local
unit tests cover measurement integrity, corpus checksum rejection, normalization
and oracle membership rejection. No benchmark workflow was added to CI.

The full series launched locally in the background under
`~/Library/Application Support/LeadBenchmarks/campaigns/wikipedia-v1-20260917/`.
Its sibling `.log` and `.pid` files identify the detached process, and `status.json`
inside the series tracks the active scale. Final results are pending; do not treat
this launch record as a completed performance measurement. A second 60-second GIN
preflight using the updated runner completed with all query types sampled.

Completion notifications for this series are handled by a detached copy of
`benchmarks/notify.py`, stored as `completion-notifier.py` in the series directory.
It polls every 30 seconds and requests a macOS notification when the 100k campaign
ends and when the full series ends. Incomplete/failed results are labeled as such;
an unexpectedly stopped process gets a separate alert. `notifications.json` records
notification requests, not confirmed display. Focus mode and macOS notification
permissions control delivery. This does not send a message into the chat. The
watcher's PID and log are stored alongside the results.

## Paired original-versus-fork iteration

`paired.py` runs five original/fork pairs in alternating order, with fresh volumes,
identical settings and a shared random seed within each pair. Use immutable image
IDs and each image's saved source manifest; labels must match both the source and
the Docker recipe. Commit hashes alone cannot identify an uncommitted working tree,
so manifests also retain a content fingerprint and source patch.

```sh
python3 benchmarks/paired.py \
  --output benchmarks/results/paired-milestone-01 \
  --original-image ORIGINAL_IMAGE_ID --original-source ORIGINAL_SOURCE_JSON \
  --fork-image FORK_IMAGE_ID --fork-source FORK_SOURCE_JSON \
  --profile mixed --rows 10000 --repetitions 5 --seconds 30 --warmup 5
```

Run only when other benchmark traffic and heavy builds have stopped. If a long
baseline campaign is active, a supervisor must pause its launcher between trials,
wait for the current trial's client processes to finish, and resume afterward.
The paired runner's separate lock prevents two paired invocations; it does not
automatically pause other campaign launchers. An idle baseline container may remain
alive on its own port. The paired default port is 28919.

Aggregate speedups are withheld until every planned pair passes correctness,
comparison-contract and OOM checks. Reports retain per-query latency, read and write
throughput, build time, total index bytes, dispersion and resource counters. A 10k
synthetic result is an iteration check; use the frozen Wikipedia corpora to evaluate
scaling and comparison with other engines. Keep raw artifacts outside disposable
worktrees or archive them after completion.
