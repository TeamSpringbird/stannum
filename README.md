# Lead

Lead is a deliberately non-production Postgres text-search extension for exercising TIN-compatible application SQL in development, test, CI, and staging environments.

It favors correctness and a small implementation over production query performance.

This fork is developing a real indexing engine and remains unsuitable for production workloads. New logged indexes now store WAL-protected term postings and selectively retrieve term, Boolean and phrase candidates with exact heap rechecks. Unsupported expressions retain the original heap-scan fallback. See the [durable retrieval milestone](docs/durable-postings.md) for tested behavior and limitations. The [first measured milestone](docs/performance-milestone-01.md) records a 3.14x mixed-workload throughput gain over original Lead on a small synthetic corpus.

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

Lead provides the `tin` access method, the `==>` operator, TINQL parsing, tokenizer and index reloptions, and the scoring functions `tin.score`, `tin.full_score`, `tin.max_score`, and `tin.score_inspect`, plus explicit and implicitly bound `tin.highlight` and `tin.highlight_ansi`. Postgres 17 and 18 are build targets. Candidate tuples are rechecked with `==>` and PostgreSQL visibility rules. Unsupported retrieval shapes retain whole-page candidates, including expression and partial-index rechecks.

Scoring deliberately rescans and retokenizes the visible indexed column or expression. A score call must be in the same query level as the matching `==>` predicate. Implicit highlighting has the same binding boundary; passing its `query` argument explicitly works without a bound predicate.

## Execution and storage

New logged indexes store fingerprint-to-TID postings in PostgreSQL pages. Builds and inserts maintain postings, term, Boolean and phrase scans populate a rechecked tuple bitmap, and VACUUM removes dead entries. Fixed buckets and unrecycled overflow pages are initial implementation limits. Unsupported expressions, recovery-mode reads, temporary/unlogged indexes and legacy zero-page indexes retain full heap-page candidates. REINDEX upgrades an existing zero-page index.

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
