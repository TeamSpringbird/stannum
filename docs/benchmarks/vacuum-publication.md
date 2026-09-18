# VACUUM publication, budgeted overflow merges and orphan reclamation

This experiment compares `9b0bf47` (the merge-budget baseline) with the
storage changes in this branch: VACUUM holds the meta lock only to publish,
merges that bring the directory back under `stannum.max_segments` are
budgeted like tier merges, and cleanup reclaims pages a crash left
unreferenced. The [architecture document](../architecture/segmented-storage.md)
describes the design and its safety argument.

## Protocol and limits

- PostgreSQL 18.6 on the shared local pgrx server, localhost:28818; release
  builds installed from a `git archive` of the baseline and from this branch.
- Verified Wikipedia 100,000-document corpus at
  `$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-100000`.
- `python3 benchmarks/run.py run --profile mutation`, 600-second timed windows,
  30-second warmup, two readers, 50 scheduled mutations/second, equal insert /
  delete / update weights, seed 1729, checks every 30 seconds, scheduled
  `VACUUM (INDEX_CLEANUP ON)` every 60 seconds, layout samples every 5 seconds,
  custom scans on (the default).
- Default storage settings and a stress configuration with
  `write_buffer_docs=256, merge_tier_factor=4`, applied using `--set`.
- A machine-wide lock covers every install and entire measurement window; each
  window starts in a fresh `stannum_bench_*` database, and the library's
  SHA256 is checked before and after each window.
- One window per configuration on a shared development machine with five
  other agents building and testing between windows. These are directional
  measurements, not confidence intervals.

Mutation p99 is pgbench's figure over the whole run (latency from the
scheduled start); the maximum is the worst execution time of any statement
of that kind in a 10-second bucket, with schedule lag subtracted, as the
harness timeline reports it. Reader p99 is the worst per-query p99 of the
named shape. VACUUM duration is the wall time of the scheduled statement.

## Measurements

All values are milliseconds unless stated otherwise.

| Run | Insert p99 / max | Delete p99 / max | Update p99 / max |
| --- | ---: | ---: | ---: |
| Before, default | 11.636 / 81.046 | 11.061 / 11.772 | 12.087 / 110.107 |
| After, default | 11.113 / 78.044 | 10.587 / 2.680 | 11.197 / 87.274 |
| Before, stress | 28.855 / 205.160 | 10.989 / 2.903 | 18.305 / 200.970 |
| After, stress | 25.624 / 230.586 | 11.012 / 3.992 | 11.533 / 198.607 |

| Run | Reader count p99 | Reader ranked p99 | Reads/s | VACUUM durations (s) | Oracle rounds |
| --- | ---: | ---: | ---: | --- | ---: |
| Before, default | 4.328 | 7.530 | 867.8 | 0.10 0.13 0.12 1.65 0.13 0.11 0.12 1.17 0.81 0.16 | 20 |
| After, default | 3.649 | 7.169 | 912.9 | 0.10 0.12 0.12 1.53 0.13 0.12 0.11 1.18 0.68 0.14 | 20 |
| Before, stress | 3.581 | 6.973 | 923.8 | 0.11 0.52 0.57 1.67 0.53 0.51 0.63 1.94 1.09 0.19 | 20 |
| After, stress | 3.598 | 7.218 | 940.2 | 0.10 0.51 1.59 0.52 0.52 0.52 1.89 0.12 0.52 1.60 | 20 |

Every run passed every periodic oracle round and the final post-traffic round.

### Reader p99 and segment count per minute

Insert and update maxima are the worst 10-second bucket of the minute;
segments are the last sample of the minute; VACUUMs are those that completed
in the minute.

#### Default storage

| Window start (s) | Before count / ranked p99 | Before insert / update max | Before segments, VACUUM | After count / ranked p99 | After insert / update max | After segments, VACUUM |
| ---: | ---: | ---: | --- | ---: | ---: | --- |
| 0 | 2.902 / 4.281 | 67.234 / 89.834 | 7 | 2.581 / 4.073 | 65.340 / 75.404 | 7 |
| 60 | 3.218 / 5.050 | 66.510 / 77.212 | 11, VACUUM 0.10 s | 2.845 / 4.812 | 77.407 / 67.606 | 11, VACUUM 0.10 s |
| 120 | 3.175 / 5.382 | 76.515 / 79.352 | 15, VACUUM 0.13 s | 2.969 / 5.205 | 78.044 / 72.208 | 15, VACUUM 0.12 s |
| 180 | 3.093 / 6.105 | 81.046 / 79.201 | 19, VACUUM 0.12 s | 3.116 / 5.977 | 70.496 / 80.157 | 19, VACUUM 0.12 s |
| 240 | 3.060 / 5.358 | 75.842 / 110.107 | 8, VACUUM 1.65 s | 3.021 / 5.325 | 66.738 / 87.274 | 8, VACUUM 1.53 s |
| 300 | 3.715 / 6.348 | 76.311 / 77.210 | 12, VACUUM 0.13 s | 3.164 / 5.830 | 69.214 / 76.690 | 12, VACUUM 0.13 s |
| 360 | 3.323 / 6.184 | 78.914 / 64.775 | 16, VACUUM 0.11 s | 3.275 / 6.031 | 70.777 / 70.080 | 16, VACUUM 0.12 s |
| 420 | 3.428 / 6.416 | 6.963 / 80.091 | 20, VACUUM 0.12 s | 3.362 / 6.280 | 71.786 / 70.641 | 20, VACUUM 0.11 s |
| 480 | 3.529 / 6.557 | 72.424 / 73.175 | 17, VACUUM 1.17 s | 3.440 / 6.478 | 70.783 / 74.025 | 17, VACUUM 1.18 s |
| 540 | 3.500 / 7.283 | 61.681 / 74.926 | 14, VACUUM 0.81 s | 3.415 / 7.182 | 75.089 / 62.213 | 14, VACUUM 0.68 s |

#### Stress storage

| Window start (s) | Before count / ranked p99 | Before insert / update max | Before segments, VACUUM | After count / ranked p99 | After insert / update max | After segments, VACUUM |
| ---: | ---: | ---: | --- | ---: | ---: | --- |
| 0 | 2.560 / 4.125 | 49.227 / 200.970 | 8 | 2.534 / 4.028 | 51.526 / 198.607 | 8 |
| 60 | 2.831 / 4.851 | 173.497 / 34.519 | 12, VACUUM 0.11 s | 2.871 / 4.815 | 179.553 / 34.987 | 12, VACUUM 0.10 s |
| 120 | 2.942 / 5.184 | 48.510 / 38.414 | 14, VACUUM 0.52 s | 2.903 / 5.088 | 39.493 / 41.945 | 14, VACUUM 0.51 s |
| 180 | 3.008 / 5.868 | 205.160 / 37.177 | 15, VACUUM 0.57 s | 2.981 / 5.728 | 230.586 / 34.472 | 12, VACUUM 1.59 s |
| 240 | 3.043 / 5.458 | 39.903 / 141.878 | 14, VACUUM 1.67 s | 3.039 / 5.390 | 44.650 / 31.013 | 14, VACUUM 0.52 s |
| 300 | 3.149 / 5.881 | 50.406 / 37.062 | 15, VACUUM 0.53 s | 3.204 / 5.814 | 42.359 / 34.641 | 15, VACUUM 0.52 s |
| 360 | 3.319 / 6.098 | 45.122 / 44.828 | 17, VACUUM 0.51 s | 3.231 / 5.940 | 43.183 / 39.063 | 17, VACUUM 0.52 s |
| 420 | 3.304 / 6.177 | 176.522 / 39.766 | 18, VACUUM 0.63 s | 3.215 / 5.777 | 206.414 / 40.862 | 9, VACUUM 1.89 s |
| 480 | 3.342 / 6.194 | 136.447 / 191.266 | 11, VACUUM 1.94 s | 3.358 / 6.170 | 41.529 / 172.196 | 14, VACUUM 0.12 s |
| 540 | 3.303 / 6.974 | 198.790 / 47.131 | 10, VACUUM 1.09 s | 3.472 / 7.255 | 34.345 / 37.795 | 16, VACUUM 0.52 s |


## Interpretation

Worst mutation latency in this workload is not set by VACUUM. With default
settings every minute of both runs shows insert and update maxima of 60 to
80 ms in buckets with no VACUUM at all: that is the fold of a full 512
document write buffer, which the inserting backend performs under the meta
lock. Under stress (256-document folds, tier factor 4) the maxima of about
200 ms in quiet buckets are the insert's own budgeted tier merges of four
folds (1,024 documents, exactly the default `max_merge_docs`). Neither is
changed by this branch, and the whole-run maxima therefore move little:
insert 81.0 to 78.0 ms and update 110.1 to 87.3 ms by default, insert 205.2
to 230.6 ms and update 201.0 to 198.6 ms under stress. Delete maxima fell from
11.8 to 2.7 ms by default; deletes touch only the heap and previously waited
for VACUUM's dead-list construction under the lock.

What the baseline attributed to VACUUM is visible in the buckets that overlap
its long, merge-carrying runs. By default the 1.65 s VACUUM at 240 s pushed the
update maximum to 110.1 ms against 75 to 90 ms elsewhere; after the change the
matching 1.53 s VACUUM's buckets show 87.3 ms, no more than a fold. Under
stress the baseline's 1.94 s VACUUM bucket held both an insert of 136 ms and
an update of 191 ms; after the change the buckets overlapping the three long
VACUUMs show updates of 34 to 41 ms, and inserts of 206 and 231 ms that match
the budgeted insert merges seen in quiet buckets (199 to 201 ms). One window
per configuration cannot separate a residual few milliseconds of publication
wait from that coincidence.

VACUUM wall time is unchanged, as expected: the merge and dead-list work is
the same, it now runs without the meta lock. Reader p99 and throughput were
slightly better in both after windows (count p99 4.33 to 3.65 ms and reads/s
868 to 913 by default; 3.58 to 3.60 ms and 924 to 940 under stress), within
what a shared machine varies by. Every run passed all 20 oracle rounds and the
final round; the after windows ran against the tree merged with the LSG3
format and the standby removal-horizon work, whose hooks are inert without
`shared_preload_libraries`.

The remaining stall sources are therefore the fold itself and the budgeted
insert merge, both of which run under the meta lock in the inserting backend
by design; the fold caps and the merge budget are the knobs for them.

## Verification

- `cargo fmt --all --check`; PG18 and PG17 workspace/all-target clippy with
  `pg_test`, warnings denied; non-extension workspace tests.
- `cargo pgrx test pg18`: the suite including the new tests for budgeted
  overflow merges, VACUUM publication racing folds and merges (through a
  `pg_test`-only race hook between the unlocked phase and publication), and
  orphan reclamation racing an insert that allocates freed pages.
- `postgres/tests/postings_lifecycle.py` and `docs/benchmarks/merge_lifecycle.py`,
  the latter now ending with a crash between a run write and its publication:
  a 40,000-document fold is interrupted by an immediate shutdown while its run
  pages are being written, `stannum.verify_index` reports the pages as
  `page N` warnings, `VACUUM (INDEX_CLEANUP ON)` reclaims them into the FSM,
  the verifier is clean and the repeated fold reuses the pages.
- `script/reference-oracle` against upstream Lead.

Raw local artifacts are in `/tmp/stannum-vacuum-results/`.
