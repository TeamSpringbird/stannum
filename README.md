# Stannum

Stannum is an experimental, open-source PostgreSQL search engine with Boolean and
positional queries, BM25 ranking, and exact counts under concurrent writes. The
index, query execution, scoring, and storage implementation live in this repository.

Stannum is an independent fork of [PlanetScale Lead](https://github.com/planetscale/lead),
which provides a correctness-oriented substitute for PlanetScale TIN. We are
building an indexed search engine on that foundation. The project uses the TINQL
query language and retains the inherited [AGPL-3.0 license](LICENSE).

**Stannum is development software.** Local 100k-document benchmarks show substantial
progress over Lead, but reliability and broader compatibility are still being
validated. See [results and next steps](docs/benchmarks/README.md).

It supports terms, Boolean queries, phrases, proximity, ranking, and highlighting.
The [architecture guide](docs/architecture/segmented-storage.md) explains how the
index works and lists known limitations, including tokenizer consistency,
maintenance latency, and slower fallback paths.

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
SELECT * FROM stannum.verify_index('documents_search', heap_check => true);
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
`stannum.score_inspect`, `stannum.highlight`, `stannum.highlight_ansi`,
`stannum.segment_info`, and `stannum.verify_index`. Settings
use the `stannum.` prefix; for example, `SET stannum.enable_custom_scan = off`
selects the bitmap path.

Scoring and implicitly bound highlighting must appear at the same query level as
the matching `==>` predicate. Explicit highlighting accepts its own query.

Stannum and TIN need separate databases because both define the `==>` operator in
`pg_catalog`. There is no in-place migration from TIN or older renamed builds:
create a fresh database, reload the data, and rebuild indexes with `USING stannum`.

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
when finished.

## Learn more and contribute

Start with the [documentation index](docs/README.md) for the current guides.

- [Benchmarks, results, and the comparison plan](docs/benchmarks/README.md)
- [Benchmark harness reference](docs/benchmarks/harness.md)
- [Storage and query execution](docs/architecture/segmented-storage.md)
- [TINQL guide](docs/query-language/README.md)

Report issues in this repository with reproduction SQL, PostgreSQL version,
expected results, and observed results. Preserve correctness evidence alongside
performance changes; a faster query that changes the answer is not an improvement.
