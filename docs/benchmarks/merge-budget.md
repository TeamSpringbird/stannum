# Insert merge budget and deferred maintenance

This experiment compares clean `45d2696` with the storage changes in this
branch. Inserts now budget ordinary merge input documents, defer larger tiers
to VACUUM, and fold at 512 documents or 1 MiB of encoded forward records.
VACUUM constructs deferred segments outside the meta lock, then validates the
captured directory entries before publishing them. The architecture document
explains the emergency directory-overflow exception and remaining lock waits.

## Protocol and limits

- PostgreSQL 18.6 on the shared local pgrx server, localhost:28818; release builds.
- Verified Wikipedia 100,000-document corpus at
  `$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-100000`.
- `python3 benchmarks/run.py run --profile mutation`, 600-second timed windows,
  30-second warmup, two readers, 50 scheduled mutations/second, equal insert /
  delete / update weights, seed 1729, checks every 30 seconds, scheduled
  `VACUUM (INDEX_CLEANUP ON)` every 60 seconds, layout samples every 5 seconds.
- A final after-build isolation run restores the old buffer caps while keeping
  the new merge policy, to separate fold-size effects from merge deferral.
- Default storage settings and a stress configuration with
  `write_buffer_docs=256, merge_tier_factor=4`, applied using `--set`.
- A machine-wide lock covers every shared PostgreSQL install, test, oracle and
  entire measurement window. Each mode starts in a fresh `stannum_bench_*`
  database. Binary SHA256 checks bracket measurements; the saved baseline
  binary and its embedded SQL metadata permit exact baseline restoration.
- One window per configuration on a shared development machine, warm caches,
  no randomized run order. These are directional measurements, not confidence
  intervals or production latency guarantees.

The original **default custom-scan baseline failed correctness** at 282.56
seconds, before its first large merge. The AND query's ranked top ten ended
with score `7.0131845` after `6.7479887`, violating the descending-order check;
all ten IDs were inside the oracle set. Eight earlier check rounds passed.
That run used the untouched baseline release, and its recorded source patch
is empty. It is not treated as a completed performance comparison.

The matched runs therefore all use `--set stannum.enable_custom_scan=off`.
This preserves the index, mutations, folds and VACUUM, while PostgreSQL's
bitmap/executor path handles the reads and sorting. The results establish
storage behavior and bitmap-reader impact; they do not establish default
custom-scan performance or fix its separate concurrent ranking defect.

Mutation p99 and maxima below are **execution latency**, subtracting pgbench's
schedule lag exactly as the harness timeline does. Reader p99 combines all
queries of the named shape. Minute timelines show the last sampled immutable
segment count; that sample need not coincide with a minute boundary. Final
partial buckets contain only traffic finishing after the nominal 600 seconds
and are excluded from minute-to-minute comparisons.

## Measurements

All values are milliseconds unless stated otherwise. Each `p99 / max` pair
is over the entire timed run, including statements that finish while traffic drains.

| Run | Insert p99 / max | Delete p99 / max | Update p99 / max |
| --- | ---: | ---: | ---: |
| Before, default | 6.909 / 2909.452 | 5.889 / 21.834 | 6.842 / 304.459 |
| After, default | 6.636 / 149.356 | 5.410 / 19.131 | 7.341 / 137.456 |
| Before, stress | 6.639 / 895.725 | 4.973 / 15.415 | 6.550 / 1098.025 |
| After, stress | 6.650 / 178.990 | 4.798 / 19.075 | 7.406 / 303.068 |
| After, old buffer caps | 6.100 / 427.382 | 4.869 / 17.822 | 6.490 / 404.776 |

| Run | Reader count p99 | Reader ranked p99 | Reads/s | Segments min–max / final | Max sampled buffer docs | Oracle rounds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Before, default | 6.310 | 11.609 | 870.4 | 4–10 / 6 | 2125 | 20 |
| After, default | 6.174 | 11.547 | 868.9 | 4–20 / 14 | 510 | 20 |
| Before, stress | 6.150 | 11.625 | 875.9 | 4–12 / 10 | 253 | 20 |
| After, stress | 6.088 | 11.665 | 875.0 | 4–19 / 10 | 253 | 20 |
| After, old buffer caps | 6.072 | 11.424 | 891.9 | 4–11 / 6 | 2121 | 20 |

Every completed run also passed the final post-traffic oracle round. No result
sets differed from the regex sequential-scan oracle and every checked ranked
top ten satisfied the harness invariants. Binary hashes were unchanged across
each window.

### Default storage: reader p99 and segment count over time

| Window start (s) | First run count / ranked p99 | First run segments | Second run count / ranked p99 | Second run segments |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 5.696 / 9.133 | 4 | 5.403 / 8.939 | 7 |
| 60 | 5.629 / 9.571 | 5 | 5.651 / 9.780 | 11 |
| 120 | 5.722 / 10.255 | 6 | 5.913 / 10.408 | 15 |
| 180 | 8.250 / 13.307 | 7 | 6.133 / 11.088 | 19 |
| 240 | 6.176 / 11.341 | 8 | 5.800 / 10.933 | 8 |
| 300 | 6.645 / 11.791 | 9 | 6.138 / 11.493 | 12 |
| 360 | 6.326 / 11.844 | 10 | 6.427 / 11.751 | 16 |
| 420 | 6.290 / 11.801 | 4 | 6.634 / 11.967 | 20 |
| 480 | 6.177 / 11.710 | 5 | 6.464 / 11.761 | 10 |
| 540 | 6.558 / 12.374 | 6 | 6.582 / 12.806 | 14 |

First run: `before-default-bitmap`. Second run: `after-default-bitmap`.

### Stress storage: reader p99 and segment count over time

| Window start (s) | First run count / ranked p99 | First run segments | Second run count / ranked p99 | Second run segments |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 5.506 / 8.988 | 8 | 5.331 / 8.845 | 8 |
| 60 | 5.557 / 9.800 | 9 | 5.551 / 9.883 | 12 |
| 120 | 5.642 / 10.023 | 8 | 5.673 / 10.415 | 14 |
| 180 | 5.747 / 10.614 | 9 | 5.886 / 10.813 | 12 |
| 240 | 6.042 / 10.916 | 8 | 6.085 / 11.178 | 14 |
| 300 | 6.065 / 11.150 | 9 | 6.155 / 11.404 | 15 |
| 360 | 6.355 / 11.786 | 11 | 6.285 / 11.876 | 17 |
| 420 | 6.246 / 11.591 | 9 | 6.502 / 12.133 | 18 |
| 480 | 6.424 / 12.207 | 11 | 6.592 / 12.202 | 11 |
| 540 | 6.714 / 12.752 | 10 | 6.328 / 12.316 | 10 |

First run: `before-stress-bitmap`. Second run: `after-stress-bitmap`.

### Buffer-cap isolation: reader p99 and segment count over time

| Window start (s) | First run count / ranked p99 | First run segments | Second run count / ranked p99 | Second run segments |
| ---: | ---: | ---: | ---: | ---: |
| 0 | 5.347 / 8.849 | 4 | 5.403 / 8.939 | 7 |
| 60 | 5.583 / 9.598 | 5 | 5.651 / 9.780 | 11 |
| 120 | 5.602 / 10.246 | 6 | 5.913 / 10.408 | 15 |
| 180 | 5.726 / 10.747 | 7 | 6.133 / 11.088 | 19 |
| 240 | 5.944 / 10.889 | 8 | 5.800 / 10.933 | 8 |
| 300 | 6.189 / 11.387 | 9 | 6.138 / 11.493 | 12 |
| 360 | 6.204 / 11.738 | 10 | 6.427 / 11.751 | 16 |
| 420 | 6.346 / 11.986 | 11 | 6.634 / 11.967 | 20 |
| 480 | 6.378 / 11.857 | 5 | 6.464 / 11.761 | 10 |
| 540 | 6.478 / 12.112 | 6 | 6.582 / 12.806 | 14 |

First run: `after-legacy-buffer-bitmap`. Second run: `after-default-bitmap`.

### Measured artifacts

| Build | SHA256 |
| --- | --- |
| 45d2696:release | `c062df00001bf982b4d26017221fbfa46da090d5718d8afa0d2146dedd6ef256` |
| 45d2696+merge-budget:release | `c39becfb4b24f29f9f2c42cdc41be88ab4a74807fd68da6f0cb7e46eca04f30b` |

## Interpretation

With default storage settings, the observed worst insert fell from 2,909.452
ms to 149.356 ms (19.5× lower). Under stress it fell from 895.725 ms to
178.990 ms (5.0× lower). Worst updates also improved, from 304.459 to 137.456
ms by default and 1,098.025 to 303.068 ms under stress.

This improves rare stalls, not every latency statistic: insert p99 was nearly
unchanged, while update p99 increased by 0.499 ms by default and 0.856 ms under
stress. Reader p99 and aggregate throughput remained nearly unchanged in the
matched windows; median reader latency was slightly higher. More deferred
segments were visible: peak counts rose from 10 to 20 by default and from 12
to 19 under stress. One window per case cannot exclude small read regressions.

The buffer-cap isolation run keeps the new merge policy but restores the old
16,384-document/4 MiB caps. Its worst insert/update were 427.382/404.776 ms,
versus 149.356/137.456 ms with the smaller defaults: another 2.9× reduction.
The maximum sampled buffer shrank from 2,121 to 510 documents. However, the
smaller buffer's reader count/ranked p99 were 1.7%/1.1% higher and throughput
was 2.6% lower than the old-cap run. That is a modest observed read tradeoff,
not proof of zero reader cost; the smaller defaults prioritize stall latency.
Both caps remain tunable, and repeated trials are needed to distinguish these
small reader differences from shared-host variation. The
[buffer-index note](buffer-index.md) profiles that reader cost, attributes it
to per-segment query setup rather than the buffer, removes most of it, and
re-measures the caps.

The ceiling is on ordinary merge input documents per fold, not elapsed time.
Directory overflow, large individual records, run copying/publication and
pre-existing VACUUM dead-list/rewrite/reclamation work can still delay an
insert. Timely maintenance remains necessary; these measurements do not turn
149 ms into a guaranteed maximum.

## Verification

- `cargo fmt --all --check`.
- PG18 and PG17 workspace/all-target clippy with `pg_test`, warnings denied.
- Non-extension workspace tests: 420 passed; two pre-existing ignored tests.
- `cargo pgrx test pg18 --no-default-features --features "pg18 pg_test"`:
  78 passed, including ceiling/cleanup, directory overflow with a zero budget,
  oversized-record byte-cap behavior and merge-policy budget coverage.
- Existing `postgres/tests/postings_lifecycle.py`: passed, 34 index-verifier
  calls, including concurrent readers/writers, snapshots, WAL recovery,
  standby/promotion, reclamation, reindex and cancellation.
- `docs/benchmarks/merge_lifecycle.py`: private cluster without preload,
  insert-triggered autovacuum, concurrent emergency/deferred merges, a retained
  scan, immediate shutdown/WAL recovery and heap/index comparison.

The private-cluster scenario can be repeated after a release install:

```sh
python3 /tmp/stannum-pgrx-lock.py -- python3 docs/benchmarks/merge_lifecycle.py
```

Raw local artifacts are in `/tmp/stannum-merge-results/`, including manifests,
per-statement logs, oracle plans/results, layout samples, VACUUM outputs and
per-minute timelines. The failed initial custom-scan run is `before-default`;
matched completed runs have a `-bitmap` suffix.
`before-stress-bitmap-setup-failed` preserves a corrected control-file template
restoration error before extension creation; it contains no timed traffic.
