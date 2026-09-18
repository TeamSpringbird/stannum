# Adapter smoke environments, 2026-09-17

Historical environment observations; Lead/TIN build identities are preserved.
For current Stannum results, see [BENCHMARKS.md](../BENCHMARKS.md).

These are correctness/execution checks with 1,000 synthetic documents, one reader,
5 seconds of traffic, 1 second of read warmup, and a requested 10 updates/second.
They are too short and too small for performance claims. All completed their
pre/post exact-match checks and transaction logs without errors. Ranked/mixed
adapters additionally passed result shape, membership and score-order probes.

| Engine | Exact build | Environment | Workload exercised |
| --- | --- | --- | --- |
| Lead | `3fcf441ac7c3d183de179b1f846ceb0ef83e1358`, release/pgrx 0.19.1, Rust 1.96.0 | Native ARM64, Homebrew PostgreSQL 18.6, pgrx instance | Mixed and count |
| GIN | Built into Homebrew PostgreSQL 18.6 | Same instance as Lead, separate fresh database | Count |
| ParadeDB | `pg_search` 0.25.9, image below | ARM64 Linux Docker, 2 CPU / 2 GiB, PostgreSQL 18.6 | Mixed |
| pg_textsearch | `1.5.0-dev`, commit `d7f04d59fa870902ced5f28689cad307884c0787` | Separate native ARM64/Homebrew 18.6 instance with preload | Mixed |

pg_textsearch's current development branch has Boolean-filter support beyond the
released 1.4.0 configuration described in TIN's launch article. This validation
does not claim that the same adapter works against that older release. A release
campaign must explicitly choose released versions or label development builds.

## ParadeDB

The resolved ARM64 image was pinned before use:

```sh
docker run -d --name lead-bench-paradedb --cpus=2 --memory=2g \
  -p 127.0.0.1:28820:5432 \
  -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=lead_bench_parade \
  paradedb/paradedb@sha256:c17153b8b7307734c3aede0393dfe8fa447f4a528ec028b1c3937186ab3ee242
docker exec lead-bench-paradedb pg_isready

PGHOST=127.0.0.1 PGPORT=28820 PGUSER=postgres python3 benchmarks/run.py run \
  --engine paradedb --database lead_bench_parade \
  --output benchmarks/results/mixed-paradedb \
  --environment docker-arm64-2cpu-2gb \
  --build-id paradedb/paradedb@sha256:c17153b8b7307734c3aede0393dfe8fa447f4a528ec028b1c3937186ab3ee242 \
  --profile mixed --rows 1000 --seconds 5 --warmup 1 --clients 1 --write-rate 10
docker stop lead-bench-paradedb
```

Trust authentication is confined to this loopback-published, disposable benchmark
instance. Commands assume the container/database name is not already in use.
Repeated runs require new database names; never reuse the populated fixture.

## pg_textsearch

The following records the source-build method; use fresh directories and database
names if reproducing. The extension library/control/SQL files were installed into
Homebrew PostgreSQL's extension directories. The existing pgrx server configuration
was not changed; a separate temporary cluster was started for preload support.

```sh
git clone https://github.com/timescale/pg_textsearch.git /tmp/lead-bench-pg-textsearch
git -C /tmp/lead-bench-pg-textsearch checkout d7f04d59fa870902ced5f28689cad307884c0787
make -C /tmp/lead-bench-pg-textsearch -j4 PG_CONFIG="$(command -v pg_config)"
make -C /tmp/lead-bench-pg-textsearch install PG_CONFIG="$(command -v pg_config)"
initdb -D /tmp/lead-bench-pgtext-data --locale=C --encoding=UTF8 --auth=trust
pg_ctl -D /tmp/lead-bench-pgtext-data -l /tmp/lead-pgtext-server.log \
  -o '-p 28819 -h 127.0.0.1 -c shared_preload_libraries=pg_textsearch' start
PGHOST=127.0.0.1 PGPORT=28819 createdb lead_bench_pgtext
PGHOST=127.0.0.1 PGPORT=28819 python3 benchmarks/run.py run \
  --engine pg_textsearch --database lead_bench_pgtext \
  --output benchmarks/results/mixed-pgtext --environment local-arm64-pgtext \
  --build-id 'pg_textsearch d7f04d59fa870902ced5f28689cad307884c0787; make -O2' \
  --artifact "$(pg_config --pkglibdir)/pg_textsearch.dylib" \
  --profile mixed --rows 1000 --seconds 5 --warmup 1 --clients 1 --write-rate 10
pg_ctl -D /tmp/lead-bench-pgtext-data stop
```

## Retained evidence

Local run directories live under ignored `benchmarks/results/`; each manifest
contains versions, harness digest and actual SQL. `history.csv` can be regenerated
with the `history` command. Different harness revisions during initial bring-up
form separate cohorts. Failed or incompatible runs must not be retroactively
relabeled as comparable. Archive results before cleaning this workspace; these
local artifacts have not been uploaded to a durable remote store.

The initial five validated run directories were bundled locally as
`results/adapter-smoke-2026-09-17.tar.gz` with SHA-256
`00f6ea6225a03d7e218503d27b87ce09423a12b54ec4dcd45a0520926eba10df`.
This preserves raw evidence for harness bring-up; it is not a competitive baseline.
Four measurement-integrity unit tests passed, and an attempted cross-engine mixed
comparison was correctly rejected. The temporary servers were stopped after runs;
database contents, the downloaded Docker image/container, and source build remain
available locally for inspection.

The subsequent [local Mac Studio campaign](LOCAL.md) runs all four engines inside
the same Linux VM using equal container budgets, a common pinned image and five
rotating repetitions. Extend correctness coverage and ranking-quality validation
before allowing ranked cross-engine ratios. Import real corpora and run long enough
for maintenance before publishing a competitive conclusion.
