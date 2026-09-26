# Testing Stannum

`script/test-all` is the one entry point for the tests. CI and the local
gates (`benchmarks/local/gates.sh`) call it, so the three run the same
commands; this page is the index of every kind of test, what it needs and
where it runs.

```sh
script/test-all quick     # formatting, lint and unit tests; no PostgreSQL server
script/test-all full      # quick, then pg_tests, the cluster tests and conformance
script/test-all cluster   # one step; --list shows what a tier runs
```

| tier | steps |
|------|-------|
| `quick` | `headers` `fmt` `clippy` `rust-unit` `python-unit` |
| `full` | `quick`, then `pgrx` `install` `cluster` `conformance` |
| `gates` | `install` `cluster` `conformance` `oracle` (what `benchmarks/local/gates.sh` runs) |

Every command in the selected steps runs, and the run ends with a summary and
a nonzero exit if any failed; `--fail-fast` stops at the first failure and
`--logs DIR` writes each command's output to `DIR/<command>.log`. A failed
`install` always stops the run, since later steps would test another build.

## Prerequisites

- The Rust toolchain `rust-toolchain.toml` pins, with rustfmt and Clippy.
  Without a rustup proxy on `PATH`, `script/test-all` finds the pinned
  toolchain under `~/.rustup/toolchains`.
- For everything past `quick` except `fmt`, `rust-unit` and `python-unit`:
  PostgreSQL 17 or 18 with its server headers, `cargo-pgrx` 0.19.1 and
  `cargo pgrx init` pointed at that server. `script/test-all` uses the
  `pg_config` on `PATH` (or `PG_CONFIG`) and puts its `bindir` first on
  `PATH`, so `initdb`, `pg_ctl` and `psql` match it.
- For the conformance suite, the count fuzzer and the exit test: a Python
  with `psycopg` 3 and PyYAML, named by `STANNUM_PYTHON` (default `python3`),
  for example `python3 -m venv /tmp/stannum-venv && /tmp/stannum-venv/bin/pip
  install 'psycopg[binary]' pyyaml`.
- Everything else in Python needs only the standard library and `psql`.

### The pgrx lock

`cargo pgrx test` and `cargo pgrx install` replace the extension in the
server's directories, and `cargo pgrx test` uses one server per machine (port
28800 + the major version). On a machine where several checkouts share that,
wrap every command that installs the extension or uses the installed one:

```sh
script/pgrx-lock.py -- cargo pgrx test pg18 -p stannum
```

`script/test-all` takes the lock itself, once, for its `pgrx`, `install`,
`cluster`, `conformance` and `oracle` steps. The lock is reentrant, so it can
run inside an outer hold. `$STANNUM_PGRX_LOCK` names the lock file (default
`/tmp/stannum-pgrx.lock`). `cargo pgrx test` leaves a debug build with test
hooks installed; `script/test-all install` restores the release build.

## Every kind of test

"CI" is `.github/workflows/ci.yml`: the `lint` job, the `build-and-test`
matrix (x86-64 and arm64, PostgreSQL 17 and 18) and the `reference-oracle`
job. "Step" is the `script/test-all` step that runs it.

| kind | where | command | needs | runs in |
|------|-------|---------|-------|---------|
| Rust unit tests and proptests | `#[test]` and `proptest!` in `segment`, `tinql`, `tokenizer`, `boldi-vigna` | `cargo test --workspace --exclude stannum` | Rust | CI build-and-test; step `rust-unit` |
| Crate integration tests | `tinql/tests/`, `tokenizer/tests/` | the same command | Rust | the same |
| Parser differential test | `tinql/src/parser/differential.rs`: the descent parser against the retired pest grammar | the same command | Rust | the same |
| Heavier proptests | the property tests that read `PROPTEST_CASES` (`segment/src/random_tests.rs`, `tinql/src/runtime/plan.rs`, `boldi-vigna/src/phrase_plan.rs`) | `PROPTEST_CASES=5000 cargo test --release -p segment` | Rust | manual |
| pg_tests | `#[pg_test]` in `postgres/src`, including `tin_conformance.rs` (TIN 1.0.3's recorded answers) | `cargo pgrx test pg18 -p stannum` | pgrx; installs a pg_test build | CI build-and-test; step `pgrx` |
| Extension-crate `#[test]`s | plain `#[test]` in `postgres/src` (options, BM25, UDF helpers); they link against PostgreSQL, so only `cargo pgrx test` runs them, not `cargo test` | the same command | the same | the same |
| Crashes before publication | `postgres/tests/crash_before_publication.py`: crash hooks that exist only in the pg_test build | right after `cargo pgrx test`, under the same lock hold | the pg_test build installed | CI build-and-test; step `pgrx` |
| Cluster tests | `postgres/tests/`: `extension_upgrade.py`, `postings_lifecycle.py`, `merge_lifecycle.py`, `ranked_fuzz.py --smoke`, `vacuum_cleanup_pins.py`; and `benchmarks/mutation_targets.py` (the benchmark writers' target selection) | `python3 postgres/tests/<test>.py` | the release build installed; each starts its own throwaway cluster | CI build-and-test; step `cluster` |
| Conformance against TIN 1.0.3 | `conformance/`: 189 cases checked against `conformance/expected/tin-1.0.3` ([README](../conformance/README.md)) | `conformance/run.py --engine stannum --check conformance/expected/tin-1.0.3` | `STANNUM_PYTHON`; the release build installed | CI build-and-test (PostgreSQL 18); step `conformance`, in a throwaway cluster |
| Reference oracle | `script/reference-oracle`: 47 query shapes in five mutation states against PlanetScale's Lead, built as `tin` | `PGHOST=... PGPORT=... script/reference-oracle OUT` | a running server; a Lead checkout (`LEAD_REF_DIR`) | CI reference-oracle; step `oracle` |
| Cluster-test unit tests | `postgres/tests/test_*.py` (the fuzzer's query generation) | `python3 -m unittest discover -s postgres/tests -p 'test_*.py'` | Python | CI lint; step `python-unit` |
| Benchmark harness unit tests | `benchmarks/test_*.py` | `python3 -m unittest discover -s benchmarks -p 'test_*.py'` | Python | CI lint; step `python-unit` |
| Script unit tests and source headers | `script/test_*.py`; `script/source_headers.py` checks every license header against `source-provenance.json` | `python3 -m unittest discover -s script -p 'test_*.py'` | Python | CI lint; step `headers` |
| Formatting and lint | the workspace | `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --features "pg18 pg_test" -- -D warnings` | Rust; pgrx for Clippy | CI lint and build-and-test; steps `fmt`, `clippy` |
| Benchmark adapter tests | `benchmarks/tin/adapter.test.mjs` and the published driver's query tests | see the `lint` job in `ci.yml` | Node.js; network | CI lint |
| Benchmark harness smokes | the VACUUM workload and sustained mixed-write harnesses on tiny inputs | see `ci.yml` | the release build installed | CI build-and-test |
| Ranked-scan fuzzer, long runs | `postgres/tests/ranked_fuzz.py` (below) | `python3 postgres/tests/ranked_fuzz.py --seed 7 --seconds 600` | the release build installed | manual |
| Count fuzzer | `benchmarks/count_fuzz.py`: seeded AND/OR count queries under writes, against a Python evaluation of the same token sets, with forced page counting | `$STANNUM_PYTHON benchmarks/count_fuzz.py --output DIR` (`--replay DIR/<seed>-fixture.json` repeats one) | `STANNUM_PYTHON`; libpq variables naming a database where the role may create schemas and `stannum` is installed | manual |
| Backends that exit mid-walk | `postgres/tests/exit_during_walk.py` (below) | `benchmarks/local/exit-test.sh` | Docker; `STANNUM_PYTHON` | manual |
| Timing microprobes | `#[ignore]`d: `segment/tests/buffer_cost.rs` (`STANNUM_DOCS`, `STANNUM_COUNT`) and `grouped_record_ingestion_microprobe` in `segment/src/segment.rs`; they print timings and assert nothing | `cargo test --release -p segment -- --ignored --nocapture` | Rust | manual |

To add a test, put it where its kind lives above; a new Python cluster test
also goes into the `cluster` step of `script/test-all`, which is what CI and
the gates run.

## The ranked scan under concurrency

The ranked (top-k) scan keeps state across the life of a scan: the scorer it
built, the rows it pruned to, and the write-buffer index it retained. Two bugs
in one day (rows re-emitted after a deletion, fixed in `45d2696`; a retained
document scored with another document's length after a buffer refresh, fixed
in `9d3a7a0`) were caught only by the sustained-mutation benchmark, whose
checks are not an oracle. `postgres/tests/ranked_fuzz.py` is the oracle: a
randomized concurrency fuzzer for this class of bug.

### What the fuzzer does

It starts a throwaway cluster (install stannum first; standard library plus
`psql`, autovacuum off so every VACUUM is a scheduled step) and builds a small
corpus from a dozen templates, so exact score ties abound, in a table with a
random fillfactor. Several **writer sessions** then run random operations:
inserts, deletes, non-HOT updates (new body), HOT updates (a non-indexed
column), VACUUM, delete-VACUUM-insert cycles that reuse heap line pointers,
rolled-back inserts, multi-statement transactions, `keep`-table churn for
joins, and per-session tunables (`stannum.write_buffer_docs`,
`write_buffer_bytes`, `merge_tier_factor`, `max_segments`, `max_merge_docs`,
`build_segment_docs`) drawn from small values so folds, tiered merges and
deferred merges happen constantly. REINDEX and CREATE INDEX CONCURRENTLY run
only between reader transactions, so a lock wait cannot stall the scheduler.

**Reader sessions** run episodes. An episode opens a transaction (mostly
REPEATABLE READ, sometimes READ COMMITTED), pins its snapshot, lets writers
churn, and then, in one quiescent batch with no writer statement in flight:

1. runs the query through the unpruned path (`stannum.enable_custom_scan = off`,
   `ORDER BY score DESC, ctid`) for every matching row;
2. runs a regex sequential scan with the same predicate (`\mterm\M` per term,
   combined as the query combines them) and checks the match sets are equal;
3. reads the heap's HOT chains through `pageinspect`, since the scan orders a
   HOT-updated row by the root it posted while the executor projects the
   member's `ctid`;
4. runs the custom-scan query with `LIMIT`/`OFFSET`, or declares a cursor and
   fetches its first rows.

Statistics (document counts, lengths, frequencies) include buffered and
dead documents, so scores drift with every write; the quiescent batch is what
makes bit-for-bit comparison valid. A cursor then keeps fetching while writers
churn, sometimes with the fetch and a write overlapping in time, and its
whole output must equal the oracle's prefix: the scan retained its scorer, so
its scores must not move. A second cursor in the same transaction (same or
another query, its own oracle) checks that two scans do not share state.

Queries are single terms, `OR`, `AND` (two or three terms), boosts, phrases,
`AND NOT`, prefixes, `AT LEAST`, and `(a OR b) AND c`, scored by `full_score`
or `score`, with limits around the 128-posting block boundary and above the
4,096-row pruning cap, offsets, joins that read past k (`enable_hashjoin` and
`enable_mergejoin` off so the scan's order reaches the top; when a Sort still
sits above the scan, ties are compared tolerantly), and filters applied above
the scan.

Every result is checked for unique ids, finite scores, descending scores with
ties in the posting's heap order, membership in the regex match set, and
equality with the oracle's slice (ids, ctids and score text with
`extra_float_digits = 3`, which round-trips float4 exactly).

On a failure the fuzzer writes `failure.json` and `repro.sql` into its
artifact directory: the seed and arguments, the failing comparison, and every
statement issued, in order, labelled by session. Re-running with the same
`--seed`, `--seconds`, `--writers`, `--readers` and `--corpus` repeats the
scenario and random seed, but concurrent scheduling and the timed cutoff can
change the exact execution. Retain the failing trace for diagnosis;
`--stop-at N` stops after episode N.

```
python3 postgres/tests/ranked_fuzz.py --seed 7 --seconds 600
python3 postgres/tests/ranked_fuzz.py --smoke     # fixed seeds and REGRESSIONS
```

`--smoke` runs one fixed seed and the `REGRESSIONS` list in the script (each
a short configuration that once failed or pins a bug class) in under two
minutes; CI runs it in the `cluster` step.

### Bug classes it found

- **HOT-updated rows scored zero.** The index posts the root of a HOT chain;
  the executor projects the visible member's `ctid`; `score_bound_indexed`
  looked that location up in the index, found nothing, and returned zero on
  both paths. The pruned scan then ordered such a row by its real score while
  reporting zero. `IndexScorer::score` now resolves a location absent from
  every source to its chain root (`heap_get_root_tuples` under a share lock).
  Test: `hot_updated_rows_keep_their_score_on_both_ranked_paths`.
- **Scans on one query shared a scorer.** The scan handed its scorer to the
  score functions through a slot keyed by a backend-wide statement counter,
  so a later scan (or an unpruned query) on the same query replaced it, and a
  cursor's remaining rows were projected with statistics that writes in
  between had changed, out of step with the order it ranked them in. Scans
  now publish their scorer under their own identity with the score of every
  row they ranked and record the row they emitted last, with the statement
  number and an emission stamp; a score call in that statement for exactly
  that location takes the scan's score, newest emission first (any other
  location, such as an unpruned scan's row while a cursor is open on the
  same query, or a row a paused cursor emitted in an earlier statement, is
  scored by the statement's own scorer), and the entry is dropped when the
  scan ends, including after an error. The fuzzer's first three attempts at
  this fix (newest scan wins; most recent emitter wins; exact location
  without the statement scope) each failed the same seed within three
  seconds, which is the point of running it.
  Test: `concurrent_cursors_on_one_query_keep_their_own_scores`.

The earlier two bugs remain covered by
`a_completed_ranked_scan_does_not_repeat_the_rows_it_emitted` and
`buffered_scoring_keeps_document_lengths_when_heap_space_is_reused`, and by
the fuzzer's cursor episodes, which exercise both interleavings continuously.

### Wide disjunction cursors

`--wide` uses 31, 32, 33 and 128 distinct alphabetic terms with positive boosts,
cycling across the grouped-pivot threshold. Documents mix dense 128-term bodies
with sparse 32-term bodies. Queries use `full_score` so common-term elision does
not remove the intended scoring terms. Episodes use cursors or two open cursors,
with limits around the posting-block boundary, offsets, filters and joins.

```sh
python3 postgres/tests/ranked_fuzz.py --wide --seed 104 --seconds 30 \
  --corpus 400 --writers 2 --readers 2
```

The existing quiescent oracle capture and subsequent concurrent writer churn
remain unchanged. An extra EXPLAIN ANALYZE checks ordinal pruning while the
first cursor is open. This also exercises another statement's scorer without
allowing it to change the retained cursor's scores. Reports count exercised
widths; missing any of the four widths fails the run rather than silently passing
an empty or incomplete campaign. A fixed wide scenario is included in `--smoke`.
These are correctness stress tests, not performance measurements.

Local validation of the wide mode: seeds 104/105 completed 204 comparisons,
426 cursor fetches, 1,480 writer operations and 244 VACUUM operations, with all
four widths exercised and no skipped comparisons. The six-scenario smoke suite
passed 912 comparisons. These counts are scheduling-dependent observations,
not fixed expected counts.

### Backends that exit mid-walk

A FATAL error (`pg_terminate_backend`, a fast shutdown, postmaster death)
exits a backend through `proc_exit` without unwinding the ranked walk it
interrupts. PostgreSQL releases the walk's pins and relation reference
itself; on Linux `exit` then runs the backend's thread-local destructors,
which drop the cached segment readers after `CurrentResourceOwner` is gone.
`postgres/tests/exit_during_walk.py` terminates 40 sessions, each busy with
ranked disjunctions, conjunctions and phrases, and then fast-stops the server
under four more. It fails on a changed postmaster start time, a lost control
connection, or any crash, restart, WARNING, ERROR or unexpected FATAL in the
server log. It needs a Linux server, so it runs in Docker (a few minutes,
most of it the image build):

```sh
STANNUM_PYTHON=/tmp/stannum-venv/bin/python benchmarks/local/exit-test.sh
# STANNUM_EXIT_IMAGE=... reuses an image built earlier
```

The table has 4.6 million documents because a walk checks for interrupts
only every 64 chunks of 65,536 rows. Below that, every termination lands
between walks. At this size most of them land with 7 to 17 pages held. The
real exit could not be made to crash on 36ba97f: the walked readers stay
referenced by the executor frame that the FATAL abandons, so their
destructors never run at exit. The ones that do run at exit hold nothing.
`a_reader_dropped_at_exit_leaves_its_pins_to_postgres` pins the contract
directly on any platform: a reader dropped during `proc_exit` with a span
open leaves its pin and relation to PostgreSQL.
