# Unlocked-fold regression diagnosis

PR #10's historical 27% writer-throughput regression did **not** reproduce in
three fresh, original-protocol comparisons on 2026-09-18. The retained release
binaries were unchanged. This is not a fix: the historical discrepancy remains
unexplained, and no production synchronization code was changed during this
investigation.

The investigation follows the [original fold measurements](https://github.com/TeamSpringbird/stannum/blob/1100a04/docs/benchmarks/fold-and-merge.md).
The codec-only binary is the production behavior merged by PR #9; the combined
binary adds PR #10's optimistic unlocked construction. Neither comparison
includes later direct-merge API work.

## Reproduction and controls

All runs used a private PostgreSQL 18.6 cluster on the same native ARM64 macOS
machine, 512 MB shared buffers, JIT disabled, 512 initial documents, 200 body
repetitions (long documents), a 32-document/64 MiB write buffer, merge budget
1,024, 50 ms wait sampling and 1,000 ms correctness checks. Standard traffic
used two readers searching `common AND w7` and two inserting writers.
Installation and timing were serialized with `/tmp/stannum-pgrx-lock.py`;
other agents did not compile or run CPU-heavy tests during measurements.

These remain growing-table, closed-loop workloads: faster writers create more
rows and make later searches more expensive. They are not fixed-corpus query
comparisons or throughput at a fixed offered load. Samples of `BufferContent`
do not identify which index page caused a wait.

Retained binary SHA-256 hashes:

| Build | Source | SHA-256 |
| --- | --- | --- |
| Codec | `a69b3b5` | `b29075c4330279a9bb917782083c517f3bff64f1372b31ee457aea7d269c73df` |
| Combined | `9ea2a27` | `57f12a06a4b70da6b51814458246ce8cd4b1bb6617b505a43ca34e9817a1b347` |

The first diagnostic controls shortened windows to 12 seconds and separately
varied writer count and index-reading traffic. The idle-reader control replaces
the reader query with `SELECT pg_sleep(0.001)` and intentionally skips the index
plan assertion; background correctness checks still query the real index.
It is a diagnostic control, not a search-performance measurement.

| 12-second control | Codec writer QPS | Combined writer QPS | Difference |
| --- | ---: | ---: | ---: |
| Search, one writer | 7,221 | 6,849 | -5.2% |
| Search, two writers | 8,668 | 8,544 | -1.4% |
| Idle readers, one writer | 7,014 | 7,159 | +2.1% |
| Idle readers, two writers | 8,214 | 8,892 | +8.3% |

Each control is one pair and cannot establish a stable performance difference.
None reproduced the historical 27% loss. The short duration warranted repeating
the original 20-second protocol before selecting a remedy.

## Original-protocol repeats

Three 20-second pairs used codec/combined, combined/codec, codec/combined order.
All completed with exact-match, heap/index accounting and deep verification
checks passing. Rates and p99 values below summarize each window independently.

| Pair | Codec writer QPS | Combined writer QPS | Difference |
| --- | ---: | ---: | ---: |
| 1 | 7,373 | 7,075 | -4.0% |
| 2 | 7,014 | 7,071 | +0.8% |
| 3 | 7,022 | 6,935 | -1.2% |
| Median of windows | 7,022 | 7,071 | +0.7% |

Median reader throughput was 1,694 versus 2,197 queries/second (+29.7%). Median
window-level reader p99 was 6.393 versus 6.004 ms; writer p99 was 4.299 versus
4.249 ms. The positive difference between separate medians should not obscure
the small negative differences in two of the three paired runs.

## What instrumentation establishes

Three ranked hypotheses guided the probes:

1. If competing writers duplicate fold construction, one writer should eliminate
   discarded builds and reduce any associated loss.
2. If readers admitted during construction delay publication, idle readers
   should reduce the loss caused by index reads.
3. If unlock/relock overhead alone dominates, loss should persist with a single
   writer and idle readers.

A temporary patch accumulated attempted/stale builds, construction elapsed
time and exclusive metadata reacquisition elapsed time per backend. It emitted
one cumulative server-log record per 128 attempts; it did not log every fold.
The patch was removed immediately after building the diagnostic library.
The diagnostic library was used only in two separately labeled 20-second runs.

| Counter | One writer | Two writers |
| --- | ---: | ---: |
| Attempts represented by final log snapshots | 3,840 | 8,448 |
| Stale attempts | 0 | 4,149 (49.1%) |
| Mean construction time | 133 µs | 134 µs |
| Mean metadata reacquisition time | 2 µs | 1,161 µs |

These are truncated cumulative snapshots: each backend's final zero to 127
attempts are omitted. No claim of cycle-accurate accounting is intended. The
stale counter compares the complete captured buffer state after reacquisition;
in this insert-only workload, it identifies builds invalidated by concurrent
writers. It does not distinguish every possible invalidation cause in general.

Competing writers really do duplicate work. Multiplying the stale count by the
mean construction time suggests approximately 0.56 seconds of wasted construction elapsed time
summed across both writers in this 20-second run. Stale builds were not timed
separately and elapsed time includes scheduling, so this is neither a direct
measurement of discarded-build time nor a CPU-time measurement. The large reacquisition interval
also includes waiting for another writer's existing locked publication and
foreground merges. It is not a measurement of newly introduced overhead, and it
does not distinguish reader-held from writer-held metadata locks. There is no
corresponding codec-only lock-wait counter. These facts do not establish that
duplicate work caused the historical 27% throughput loss.

## Checkpoint control

Server logs revealed WAL-triggered checkpoints approximately 11 seconds apart
under the original default `max_wal_size`. The original campaign reused a
cluster across windows, so checkpoint phase was uncontrolled. This is a
benchmark confound, not evidence that checkpoints caused the historical result.

A separate follow-up protocol sets `checkpoint_timeout = '30min'` and
`max_wal_size = '16GB'`, then completes an explicit `CHECKPOINT` before every
20-second window. Its results are reported separately from the original
protocol.

| Checkpoint-controlled pair | Codec writer QPS | Combined writer QPS | Difference |
| --- | ---: | ---: | ---: |
| 1 | 7,352 | 7,033 | -4.3% |
| 2 | 6,954 | 6,911 | -0.6% |
| 3 | 7,045 | 7,084 | +0.6% |
| Median of windows | 7,045 | 7,033 | -0.2% |

Median reader throughput was 1,975 versus 2,249 queries/second (+13.9%). Median
reader p99 was 6.691 versus 6.158 ms; writer p99 was 4.637 versus 4.628 ms.
All correctness checks passed. Server logs contained no checkpoint start or
completion within any traffic interval; explicit checkpoints and database-drop
checkpoints occurred between windows. There were no WAL- or timeout-triggered
checkpoints in this campaign.

Historical server-log analysis also works against a simple checkpoint
explanation: checkpoint work overlapped the original codec windows for
10.209/9.659/9.143 seconds, but the slower combined windows for only
5.422/4.951/4.600 seconds. Overlap is not a measure of its performance impact,
and these observations do not establish a cause for the historical slowdown.

## Decision and next experiment

No production locking change is justified as a demonstrated regression fix by
these measurements. Keep the historical result visible alongside the failed
reproduction; do not replace it with a claim that PR #10 was fixed.

If coordination is pursued as an independent optimization, a per-index
transaction-owned heavyweight page lock is a candidate. Full-buffer writers
would release metadata before acquiring the coordination lock, reacquire and
revalidate metadata, and retain coordination across construction/publication.
The proposed order is coordination → metadata → data pages → relation extension.
Readers and VACUUM would retain their existing behavior. Full buffer-state
validation and bounded fallback remain necessary for VACUUM and writers using
different caps. This is a design candidate, not implemented or proven faster.
PostgreSQL’s [page-lock implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/storage/lmgr/lmgr.c)
uses transaction-owned heavyweight locks. The candidate needs real two-backend cancellation/error-cleanup tests and comparisons
against both current implementations before adoption.

Production disk allocation/writes remain under their existing locks; changing
that separately would require preserving orphan-reclamation exclusion.

## Artifacts and replay

Ignored artifacts live under `benchmarks/results/fold-diagnosis/`: per-window
SQL, query plans, binary hashes, raw pgbench logs, results and correctness
checks. `validation/` contains exact campaign drivers, the explicitly modified
idle-reader harness, the instrumentation patch and parsed counter snapshots.
The drivers retain their machine-local binary and installation paths; adapt
those paths before replaying on another machine.

The checked-in `benchmarks/fold_reproduction.py` replays three alternating pairs
in a private cluster and restores the installed library afterward. Its inputs
must be retained copies of already-built release libraries matching the
installed SQL. Put PostgreSQL's `initdb`, `pg_ctl`, `psql` and `pgbench` on `PATH`:

```sh
python3 /tmp/stannum-pgrx-lock.py python3 benchmarks/fold_reproduction.py \
  --codec /path/to/codec.dylib --combined /path/to/combined.dylib \
  --installed-library /path/to/installed/stannum.dylib \
  --output benchmarks/results/fold-reproduction --checkpoint-control
```

Omit `--checkpoint-control` to replay the original checkpoint protocol. The lock
wrapper is machine-local; use the equivalent installation exclusion elsewhere.
The generalized driver passed a one-pair three-second execution smoke check;
two mocked failure tests also ensure startup timeout stops a surviving postmaster
before cleanup, and failed shutdown preserves its data directory. The full Python
harness suite passed 62 tests.
those smoke timings are not included above. The reported campaigns used the
archived original drivers with the same workload and cluster settings.

For each installed release build, the standard workload command was:

```sh
python3 benchmarks/contention.py \
  --seconds 20 --writers 2 --readers 2 \
  --write-buffer-docs 32 --repeat 200 --sample-ms 50 \
  --max-merge-docs 1024 \
  --artifact /path/to/installed/stannum.dylib \
  --output benchmarks/results/fold-diagnosis/unique-window-name
```

Point libpq environment variables at the isolated cluster. Swap libraries only
between runs with fresh backend connections and the installation lock held.
Archive the server log as well as client results so checkpoints and other
background work can be checked against traffic intervals.
