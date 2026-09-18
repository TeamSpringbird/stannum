# Benchmark tools

Start with [Run a local benchmark](local.md) for a complete campaign. This page
explains the individual tools, their outputs, and comparison limits. Run commands
from the repository root; each tool also provides `--help`.

## Tools

| Script in `benchmarks/` | Purpose |
| --- | --- |
| `dataset.py` | Prepare checksummed Wikipedia samples and expected match sets |
| `campaign.py` | Build a container image and run repeated trials in fresh volumes |
| `baselines.py` | Run a series over selected dataset sizes; defaults to 100k |
| `run.py` | Run one workload, compare compatible results, or export history |
| `paired.py` | Alternate two Stannum builds under the same protocol |
| `oracle.py` | Compare document sets and score bits between Stannum and TIN |
| `server_times.py` | Inspect per-query server execution times and plans |

## One run against an existing server

Install a release build first. Set libpq connection variables for your dedicated
benchmark server and create a fresh database with a `stannum_bench_` prefix:

```sh
createdb stannum_bench_run01
python3 benchmarks/run.py run \
  --engine stannum --database stannum_bench_run01 \
  --output benchmarks/results/run01 \
  --environment dedicated-benchmark-server \
  --build-id "$(git rev-parse HEAD):release" \
  --profile mixed --rows 10000 --seconds 60 --warmup 10 \
  --clients 2 --write-rate 20
```

`--build-id` must identify the installed build, not just your source checkout.
Use `--artifact /path/to/stannum.so` (or `.dylib`) to record its binary hash.
Restart server sessions after installing a new binary. The runner creates its own
`documents` table and refuses to overwrite an existing fixture. It leaves the
database for inspection.

Use `count` for counts, `ranked` for top-ten ranking, and `mixed` for both.
`--write-rate 0` makes a single run read-only. Credentials belong in local libpq
configuration, not in committed files or build labels.

## Available adapters

The harness supports `stannum`, externally installed `tin`, built-in PostgreSQL
`gin`, ParadeDB `paradedb`, and `pg_textsearch`. GIN is count-only. The local
container recipe pins comparator versions; different versions may require
adapter changes. TIN is not bundled in that image.

Matching result sets does not establish equivalent ranking across engines.
Cross-engine ranked/mixed speedup reports are therefore blocked by the comparison
tool. Different hardware or resource budgets are not comparable even when the
query text matches.

## Correctness and diagnostics

`run.py` checks match membership before and after traffic. Ranked checks cover
membership, cardinality, uniqueness, finite scores, and descending order; they do
not prove global top-k correctness.

`oracle.py` compares exact document sets and score bits across mutation states:

```sh
python3 benchmarks/oracle.py \
  --left stannum.env --left-engine stannum \
  --right tin.env --right-engine tin \
  --rows 5000 --output benchmarks/results/oracle-01
```

Use two distinct, dedicated databases with no `oracle_docs` table. The environment
files contain libpq settings and must remain outside Git. The tool creates its
fixture and removes it on success unless `--keep` is selected.

`server_times.py` runs `EXPLAIN ANALYZE` against an existing benchmark fixture.
It reports execution time and plan shape, excluding planning and client transfer.
It is a diagnostic tool, not a substitute for comparable end-to-end benchmarks.

## Result files

| File | Contents |
| --- | --- |
| `manifest.json` | Build, source fingerprint, settings, workload, and completion status |
| `source.patch` | Tracked source changes at measurement time |
| `fixture.sql`, `query-*.sql`, `writer.sql` | Executed workload |
| `correctness-*.json`, `ranked-*.json`, `plan-*.json` | Checks and query plans |
| `reader-log.*`, `writer-log.*` | Raw transaction measurements |
| `summary.json` | Throughput, latency, and achieved write rate |
| `before.json`, `after.json` | Size and database counter snapshots |

Keep complete result directories outside Git. Do not pool incompatible runs,
rewrite old manifests, or discard failures. Reports should retain the number of
successful trials and the spread of results, not just a single speedup.

```sh
python3 benchmarks/run.py compare benchmarks/results/run01 benchmarks/results/run02
python3 benchmarks/run.py history benchmarks/results > benchmarks/results/history.csv
python3 -m unittest discover -s benchmarks -p 'test_*.py'
```
