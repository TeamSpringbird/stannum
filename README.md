<!--
Copyright (C) 2026 Ben Weis <ben@springbird.app>
Based on Lead, copyright (C) 2026 PlanetScale

See LICENSE in the repository root for license terms.
-->

# Stannum

Stannum is a full-text search index for PostgreSQL, written in Rust with pgrx.
It implements the interface of PlanetScale's TIN: the `==>` operator, the
TINQL query language (terms, Boolean queries, phrases, proximity, span
relations, positional filters and term expansions), BM25 ranking and
highlighting. Counts are exact under concurrent writes, and ranked queries
return the same rows and scores as scoring every match.

Stannum began as a fork of [PlanetScale Lead](https://github.com/planetscale/lead),
PlanetScale's open-source, correctness-oriented TIN reference, and replaces its
index with a segmented one built for corpora larger than memory. It is licensed
under the [AGPL-3.0-or-later](LICENSE), as Lead is.

## Status

Stannum is at **0.1.0-dev** and has not been tagged. PostgreSQL 17 and 18 on
x86-64 and arm64 are supported build targets; most lifecycle and performance
evidence is on PostgreSQL 18. The on-disk format may still change before the
first release, and a format change requires `REINDEX`. See
[releasing](docs/RELEASING.md) for the compatibility policy and the
[changelog](CHANGELOG.md) for what has changed.

## Build and install

The Rust toolchain is pinned in `rust-toolchain.toml`. Install `cargo-pgrx`
**0.19.1** and initialize a PostgreSQL version:

```sh
cargo install cargo-pgrx --version 0.19.1 --locked
cargo pgrx init --pg18=download
cargo pgrx run pg18 --package stannum
```

To build a package for an existing server:

```sh
cargo pgrx package --package stannum --no-default-features --features pg18
```

The server needs the compiled library, control file and extension SQL
installed before `CREATE EXTENSION stannum`. Stannum loads on demand on a
primary. Adding it to `shared_preload_libraries` on the primary and its
standbys also enables index reads on hot standbys; see
[recovery and parallel execution](docs/architecture/recovery-and-parallel.md).

## Usage

```sql
CREATE EXTENSION stannum;

CREATE TABLE documents (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
INSERT INTO documents (body) VALUES
  ('PostgreSQL supports full text search'),
  ('A search engine with exact phrase matching');
CREATE INDEX documents_search ON documents USING stannum (body);
ANALYZE documents;

-- Ranked search
SELECT id, stannum.full_score(ctid) AS score
FROM documents
WHERE body ==> 'search'
ORDER BY score DESC
LIMIT 10;

-- Exact counts, phrases and highlighting
SELECT count(*) FROM documents WHERE body ==> '"phrase matching"';
SELECT stannum.highlight(body, '<mark>', '</mark>', query => 'search') FROM documents;

-- Inspection and consistency checks
SELECT * FROM stannum.segment_info('documents_search');
SELECT * FROM stannum.verify_index('documents_search', heap_check => true);
```

The extension, library, access method, SQL schema and settings are all named
`stannum`. Functions include `stannum.score`, `stannum.full_score`,
`stannum.max_score`, `stannum.score_inspect`, `stannum.highlight`,
`stannum.highlight_ansi`, `stannum.segment_info` and `stannum.verify_index`.
Scoring must appear at the same query level as the matching `==>` predicate;
implicit highlighting also binds through a subquery or CTE the planner
flattens into that level. The [query language guide](docs/query-language/introduction.md)
covers TINQL, and [SQL permissions](docs/SECURITY.md) covers who may call what.

## Compatibility with TIN

Stannum is checked against PlanetScale TIN 1.0.3 by the
[TIN conformance suite](conformance/README.md), which replays TIN's recorded
answers: **170 PASS, 12 DIFF** (same SQLSTATE, different error wording),
**5 IMPROVED** (queries TIN refuses that Stannum answers), **2 GAP** and
**0 FAIL**. CI also runs a differential oracle against upstream Lead on every
push. [Compatibility](docs/compatibility.md) lists what matches, the
documented improvements and gaps, and the index options.

Stannum and TIN need separate databases, because both define `==>` in
`pg_catalog`. There is no in-place migration from TIN: create a fresh
database, reload the data, and build indexes with `USING stannum`.

## Performance

On PlanetScale's published Stack Exchange workloads at 150 million rows
(AWS i7i.8xlarge, 8 clients, an index larger than its 24 GB of shared buffers),
Stannum serves the mixed ranked workload at 269.9 queries a second (p50 15 ms,
p99 172 ms), conjunctions and phrases at 437.9, and disjunctions at 127.6
beside 254,224 updates with no errors, with every correctness check clean.
[Benchmarks](docs/benchmarks.md) describes the method, the local results and
how to reproduce them.

## Testing

[Testing](docs/testing.md) indexes every kind of test (unit and property
tests, the pgrx suite, lifecycle and crash tests, fuzzers, the reference
oracle and conformance) and how to run them. Report issues with reproduction
SQL, the PostgreSQL version, and expected and observed results. Performance
changes keep their correctness evidence: a faster query that changes the
answer is not an improvement.

## Documentation

The [documentation index](docs/README.md) lists every guide. Start with:

- [Query language](docs/query-language/introduction.md)
- [How Stannum works](docs/architecture/segmented-storage.md): storage,
  query execution, ranking, maintenance, index verification and current
  limits. Temporary and unlogged indexes use the same segmented storage;
  unlogged indexes reset to a valid empty index after a crash.
- [Compatibility with TIN](docs/compatibility.md)
- [Benchmarks](docs/benchmarks.md)
- [Testing](docs/testing.md)

## License

Stannum is free software under the GNU Affero General Public License, version
3 or any later version; see [LICENSE](LICENSE). It contains code from
PlanetScale Lead, copyright PlanetScale; [source attribution](docs/ATTRIBUTION.md)
explains the notices and provenance records, and
[upstream synchronization](docs/UPSTREAM.md) records how Lead changes are
taken.
