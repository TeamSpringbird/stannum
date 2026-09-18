# TIN architecture and benchmark reproducibility

Investigated 2026-09-17. This note records primary-source claims and inspected benchmark assets. No TIN or Lead performance measurements were taken.

## Architectural direction

PlanetScale describes native `ctid` postings encoded as page and tuple bitmaps, with 256-page groups. Bitmap operations skip unnecessary decoding and support SIMD intersection, union, and counting. Custom scans combine visibility-map checks with heap visibility checks; VACUUM updates liveness bitmaps. Mutable segments become immutable, then merge without document renumbering; some stored bitmap data can transfer ownership. These are vendor descriptions, not independently verified implementations. [Introducing TIN](https://planetscale.com/blog/introducing-tin#why-tin-is-fast)

TIN's public documentation also establishes the intended behavior: snapshot-consistent search, BM25 ranking, exact counts under writes and VACUUM, and positional query composition. These are compatibility targets, not evidence that any specific implementation strategy is sufficient. [TIN documentation](https://planetscale.com/docs/postgres/search)

**Inference:** Lead can retain its parser and SQL contract while gaining an actual retrieval engine, but knowing the outline does not determine the on-disk format, top-k pruning strategy, WAL protocol, or concurrent segment publication rules. Those require design, correctness tests, and measurements.

## Public benchmark assets

The linked [benchmark fork](https://github.com/planetscale/paradedb-benchmarker) has materially different branches. Pin commits rather than assuming the default checkout contains the complete experiment:

| Branch inspected | Commit | Contents |
| --- | --- | --- |
| `main` | `87cdf7700c8532b07b01b72d880d31c9a072582a` | k6 framework, loader, sample workload |
| `ps-mods` | `fbad65cb0d1409dd43a514d86d4c89dbc554f3a1` | Phase coordination, prewarming, PostgreSQL diagnostics, I/O and WAL accounting |
| `datasets` | `f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86` | Stack Exchange and Wikipedia schemas, query sets, manifests, Git LFS corpus pointers |

The framework accepts SQL workloads and supports concurrent query and update scenarios. Its README explicitly identifies the sample dataset as a usage example rather than a meaningful performance benchmark. [Framework README](https://github.com/planetscale/paradedb-benchmarker/blob/87cdf7700c8532b07b01b72d880d31c9a072582a/README.md)

The Stack Exchange manifest records an 84,521,226,768-byte CSV, a 28,137,896,404-byte gzip stream, and 15 chunks, with SHA-256 checksums for each. Chunk files in Git are LFS pointers. The schema is `documents(id text, body text)`; provenance references a June 2026 Internet Archive export and an existing sampled CSV. The large LFS objects were not downloaded, so their availability and checksum integrity remain unverified. [Manifest](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/stackexchange/data-manifest.json), [schema](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/stackexchange/schema.yaml), [provenance](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/stackexchange/source.json)

Parsing the published JSON yields **1,254 Stack Exchange source queries** and **302 Wikipedia source queries**, each with per-engine encodings. [Stack Exchange queries](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/stackexchange/queries.json), [Wikipedia queries](https://github.com/planetscale/paradedb-benchmarker/blob/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets/wikipedia/queries.json)

The article instead reports 1,719 total queries across three forms; therefore the inspected dataset does not establish an identical trace. [Published workload](https://planetscale.com/blog/introducing-tin#workloads-and-corpus)

## Measurement semantics and gaps

The PostgreSQL driver computes index read bytes from `pg_statio_user_indexes.idx_blks_read * block_size`, separately from shared-buffer hits. This is index-block accounting, not necessarily physical-device I/O, and excludes heap reads. The generic registered PostgreSQL FTS backend enables it for GIN indexes. A Lead adapter must also measure heap and TOAST activity: index-only accounting would misleadingly favor an extension that stores no postings. [Driver implementation](https://github.com/planetscale/paradedb-benchmarker/blob/fbad65cb0d1409dd43a514d86d4c89dbc554f3a1/backends/shared/postgres/driver.go), [backend registration](https://github.com/planetscale/paradedb-benchmarker/blob/fbad65cb0d1409dd43a514d86d4c89dbc554f3a1/backends/postgres/register.go)

WAL per completed update is deliberately server-wide, measured after prewarming and including concurrent maintenance. This should be recorded alongside actual completed updates, not merely the requested update rate. [WAL metric definition](https://github.com/planetscale/paradedb-benchmarker/blob/fbad65cb0d1409dd43a514d86d4c89dbc554f3a1/docs/wal-update-metrics.md)

The inspected trees do not provide a complete Stack Exchange run recipe with its exact query subset, concurrency, SQL, per-engine setup, and exported raw results. They provide ingredients for a new controlled benchmark; they do not yet establish reproduction of the published run. [Dataset tree](https://github.com/planetscale/paradedb-benchmarker/tree/f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/datasets), [instrumented tree](https://github.com/planetscale/paradedb-benchmarker/tree/fbad65cb0d1409dd43a514d86d4c89dbc554f3a1)

**Proposed evidence standard:** first compare Lead revisions against a pinned corpus and query trace locally, preserving result correctness and separating count, retrieval, ranking, and writes. Record plans, latency distributions, CPU, heap/index I/O, WAL, memory, and completed updates. Only claim proximity to TIN after matching corpus, trace, settings, hardware, cache state, and measurement semantics, ideally running TIN directly under the same conditions.
