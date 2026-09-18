# Lead

Lead is a deliberately non-production Postgres text-search extension for exercising TIN-compatible application SQL in development, test, CI, and staging environments.

It favors correctness and a small implementation over production query performance.

This fork is developing a real indexing engine and remains unsuitable for production workloads. Logged indexes store a write buffer of per-document records and immutable segments with a term dictionary, positions and document lengths, and answer every query form exactly from the index. Ranking still reads the heap. See [segmented storage](docs/segmented-storage.md) for the format, tested behavior and limits.

## Build

[Install `cargo-pgrx`](https://github.com/pgcentralfoundation/pgrx/blob/develop/cargo-pgrx/README.md) version 0.19.1 exactly and initialize it for the Postgres major versions you need, then build or package with one version feature:

```sh
cargo pgrx package --package tin --no-default-features --features pg18
```

For an interactive development database, run:

```sh
cargo pgrx run pg18 --package tin
```

Then run `CREATE EXTENSION tin` in the database. Lead loads on demand and does not require `shared_preload_libraries` or `session_preload_libraries`.

## Compatibility boundary

Lead provides the `tin` access method, the `==>` operator, TINQL parsing, tokenizer and index reloptions, and the scoring functions `tin.score`, `tin.full_score`, `tin.max_score`, and `tin.score_inspect`, plus explicit and implicitly bound `tin.highlight` and `tin.highlight_ansi`. Postgres 17 and 18 are build targets. Index scans return exact tuples for every query form, analyzed with the index's own tokenizer settings; PostgreSQL applies visibility rules and rechecks only where a wildcard, regex, range or fuzzy term expands past the index's cap. Fallback paths retain whole-page candidates rechecked with `==>`, including expression and partial-index rechecks.

Scoring deliberately rescans and retokenizes the visible indexed column or expression. A score call must be in the same query level as the matching `==>` predicate. Implicit highlighting has the same binding boundary; passing its `query` argument explicitly works without a bound predicate.

## Execution and storage

Logged indexes hold a write buffer of per-document records and a directory of immutable segments, each with a term dictionary, TID postings, token positions and document lengths. Inserts append one record; the buffer folds into a segment at a bounded size; VACUUM records dead documents, rewrites mostly-dead segments and reclaims pages through the free space map. Recovery-mode reads, temporary/unlogged indexes and indexes in older formats retain full heap-page candidates; older formats report `REINDEX required` when written to or scanned selectively.

Lead allocates no extension shared memory and creates no files outside Postgres's normal relation storage. Logged postings use generic WAL for recovery. Targeted restart and crash-recovery tests pass on PostgreSQL 18; broader fault-injection testing is still needed.


## Tests

Run the local unit and Postgres tests for a supported Postgres major version with:

```sh
cargo pgrx test pg18 --package tin --no-default-features --features pg18
```

Developers with access to the TIN private source may also run the more comprehensive test suite that comes with that:

```sh
TIN_PRIVATE_REPO=/path/to/full-tin script/run-private-regress pg18
```

## Performance experiments

The [local Mac Studio campaign](benchmarks/LOCAL.md) runs engines sequentially in
equal-budget ARM64 containers and summarizes repeated runs. It does not run in CI.
The [benchmark harness](benchmarks/README.md) records per-commit workload results,
raw latency samples, concurrent-write throughput, correctness checks, and query
plans. The [Tin configuration investigation](docs/tin-configuration-research.md)
documents semantic and execution controls relevant to fair comparisons.

## TINQL guide

The [TINQL guide](tinql/docs/src/SUMMARY.md) documents the query language. To build it with mdBook, run from the repository root:

```sh
cargo install mdbook --version 0.5.2 --locked
mdbook build tinql/docs
```

Open `tinql/docs/book/index.html` in your browser to read the book.

## Contributing

We intend for Lead to be a slow but correct substitute for TIN, for use at small scales in development and testing environments.  If you find cases where it's unsuitable for that, please contact PlanetScale through normal support channels or open an issue in this repo.  The most helpful bug reports will include information about what you expected Lead to do (which is normally whatever TIN would do in the same situation) versus what it actually did.  Help us recreate the problem so we can fix it.
