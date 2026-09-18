# Round-four recovery and integration validation

Recovered on 2026-09-18 from thread `f9abeb36-a199-4fbf-9954-b9fd6e94fb40`
(Claude session `8df69e6c-7a2c-47a7-8bb4-0d1c16ea42c6`).
Integration branch: `resume/round4-validation`, based on `ad5acf7`.

## State at interruption

The parent stopped on a provider usage-limit error while agent work was still
unfinished. Three of the six round-four tasks had reached published main:

| Work | State recovered |
| --- | --- |
| LSG3 segment compression | Landed as `d968a6e` |
| Hot-standby index reads and removal-horizon WAL | Landed as `592a5c1`; portability and SQL snapshot fixes `008b2fa`, `4f84ea9` |
| Tokenizer binding and partial-index predicates | Landed as `ad5acf7` |
| VACUUM publication, merge budgets and orphan reclamation | Agent tip `b3a8dee`; worktree left in a merge conflict |
| Term/dead-set caches and compact mutable positions | Agent tip `e10d5c3`; complete locally, not integrated |
| Concurrent ranked-scan fuzzer and scoring fixes | Agent tip `9a0fe67`; worktree left in a merge conflict; unfinished validation |

The [last published CI run](https://github.com/TeamSpringbird/stannum/actions/runs/35385526672)
passed for `ad5acf7`. This covers that base, not the subsequent integration.
The original fuzzer campaign recorded passes for seeds 1 and 2; seed 3 left
an empty output file, so it is not counted as a pass.

## Integration

The three remaining committed agent branches were squash-integrated into a new
worktree. The original worktrees, including their uncommitted merge states, were
preserved. Conflicts retain the tokenizer initialization and permission fixes,
the WAL round-trip's `pg_test` annotation, unlocked VACUUM with reclaim horizons,
and the new term/dead-set caches. Tests from all branches are retained.

The new HOT regression initially failed. Its custom scan broke score ties by
indexed HOT-root location, while its reference query ordered by visible `ctid`;
the updated row therefore moved outside the reference's tied LIMIT. The reference
now orders by fixture ID, which follows original root order. A later assertion
used a flattened self-join outside the supported scoring query context; it now
compares the two relevant rows from the ordinary scored result. The test still
requires matching score bits and positive scores for both updated documents.
No production scoring behavior was changed to accommodate those test failures.

## Validation protocol

Use the pinned Rust toolchain, pgrx 0.19.1, and the installed PostgreSQL 17/18
versions. On the shared development machine every test/install/measurement that
touches the PostgreSQL extension installation holds `/tmp/stannum-pgrx.lock`.
Always reinstall a release build after pgrx tests before measuring performance.

```sh
cargo fmt --all -- --check
cargo test --locked --workspace --exclude stannum
python3 -m unittest discover -s benchmarks -p 'test_*.py'
for version in 17 18; do
  cargo clippy --locked --workspace --all-targets --no-default-features \
    --features "pg$version pg_test" -- -D warnings
  cargo pgrx test "pg$version" --package stannum --no-default-features \
    --features "pg$version pg_test"
done
cargo pgrx install --release --package stannum --no-default-features --features pg18
python3 postgres/tests/extension_upgrade.py
python3 postgres/tests/postings_lifecycle.py
python3 docs/benchmarks/merge_lifecycle.py
python3 postgres/tests/ranked_fuzz.py --smoke
python3 postgres/tests/ranked_fuzz.py --seed 2 --seconds 240 --writers 3 --readers 2 --corpus 1500 --keep
python3 postgres/tests/ranked_fuzz.py --seed 3 --seconds 240 --writers 4 --readers 3 --corpus 4500 --keep
# Point these variables at a dedicated local server and upstream Lead checkout:
PGHOST=localhost PGPORT=28989 LEAD_REF_DIR=/tmp/stannum-lead-ref script/reference-oracle
```

## Correctness results

Code tested: `0ab416a`. Local platform: ARM64 macOS, PostgreSQL 17.11 and 18.6.

| Check | Result |
| --- | --- |
| Formatting and Clippy, PG17 and PG18 with `pg_test` | Passed, warnings denied |
| Core workspace tests | 434 unit tests and 4 doc tests passed; 3 optional probes/fixture writers ignored |
| Python harness tests | 44 passed |
| PostgreSQL extension tests | 102 passed on each major |
| Release schema snapshot and drift | Passed; zero upgrade paths currently defined |
| Lifecycle/recovery | Passed: 40 verifier calls, 337 correct standby answers, zero wrong answers; one expected recovery conflict |
| Maintenance/crash scenario | Passed; 355 orphaned pages reported after interrupted publication, then reclaimed and reused |
| Concurrent ranked smoke | Five scenarios, 851 comparisons passed |
| Seed 2: 240 seconds, 3 writers, 2 readers, 1,500 rows | 550 comparisons, 105,153 rows, 3,408 writer operations, 531 VACUUMs; passed |
| Seed 3: 240 seconds, 4 writers, 3 readers, 4,500 rows | 282 comparisons, 68,302 rows, 1,723 writer operations, 294 VACUUMs; passed |
| Local upstream Lead oracle | 235 query/state pairs agreed; zero differences; Lead `0e29dbe` |

All seven local fuzz scenarios total 1,683 comparisons and 248,847 rows, with
zero skipped comparisons. Seed 3 recorded one expected PostgreSQL transaction
deadlock between concurrent multi-row DELETE and UPDATE statements; its server
log confirms transaction ShareLocks, and the fuzzer rolled back and continued.
This is not a claim that conflicting application transactions never need retries.

[Combined-build CI](https://github.com/TeamSpringbird/stannum/actions/runs/35386851639)
passed all six jobs at `0ab416a`: formatting/harness, the upstream Lead oracle,
and x86-64/ARM64 crossed with PostgreSQL 17/18. Each matrix job includes release
schema, lifecycle/recovery and concurrency smoke checks.

Raw local logs, CI metadata, the two release binaries, and measurement artifacts
are retained in ignored `benchmarks/results/round4-validation/`, copied from
`/tmp/stannum-resume-validation/`. They include the initial test failures.

## Paired performance protocol

Compare published `ad5acf7` with integrated `0ab416a`, using release binaries
from their respective worktrees and the existing `benchmarks/run.py` harness.
This measures the three newly integrated branches together. LSG3 and the earlier
standby/tokenizer changes are present in both builds.

- The same verified 100,000-document Wikipedia dataset on the same local ARM64
  PostgreSQL 18.6 server, `shared_buffers=512MB`, `maintenance_work_mem=256MB`.
  Native development machine, no container CPU/memory limits.
- Five mixed-workload pairs, alternating baseline/changed order between pairs;
  30-second measurement windows after 5-second warmups, two readers and 20
  updates/second. Each run creates a fresh database and index.
- One additional mutation run per build: 120 seconds after 5 seconds of warmup,
  two readers, 50 mutations/second, equal insert/delete/update weights; oracle
  checks and scheduled VACUUM every 30 seconds.
- Binary SHA256 checked before and after each window; the installation lock
  covers the entire campaign. The integrated binary is restored afterward.
- Match-set and ranked-result checks run before and after mixed traffic;
  mutation runs use the changing-corpus checks. These benchmark checks supplement
  the stricter top-k comparisons in the fuzzer; they do not replace them.

These are short, warm-cache development measurements. Five repeated pairs help
assess ordinary variability; one mutation pair does not establish a tail-latency
distribution or sustained production performance.

### Mixed-workload results

Reader throughput includes count and ranked queries; writer throughput was about
19.8 updates/second in every run. All ten runs passed before/after correctness
checks and reported no transaction failures.

| Pair | Baseline queries/s | Integrated queries/s | Change |
| --- | ---: | ---: | ---: |
| 1, baseline first | 2,665.6 | 2,854.1 | +7.07% |
| 2, integrated first | 2,706.3 | 2,873.0 | +6.16% |
| 3, baseline first | 2,707.3 | 2,898.1 | +7.05% |
| 4, integrated first | 2,670.5 | 2,846.2 | +6.58% |
| 5, baseline first | 2,674.1 | 2,886.2 | +7.93% |
| Median | 2,674.1 | 2,873.0 | +7.44% |

The integrated build improved throughput in all five pairs on this workload.
This does not isolate the cache optimization from the other integrated changes,
nor establish a speedup over TIN or over the older pre-LSG3 build.

Selected median per-query p99 values across the five windows, in milliseconds:

| Query shape | Baseline | Integrated |
| --- | ---: | ---: |
| Common-term count | 0.874 | 0.820 |
| Common-term ranked | 0.919 | 0.894 |
| AND ranked | 3.954 | 3.628 |
| OR ranked | 0.743 | 0.632 |

### Mutation results

| Metric | Baseline | Integrated |
| --- | ---: | ---: |
| Reader queries/second | 2,365.6 | 2,631.9 |
| Writer mutations/second | 50.60 | 50.59 |
| Periodic correctness rounds | 4 passed | 4 passed |
| Scheduled VACUUMs | 4 | 4 |
| Insert p99 / maximum (ms) | 10.675 / 71.005 | 11.129 / 66.759 |
| Delete p99 / maximum (ms) | 10.464 / 21.419 | 10.781 / 2.720 |
| Update p99 / maximum (ms) | 11.120 / 79.068 | 11.326 / 65.455 |

Both runs passed the final correctness round as well, with no reader or writer
transaction failures. Read throughput was 11.3% higher in the integrated run.
Mutation p99 is latency from the scheduled start; the maximum is the harness's
largest per-bucket execution time after subtracting schedule lag. They measure
different quantities. Maximum observed mutation times fell, but scheduled p99s
rose slightly. One pair does not justify claiming a general write-latency gain.
This short run also does not exercise all long-term merge-tier behavior; the
separate maintenance/crash checks and the agent's longer
[VACUUM experiment](vacuum-publication.md) supply additional evidence.

Every measurement window preserved its installed binary hash. The integrated
release library was restored and its SHA256 verified after the campaign:
`73ecbdfa7417c101fd05d1aabd50ad499eeac4b46b747e6fa578a1078ef12ed6`.
`performance/builds.json` in the artifacts records both hashes and full commits.

## Resulting state

All six round-four tasks are represented in the integration branch. The code at
`0ab416a` passed local and Linux matrix validation and the paired campaign above.
The follow-up documentation records those results; it does not change that code.
The validation branch is pushed for review. Published main, releases and tags
were not changed by this recovery task. Original agent worktrees remain intact.
