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

## Sustained mutation profile

The `count`, `ranked` and `mixed` profiles keep the corpus fixed: their writer
toggles a reserved suffix, so match sets never change and nothing is inserted
or deleted. `--profile mutation` measures the index under real churn. It runs
the `mixed` read shapes with closed-loop readers by default, or a total offered
`--read-rate`, while `--writers` connections (default one) share the total
`--write-rate` in statements per second. Each writer draws each
transaction from a weighted mix (`--mix insert=1,delete=1,update=1`):

| Kind | Statement |
| --- | --- |
| `insert` | Copies a random dataset document under a fresh id from `benchmark_ids` |
| `delete` | Deletes the first live row at or above a random id |
| `update` | Rewrites a row's reserved suffix with the terms of one random query (or none) |

See the [sustained campaign](sustained-mutation.md) for repeated rate/concurrency
sweeps, affected-row accounting, scheduling pressure, maintenance overlap gates,
and a separately measured cleanup phase. This controlled mutation profile disables
table autovacuum; scheduled VACUUM remains explicit. Observer queries use the
configured finite statement timeout.

Updates append `mutablea` plus a *drift bundle*: the plain terms of one of the
fixture's non-miss queries, so a document starts or stops matching `rare`,
`"quantum mechanics"`, `war AND history` and so on. Deletes and updates probe
ids up to the number of rows plus the inserts expected in the window, so rows
inserted during the run are mutated too. Inserted bodies are copies of corpus
documents kept in `benchmark_pool`, so their term statistics resemble the corpus;
they are not held-out articles. GIN cannot run this profile (ranked shapes).

During the timed window three schedules run beside the traffic:

* **Correctness** (`--check-interval`, default 30 s): every query is answered
  by the index and by a `body ~ '\mterm\M'` regular-expression sequential scan
  (`AND`/`OR`/phrase forms accordingly) in one statement, hence one snapshot,
  inside a `REPEATABLE READ` transaction with `enable_seqscan = off`, so the
  `==>` side must use the index while the regex side cannot. The run records
  `EXPLAIN` for each check (`plan-check-*.json`) and refuses to start if the
  oracle side is not a sequential scan. Any id in one answer and not the other
  fails the run immediately, as does a ranked top ten that is not distinct,
  finite, descending, of size `min(10, count)` and inside the oracle set. The
  oracle is independent of the tokenizer and of the index; it costs about one
  second per query per 100k Wikipedia articles on one core, which the run
  shares with the readers.
* **VACUUM** (`--vacuum-interval`, default 60 s, `0` disables): `VACUUM
  (INDEX_CLEANUP ON, VERBOSE) documents`, with the verbose output retained in
  `vacuum-N.txt` and index size, free pages, dead-document counts and segment
  counts sampled immediately before and after.
* **Layout samples** (`--sample-interval`, default 5 s): index and table size,
  row count, `pg_stat_user_tables` counters, `stannum.segment_info` (segment
  count, dead documents, live pages, highest generation, buffered documents)
  and, when `pg_freespacemap` can be created, the number of index pages the
  free-space map reports as reusable. A sample that blocks behind a merge
  shows up in its own duration.

`--set NAME=VALUE` (repeatable) applies a session setting to every connection
of the run, so `--set stannum.write_buffer_docs=256` drives folds and tiered
merges many times within a short window. The settings are recorded in the
manifest. After the traffic a final oracle round replaces the fixture-based
`correctness-after.json`, since the match sets have drifted by design.

```sh
createdb stannum_bench_mut01
python3 benchmarks/run.py run \
  --engine stannum --database stannum_bench_mut01 \
  --output benchmarks/results/mut01 \
  --environment dedicated-benchmark-server \
  --build-id "$(git rev-parse HEAD):release" \
  --profile mutation --dataset benchmarks/results/datasets/wikipedia-100000 \
  --rows 100000 --seconds 600 --warmup 30 --clients 2 --write-rate 50 \
  --check-interval 30 --vacuum-interval 60 --statement-timeout-ms 1800000
python3 benchmarks/run.py timeline benchmarks/results/mut01 --bucket-seconds 60
```

Additional result files: `writer-<kind>.sql`, `check-<name>.sql`,
`plan-check-<name>.json`, `samples.json`, `vacuums.json`, `checks.json`
(every periodic round with per-query counts, so the drift is visible),
`vacuum-N.txt`, and `timeline.json`/`timeline.txt`. The timeline buckets the
pgbench logs by completion time (`--bucket-seconds`, default 10): per bucket
the reads per second, count and ranked p50/p99, the slowest statement of each
mutation kind, the writer's largest schedule lag, the last layout sample
(segments, generation, buffered documents, index size, free pages) and the
VACUUMs and checks that completed in it. A fold, merge or VACUUM stall is
therefore visible as one bucket's reader p99 or mutation maximum. Writer values
in the timeline are execution times; `summary.json` keeps pgbench's definition,
which under `-R` counts latency from the scheduled start, and adds
`maintenance.reclaim` (per VACUUM) and `worst_mutations` (per kind). Bucket
percentiles come from far fewer samples than the whole-run figures; read them
with the `completed` counts in `timeline.json`. The `timeline` subcommand
rebuckets a finished run at another width.

`campaign.py --profiles mutation` schedules the profile in the container
campaign with the defaults above; its report sums the writer's kinds into the
write QPS column. Fresh containers keep folds and merges from one repetition
out of the next.

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
not prove global top-k correctness. The mutation profile repeats a
sequential-scan comparison throughout the window (see above).

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
