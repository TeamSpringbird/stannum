# Stannum

Stannum is an experimental, open-source PostgreSQL search engine with Boolean and
positional queries, BM25 ranking, and exact counts under concurrent writes. The
index, query execution, scoring, and storage implementation live in this repository.

We started from [PlanetScale Lead](https://github.com/planetscale/lead), a deliberately
slow, correctness-oriented substitute for TIN. We are developing that foundation
into a useful search engine: durable inverted indexes, stored ranking statistics,
and PostgreSQL execution paths that avoid scanning and retokenizing the entire
corpus for every query. Stannum is an independent fork, not PlanetScale TIN or a
PlanetScale-supported product. The inherited code remains under AGPL-3.0; see
[LICENSE](LICENSE).

**This is development software, not a production-ready database extension.** The
first 100k-document measurements are encouraging, but short benchmarks and targeted
correctness tests do not establish long-term reliability. Our next steps are
repeated comparisons, sustained mutation and maintenance tests, and broader
recovery testing. See [BENCHMARKS.md](BENCHMARKS.md) for evidence and limitations.

## What works today

- TINQL terms, Boolean expressions, phrases, proximity, positional filters, and
  expansion queries, with exact matching or a conservative recheck fallback.
- Logged indexes with a mutable write buffer and immutable segments containing
  dictionaries, tuple postings, positions, term frequencies, and document lengths.
- Index-backed BM25 scoring, visibility-aware count scans, and top-k selection.
- Inserts, updates, VACUUM, segment folding/merging, page reuse, and generic WAL.
- Highlighting, tokenizer options, and `stannum.segment_info` for index inspection.

The [storage and execution notes](docs/segmented-storage.md) describe the design and
its current boundaries. Ranked queries still score all candidates before selecting
the top k. Folding and merging can delay the inserting transaction; maintenance
and reclamation need more testing. Standby/recovery reads and temporary or unlogged
indexes use slower fallback paths. Nondefault tokenizer behavior can differ between
an index scan and a sequential scan. These are active engineering gaps.

## Build and try it

The toolchain is pinned in `rust-toolchain.toml`. Install `cargo-pgrx` **0.19.1** and
initialize a supported PostgreSQL version:

```sh
cargo install cargo-pgrx --version 0.19.1 --locked
cargo pgrx init --pg18=download
cargo pgrx run pg18 --package stannum
```

Inside the development database:

```sql
CREATE EXTENSION stannum;

CREATE TABLE documents (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
INSERT INTO documents (body) VALUES
  ('PostgreSQL supports full text search'),
  ('A search engine with exact phrase matching');
CREATE INDEX documents_search ON documents USING stannum (body);
ANALYZE documents;

SELECT id, stannum.full_score(ctid) AS score
FROM documents
WHERE body ==> 'search'
ORDER BY score DESC
LIMIT 10;

SELECT count(*) FROM documents WHERE body ==> '"phrase matching"';
SELECT stannum.highlight(body, '<mark>', '</mark>', query => 'search') FROM documents;
SELECT * FROM stannum.segment_info('documents_search');
```

For a release package:

```sh
cargo pgrx package --package stannum --no-default-features --features pg18
```

PostgreSQL 17 and 18 are build targets. Most recent local lifecycle and benchmark
evidence is on PostgreSQL 18. Stannum loads on demand; it does not require
`shared_preload_libraries`. The receiving server must have the compiled library,
control file, and extension SQL installed before `CREATE EXTENSION` can work.

## Names and compatibility

The extension, library, access method, and SQL schema are **`stannum`**. Functions
include `stannum.score`, `stannum.full_score`, `stannum.max_score`,
`stannum.score_inspect`, `stannum.highlight`, and `stannum.highlight_ansi`. Settings
use the `stannum.` prefix; for example, `SET stannum.enable_custom_scan = off`
selects the bitmap path.

TINQL remains the query language name, and the `tinql` crate implements that
language. References to TIN in compatibility research and the `--engine tin`
benchmark adapter refer to PlanetScale's actual extension. They do not identify
Stannum builds. Sampled oracle fixtures compare match sets and score bits against
TIN; this is evidence of compatibility on those fixtures, not full equivalence.

Scoring and implicitly bound highlighting must appear at the same query level as
the matching `==>` predicate. Explicit highlighting accepts its own query.

### Moving from the pre-rename build

This is a breaking package/SQL rename, not an `ALTER EXTENSION tin UPDATE` migration.
Create Stannum in a fresh database, reload the data, and rebuild indexes with
`USING stannum`. Update `tin.*` application calls and settings to `stannum.*`.
Do not rename or replace an installed TIN library or reuse its indexes.

The `==>` operator still lives in `pg_catalog` for compatibility. Stannum and TIN
therefore need separate databases; distinct extension names alone do not make
installation together in one database supported. Same-instance benchmarks can use
one database per engine and alternate the measured traffic.

## Validate changes

```sh
cargo test --locked -p tinql -p tokenizer -p boldi-vigna -p segment
cargo pgrx test pg18 --package stannum --no-default-features --features pg18
cargo clippy --locked --workspace --all-targets --no-default-features --features 'pg18 pg_test' -- -D warnings
python3 -m unittest discover -s benchmarks -p 'test_*.py'
```

After installing the extension into the PostgreSQL distribution on `PATH`, run
`python3 postgres/tests/postings_lifecycle.py` for an isolated temporary cluster's
mutation, VACUUM, restart, and crash-recovery checks. It removes its own cluster
when finished. Developers with access to the upstream private regression suite can
use `TIN_PRIVATE_REPO=/path/to/full-tin script/run-private-regress pg18`; the copier
translates extension names in generated fixtures without changing the upstream source.

## Learn more and contribute

- [Benchmarks, results, and the comparison plan](BENCHMARKS.md)
- [Benchmark harness reference](benchmarks/README.md)
- [Storage and query execution](docs/segmented-storage.md)
- [TINQL guide](tinql/docs/src/SUMMARY.md)

The query guide can be built with `mdbook build tinql/docs` using mdBook 0.5.2.
Report issues in this repository with reproduction SQL, PostgreSQL version,
expected results, and observed results. Preserve correctness evidence alongside
performance changes; a faster query that changes the answer is not an improvement.
