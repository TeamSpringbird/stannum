# Local benchmark based on TIN's published methodology

Status: initial driver integration implemented, September 19, 2026; the full
matrix below remains planned. See the [working entry point](published-trace.md).
See the [source audit](tin-benchmark-source-audit.md) for implementation details
and differences between the article and the public benchmark source.

## Decision

Use the published workload and resource-isolation approach locally before doing
more index optimization. Start with Stannum and native GIN, then add ParadeDB.
Keep our existing sustained-mutation tests as correctness and regression tests.
They answer different questions from a broad search-throughput benchmark.

No AWS machine is needed for this work. The inspected host has an Apple M4 Max,
16 logical CPUs, 128 GiB RAM, and approximately 3.1 TiB available disk. Docker
currently has about 15.7 GiB RAM available, so upstream's 32 GiB query containers
and 64 GiB build containers cannot be used unchanged. ARM results also cannot
establish AVX-512 performance or reproduce the article's absolute QPS.

## Available inputs

Our existing one-million-document Wikipedia dataset passed all manifest file
checksums on September 19. It contains 2,803,277,365 body bytes before suffixes.
Its documents.csv SHA-256 is
`fce2bf1c30c5450764dbc9d97d07a50d49f44bd9a5d05bcfafd063f8b37018a5`.
It lives beside the 100k dataset under
`~/Library/Application Support/LeadBenchmarks/datasets/`.

Use that corpus for an adapter smoke test. For the actual published Wikipedia
workload, use PlanetScale's prepared corpus and trace at
[datasets commit f487fba](https://github.com/planetscale/paradedb-benchmarker/tree/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86).
Their [source manifest](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/SOURCES.md)
describes 5,032,104 documents, an 8.09 GB CSV, 2.69 GB compressed download,
and 302 base query entries. Preserve intentional repeated queries. Our newer
Wikipedia sample has different normalization and is not the same corpus.

The public Stack Exchange trace is not demonstrably the exact trace used for
the article. Keep that distinction in reports; do not claim a reproduction of
published TIN results merely by checking out the current public files.

## Implementation order

1. **Fair GIN baseline and trace replay.** Add a stored generated `body_tsv`
   variant with the normal planner, matching the upstream configuration.
   Preserve the existing expression-index variant as a separately named case.
   Replay the pinned published queries through a Stannum adapter. Reuse the
   upstream driver where practical; don't silently replace its scheduling.
   Verify phrase/boolean membership on immutable data before timing. Stop on
   parse errors, unsupported queries, or membership disagreement.
2. **Read-only workload matrix.** Run exact counts and top-10 separately, split
   by conjunction, disjunction, and phrase, plus a declared mixed workload.
   Stannum/ParadeDB BM25 and GIN `ts_rank_cd` are different ranking objectives;
   report top-10 timings by engine without claiming equivalent relevance.
   Record plans for common/rare, short/long, and slow queries. Use exhaustive
   same-engine scoring checks on a bounded fixture for ranked correctness.
3. **Cache and concurrency sweep.** Run one engine at a time in a dedicated
   named Docker volume, initially at four CPUs with 4 GiB and 8 GiB memory
   budgets, swap disabled, and shared buffers at 1 GiB and 4 GiB respectively.
   These are starting configurations, not proven cache regimes. Measure index,
   heap, stored-vector and total sizes plus block reads and memory pressure.
   If both working sets fit, lower the budget or increase the corpus; do not
   infer disk pressure from shared-buffer size alone. Sweep 1, 2, 4, 8 readers.
4. **Concurrent updates and sustainable load.** First mirror upstream's
   update-only workload, recording offered, skipped, completed, failed, and
   affected-row rates. Then run our independent insert/update/delete churn
   workload with correctness checks. Do not mix the two into one headline.
   Find the highest offered rate that meets a declared latency target without
   growing backlog; a single offered rate is not a capacity measurement.
5. **Profile the measured bottleneck.** Attribute slow query families to
   posting decoding/intersection, scoring/sorting, heap visibility/rechecks,
   I/O, or merge/lock waits before choosing the next runtime change. Replay
   the same corpus and trace against the parent and candidate build.

## Measurement contract

- Freeze source, image, PostgreSQL/extension versions, corpus and trace hashes,
  settings, resource caps, and driver revision in each run manifest.
- Separate import, generated-vector preparation, index build, and finalization
  time; report their total as well as index-only time. Record peak build memory.
- Warm the selected workload before timing and name the warmup policy. Treat
  cgroup memory caps as controlled memory pressure, not proof of cold SSD I/O:
  the macOS host may still cache the VM disk. Do not flush host-wide caches.
- Run at least three independent repetitions with engine order rotated. Use
  60-second smoke runs, then ten-minute measured windows for the final matrix.
  Monitor background containers; don't stop unrelated services automatically.
- Report QPS, latency p50/p95/p99, failures/timeouts, actual update throughput,
  WAL bytes, index/table/total bytes, CPU throttling and memory/OOM events.
  Keep all failures in the report. Publish per-query-family and query-length
  breakdowns so aggregate averages don't conceal a common-term cliff.
- Distinguish client service latency from scheduled-arrival latency. Saturated
  closed-loop QPS and fixed-arrival queueing measurements are different tests.
- Label upstream's index block-hit-plus-read counter as **index block accesses**.
  It includes repeated accesses and shared-buffer hits. Report buffer misses,
  kernel/container I/O, and timing separately; none is automatically SSD bytes.
- Preserve existing exact-membership checks and mutation lifecycle tests.
  Keep expensive oracles outside read-only timed windows and disclose observer
  overhead when checks run concurrently with mutations.

## What this changes in our interpretation

The current ten-query, 100k-document mutation results show specific queueing
and recheck behavior, not general search-engine capacity. In particular, GIN
phrase rechecks can recompute `to_tsvector` in our expression-index setup.
The upstream [stored-vector baseline](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/postgres/pre.sql)
can avoid that repeated text conversion, at a storage and write cost that must
also be measured. It is the first comparison to fix before drawing stronger
conclusions about Stannum's advantage.

The existing `benchmarks/campaign.py` already supplies sequential containers,
CPU/memory limits, build fingerprints and cgroup counters. It needs published
trace support and configurable server settings; its ten fixed cases and
current GIN count-only adapter do not yet implement this protocol. Its Docker
images need rebuilding for current source. `benchmarks/tin.py` now wraps the
pinned upstream driver instead of expanding that older measurement stack.
The full protocol remains a work plan, not a claim that its whole matrix ran.

Only revisit remote x86 hardware after the local matrix identifies a specific
question requiring AVX-512, production-like NVMe behavior, or more capacity
than this machine can supply. Even then, TIN itself remains unavailable for
a local head-to-head comparison.

## Observed heap visibility in local trace runs

`tin.py run` requires PostgreSQL's `pg_visibility` extension and records actual
visibility-map counts in each job's `workload_state` and `workload-state.json`.
Snapshots run outside timing: after the setup `VACUUM ANALYZE`, immediately
before the driver (which owns warmup), and after the driver's stopped container
is restarted. Each records heap pages, all-visible/all-frozen pages, catalog
estimates, estimated live/dead tuples, mutation counters, maintenance counters,
and table options. These are observations of the documents heap; they do not
measure index-internal dead entries or TOAST visibility. Tuple statistics are
estimates and may lag recent activity.

Repeated comparisons require this evidence and identical initial heap and
visibility coverage. They withhold aggregate ratios if coverage changed during
untimed validation, or if a read-only trial's before-driver and post-restart
coverage differ. Older artifacts remain readable as individual reports, but
cannot satisfy this stronger paired comparison contract. A missing extension
fails setup rather than silently substituting the potentially stale
`pg_class.relallvisible` estimate.

Mutation runs retain their observed final states without requiring equal final
visibility: completed updates and maintenance are outcomes of those runs.
Autovacuum remains governed by the recorded server/table settings. Shutdown,
restart, and warmup lie between the captured boundaries, so these snapshots do
not prove unchanging visibility throughout the timed window. They also do not
establish a cold cache or a pristine heap. Controlled dirty/deletion-heavy
fixtures and exact timed-boundary capture remain follow-up work.
