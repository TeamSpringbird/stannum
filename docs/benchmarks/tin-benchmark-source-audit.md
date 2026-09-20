# TIN benchmark source audit

Audited September 19, 2026. This records the public driver and dataset sources,
not a reproduction of PlanetScale's published results. No corpus archives were
downloaded and no cloud resources were created for this audit.

## Reproducible source identities

The [announcement](https://planetscale.com/blog/introducing-tin#tin-performance-and-benchmarking)
links to PlanetScale's ParadeDB Benchmarker fork. Its default `main` branch at
`87cdf7700c8532b07b01b72d880d31c9a072582a` does **not** contain the relevant
PlanetScale modifications. Those are currently on two open branches:

| Branch | Audited commit | Purpose |
| --- | --- | --- |
| `ps-mods` | `fbad65cb0d1409dd43a514d86d4c89dbc554f3a1` | Phase coordination, warm-up, I/O/WAL diagnostics ([PR 1](https://github.com/planetscale/paradedb-benchmarker/pull/1)) |
| `datasets` | `f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86` | Includes modifications, prepared corpora, query traces, runner ([PR 2](https://github.com/planetscale/paradedb-benchmarker/pull/2)) |

All source links below pin the audited `datasets` commit. Recheck upstream
before adopting it; the article does not identify the exact commit or output
artifact underlying its charts.

## What is actually available

The public tree contains both corpora as Git LFS pointers, full query JSON,
per-part checksums, corpus manifests, source descriptions, and a shared runner.
Skip LFS smudging when cloning for source inspection. Compressed parts are
consecutive pieces of one gzip stream, not independently decompressible
archives. [Sources and manifests](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/SOURCES.md)

| Corpus | Rows | Uncompressed CSV | Compressed download | Base query records |
| --- | ---: | ---: | ---: | ---: |
| Wikipedia | 5,032,104 | 8,093,810,896 bytes | 2,685,744,356 bytes | 302 |
| Stack Exchange | 150,000,000 | 84,521,226,768 bytes | 28,137,896,404 bytes | 1,254 |

Wikipedia comes from Search Benchmark Game revision
`a7c75473e91746280c5f01e69bf594ece5fca560`, with URL IDs and bodies lowercased
then normalized by replacing non-ASCII-letter runs with spaces. The 302 query
records preserve one term query and 301 union entries, including repetitions.
Our existing Hugging Face Wikipedia sample is a **different corpus**, even if
we reuse this query trace. The prepared Wikipedia CSV SHA-256 is
`7e4cba73338f9aba3a344de9f7d63007fa51f0f4ee85d70d7bd34fe95ff6e9a8`.
[Wikipedia source](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/wikipedia/source.json),
[manifest](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/wikipedia/data-manifest.json),
[trace](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/wikipedia/queries.json)

**Trace identity gap (history checked September 20):** the article describes 1,719 total requests derived
from 2–15-term substrings. The published Stack Exchange JSON instead contains
1,254 base records; the current runner expands a mixed workload to 3,762
record/style pairs, including single-term records. We can reproduce the
published source revision, but cannot call it the article's exact trace
without the missing run manifest. The corpus sampling/preparation is described,
but the original sampling procedure is not rerun by the Makefile.
[Stack Exchange trace](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/stackexchange/queries.json),
[query expansion](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/queries.js)

The deleted `queries.shortened.json` is recoverable at parent commit
[`4b065a7`](https://github.com/planetscale/paradedb-benchmarker/blob/4b065a745d14fa34569f3c4afee5a3889f0c7e7c/datasets/stackexchange/queries.shortened.json).
It contains **573 distinct records**, yielding **1,719 record/style pairs**,
exactly matching the article's total. Historical SOURCES.md records selection
seed `118538803`. However, deletion commit
[`2bf5cdd`](https://github.com/planetscale/paradedb-benchmarker/commit/2bf5cdd1b13805b287cc1274614c7fb34c1b9be8)
says **“remove shortened queries list -- was never used.”** One of its records
also contains only one term, while the article describes 2–15 terms. The full
trace was already 1,254 unique records when first committed in `62a0aa4`; this
is not an accidental duplicate count. Both trace candidates are recoverable;
which generated the charts is still unconfirmed. Preserve both and ask that
specific question, rather than asking for datasets we already have.

## SQL and baseline fairness

The public GIN baseline uses a **stored generated tsvector**:

```sql
body_tsv TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple', body)) STORED
CREATE INDEX documents_body_gin_idx ON documents USING gin (body_tsv);
```

Count predicates are `body_tsv @@ to_tsquery('simple', $1)`. Top-k selects
`id, body, ts_rank_cd(body_tsv, to_tsquery('simple', $1)) AS score`, orders
descending, and limits to ten by default. The table has text IDs and no primary
key in this baseline. Setup ends with `VACUUM (ANALYZE)` and `CHECKPOINT`.
It does not force index scans. This differs materially from our expression-GIN,
forced-index mutation comparison: stored vectors avoid repeated body parsing
on rechecks and ranking, at the cost of extra heap storage and write work.
[GIN schema](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/postgres/pre.sql),
[index setup](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/postgres/post.sql),
[SQL builder](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/queries.js)

ParadeDB uses `pdb.match` for ranked disjunctions and `pdb.parse` for other
forms, returning `id, body, pdb.score(id)`. pg_textsearch supports only ranked
disjunction in this runner, ordering its distance ascending. Therefore GIN's
`ts_rank_cd` is a separate ranking algorithm, not a BM25 correctness oracle.
Count comparisons still need analyzer/membership validation; identical query
text alone does not establish identical tokenization. The public runner has
TIN query strings in JSON, but **no TIN backend registration or TIN scenario**.
[SQL builder and supported backends](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/queries.js)

## Load, cache, and measurement semantics

The runner defaults to eight concurrent query workers, closed-loop query
execution, a 60-second measured interval, top ten, and seed `1592614637`.
It deterministically shuffles record/style pairs. Each PostgreSQL search index
is first read using `pg_prewarm(..., 'buffer', ...)`; then ordinary queries run
unmeasured for ten seconds. Measurement continues the query stream after
resetting counters. This is a warmed workload, **not cold storage latency**;
prewarming an oversized index cannot make it all resident simultaneously.
[Runner](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/search.js),
[phase coordinator](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/phases.go)

Concurrent updates use **one serial updater**, paced at a maximum start rate.
Missed slots are skipped rather than accumulated. Each operation selects the
first visible tuple on a random heap page and appends a space to its body.
This preserves search terms while exercising a real indexed-column update;
it is neither uniform row sampling nor our insert/update/delete mixture. Empty
pages can yield zero updated rows; the client turns a non-one row count into
a workload error. This failed our smaller high-churn smoke run and is handled
by the separately documented local live-tuple fallback. Reported updates count
affected rows, not attempted starts; two startup updates are excluded. The article's 1,000/s is
therefore a requested ceiling, not a sustained completion claim.
[Pacing](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/phases.go),
[update SQL](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/backends/shared/postgres/driver.go),
[completed update accounting](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/backends/driver.go)

The dashboard's bytes/query calculation is:

```text
(search-index idx_blks_read + search-index idx_blks_hit)
    × PostgreSQL block_size / completed queries
```

Those counters come from `pg_statio_user_indexes`, filtered by configured
access method. They include repeated block accesses and shared-buffer hits.
Reads can be satisfied by the OS cache. They are **not physical disk bytes**,
not unique bytes, and exclude heap/TOAST accesses. With updates enabled,
concurrent writes/maintenance can contribute to the same aggregate counters.
Record reads and hits separately and label the sum as logical search-index
block traffic. Compare with heap/TOAST and `pg_stat_io`/container diagnostics
before attributing a bottleneck to storage.
[Counter collection](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/backends/shared/postgres/driver.go),
[dashboard calculation](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/dashboard/static/index.html)

WAL uses a delta of `pg_current_wal_insert_lsn()`. This is cluster-wide WAL,
including heap/index/maintenance work, not search-index-only amplification.
Counter resets do not evict cache. Update runs retain modified data between
runs unless explicitly reloaded; repeated comparisons should instead begin
from matched snapshots or record accumulated state.
[WAL collector](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/backends/shared/postgres/driver.go),
[run lifecycle](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/README.md)

## Local adoption assessment

Local reproduction of the **method** is practical; reproducing the published
absolute numbers is not implied. The public recipe exposes CPU/RAM,
shared-buffer, maintenance-memory, query-file, and CSV overrides. Its defaults
are eight CPUs, 32 GiB query memory, 64 GiB build memory, and 24 GiB shared
buffers. Reduce all related allocations together for a smaller Docker VM.
The article used AVX-512-capable x86 hardware; native ARM measures a different
instruction set and storage stack.
[Container configuration](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/compose.yml),
[parameter overrides](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/Makefile.common)

A Stannum adapter could reuse the shared PostgreSQL driver, register Stannum's
access method for I/O counters, add schema/index setup, and generate Stannum
count/BM25 SQL from the existing TIN query strings. It must also extend the
runner's backend allowlist, connection map, index prewarm relation, and Compose
service; changing the SQL alone leaves the diagnostics wrong. This is an
implementation assessment, not a tested adapter.
[PostgreSQL registration pattern](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/backends/postgres/register.go),
[runner integration](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/search.js)

The upstream Make workflow assumes GNU Make/Bash, `flock`, GNU-compatible
utilities and, for interactive logs, util-linux `script`; invoking it unchanged
on macOS is not a verified path. Run those orchestration dependencies in Linux
or explicitly port them. Published image architectures and native ARM builds
still need verification before adding competing extensions; do not silently
benchmark one engine through x86 emulation.
[Dependencies](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/benchmarks/README.md),
[extension image definitions](https://github.com/planetscale/paradedb-benchmarker/tree/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/docker)

Recommended first integration: reuse the 302-query Wikipedia trace against
our existing local corpus, add stored-vector GIN with normal planner settings,
measure count and ranked workloads separately, and run both a resident and a
memory-constrained configuration. Preserve our stronger mutation correctness
checks as a separate workload. Promote to the exact 8.1 GB prepared Wikipedia
corpus only when trace handling, SQL semantics, resource caps, and metrics have
passed a small local smoke test. None of these steps requires AWS.
