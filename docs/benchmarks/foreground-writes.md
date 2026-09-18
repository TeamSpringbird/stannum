# Foreground write-stall attribution

The next layer above streaming search adds `benchmarks/foreground_writes.py`.
It establishes an observable baseline before changing foreground fold/merge
locking. This layer changes no production storage code.

## Probe

The probe creates a uniquely named disposable database, disables autovacuum on
its fixture, and issues one INSERT at a time through one psql session. Each
INSERT uses `EXPLAIN (ANALYZE, BUFFERS, WAL, TIMING OFF, FORMAT JSON)`; directory
snapshots taken between statements classify the sample by immutable generation
changes:

- **Buffered:** no immutable generations change.
- **Fold:** new generations appear without retiring existing ones.
- **Fold and merge:** pre-existing generations are retired and replaced.

Generation identity matters: a fold followed by a merge can leave the segment
count unchanged. The probe records pre-existing retired document counts, not
an invented total work count: intermediate runs in cascading merges do not
appear in the before/after snapshots.

The artifact directory contains the exact SQL, raw JSON stream, per-insert
samples, installed-library SHA-256, server settings and summaries. Incomplete
output, wrong membership, wrong document totals, verification errors, or a
changed library hash fail the run. p99 is omitted for groups with fewer than
100 samples. The disposable database is dropped on success and failure.

```sh
# Use a dedicated local server and hold the extension-installation lock if shared.
python3 benchmarks/foreground_writes.py \
  --docs 4161 --repeat 20 --write-buffer-docs 32 --max-merge-docs 1024 \
  --artifact /path/to/installed/stannum.so \
  --output benchmarks/results/foreground-writes/baseline
```

The output directory must be new. Connection settings come from libpq; ambient
PGOPTIONS is cleared. This probe explicitly sets the byte cap to 64 MiB,
merge tier factor to eight and segment cap to 128, so the small fixture reaches
known document-count boundaries. These are diagnostic settings, not a proposed
production configuration.

Execution time excludes commit/fsync and client round trips. Between-statement
inspection warms index buffers and slows the offered workload. There is one
writer and no concurrent readers. Therefore these results identify maintenance
cost, not production throughput, lock-wait latency, or end-to-end write p99.

## Baseline and controlled probes

Measured on the streaming-search production build `4faff8f`, native ARM64
macOS, PostgreSQL 18.6, shared buffers 512 MiB. Library SHA-256:
`7f134858587029c2eb57bb3a0aca9ceec9fea57bf09613b839c2534b677b2ee5`.
Each run inserts 4,161 documents with a 32-document buffer cap. The baseline
repeats `common filler` 20 times per document and allows 1,024 input documents
of optional merge work per fold.

| Run | Buffered p50 ms | Fold p50 ms | Fold + merge p50 ms | Merge-triggering inserts |
| --- | ---: | ---: | ---: | ---: |
| Baseline | 0.009 | 0.116 | 0.798 | 16 |
| Baseline repeated | 0.009 | 0.117 | 0.823 | 16 |
| Optional merge budget zero | 0.010 | 0.124 | 0.389 | 2 |
| Document repetition 200 | 0.027 | 0.353 | 2.686 | 16 |

All four runs passed the exact fixture-membership comparison, heap/index
counts, and deep index verifier. Five new harness tests cover attribution,
truncated output, correctness rejection, and rare-spike summaries; all 49
Python harness tests passed.

The two zero-budget merges occur at inserts 4,129 and 4,161: these exceed the
128-entry hard directory bound. Setting the optional budget to zero does not
eliminate forced foreground merging. It leaves more segments for searches and
VACUUM, so these measurements are not a recommendation to change the default.
The repeated baseline confirms the fold/merge cost separation. Longer documents
raise ordinary insert cost as well as maintenance cost; that does not by itself
prove tokenization is the sole cause. Buffered outliers also occur (one reached
3.853 ms), so not every slow insert can be attributed to a merge.

Raw artifacts are retained locally in the ignored
`benchmarks/results/foreground-writes/{baseline,repeat,deferred,long-documents}/`.

## Next implementation boundary

Foreground `fold` and `merge` still build and write segments while holding the
metadata page exclusively. The next experiment should measure concurrent reader
and writer waiting at these observed boundaries, then move construction outside
that lock using a captured buffer epoch and publication validation. It must
preserve appended records, snapshot readers, WAL-before-publication ordering,
and orphan reclamation on cancellation/crash. VACUUM's existing optimistic
publication machinery is a useful reference, but a mutable buffer needs its own
append/epoch validation. Simply releasing the lock around the existing `fold`
would be incorrect.
