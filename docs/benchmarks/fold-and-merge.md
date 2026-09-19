# Fold and merge optimization validation

Two independent changes were developed with separate file ownership: direct
forward-record ingestion in the segment codec, and optimistic fold construction
in PostgreSQL storage. The codec improvement is ready for review. Unlocked fold
construction passes correctness validation but remains draft because it regresses
writer throughput relative to the codec improvement in the small-buffer workload.

## Builds and protocol

The baseline is `a1dd092`; codec-only is `a69b3b5`; combined is `9ea2a27`.
Release libraries were retained and their SHA-256 values recorded in
`benchmarks/results/fold-and-merge/builds.json`. Raw artifacts and validation logs
are retained locally under that ignored directory; they are not published here.

The concurrent campaign used a private native ARM64 PostgreSQL 18.6 server,
512 MB shared buffers, JIT disabled, two readers and two writers, a 32-document
write buffer, 64 MiB byte cap, and merge budget 1,024. Each window ran for 20
seconds after creating an identical 512-document fixture. Three rounds rotated
baseline/codec/combined order. Long and short fixtures used harness repeat counts
200 and 20. There was no discarded warmup window. Readers queried the selective
`common AND w7` condition while writers inserted documents. Wait samples ran at
50 ms; independent snapshot membership oracles ran during traffic. Every window
also reconciled committed inserts with heap/index counts and deep verification.

These are closed-loop growing-table workloads: faster writers create larger
reader corpora. They establish workload-specific behavior, not fixed-corpus query
speedups, production capacity, or comparisons with TIN or PostgreSQL GIN.
Sampled BufferContent waits cannot identify the metadata lock specifically.

## Concurrent results

Each value is the median of three window-level values; p99 values are not pooled.

| Fixture | Build | Writer inserts/s | Writer p99 ms | Reader queries/s | Reader p99 ms |
| --- | --- | ---: | ---: | ---: | ---: |
| Long | Baseline | 5,118 | 7.166 | 1,817 | 8.665 |
| Long | Codec | 7,001 | 4.278 | 1,985 | 6.477 |
| Long | Combined | 5,104 | 7.519 | 1,930 | 8.750 |
| Short | Baseline | 8,479 | 3.677 | 1,466 | 6.200 |
| Short | Codec | 9,493 | 3.198 | 1,441 | 6.560 |
| Short | Combined | 8,865 | 3.523 | 1,615 | 6.114 |

The codec change improves median writer throughput by 36.8% for long documents
and 12.0% for short documents. It is not a universal reader improvement: short
reader throughput declines slightly and p99 increases. Adding unlocked folds
reduces long-document writer throughput by 27.1% versus codec alone, despite
passing every correctness check. Do not ship that change as a demonstrated
performance improvement.

## Larger-buffer controls

Eight additional 15-second windows compared codec and combined builds at the
512-document buffer cap, with long documents and two readers. Two rounds reversed
build order, separately for one and two writers. All windows passed correctness.
Other settings matched the contention campaign. These are exploratory two-window
medians and still have the growing-table limitation.

| Writers | Build | Writer inserts/s | Writer p99 ms | Reader queries/s | Reader p99 ms |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | Codec | 9,729 | 0.217 | 3,106 | 2.386 |
| 1 | Combined | 9,921 | 0.224 | 3,562 | 1.829 |
| 2 | Codec | 14,833 | 0.561 | 2,169 | 8.873 |
| 2 | Combined | 14,605 | 0.770 | 2,449 | 6.040 |

Larger buffers expose a reader benefit, rather than the severe small-buffer
writer regression. With two writers the combined build still trades slightly
lower writer throughput and worse writer tails for improved reader metrics.
One writer rules out competing-writer duplicate builds in that control but does
not isolate reader admission from lock-reacquisition overhead. These results do
not establish a safe cutoff or warrant enabling the change for every buffer size.

## Single-writer merge attribution

A separate fixed-input probe inserted 4,161 long documents per window with a
32-document buffer and optional merge budgets 0, 256 and 2,048. Each build/budget
has only one window, in fixed baseline/codec/combined order. All nine windows
passed correctness; maintenance classifications and directory-generation changes
matched for every insert across builds. Matched merge events had identical WAL
bytes. Final immutable segment counts were 128, 18 and 4 respectively; budget zero
still requires merges at the hard directory limit.

| Event | Baseline ms | Codec ms | Combined ms |
| --- | ---: | ---: | ---: |
| Budget 0 forced merge, median of 2 inserts | 0.945 | 0.570 | 0.544 |
| Budget 256 fold + merge, median of 16 inserts | 2.521 | 1.442 | 1.212 |
| Budget 2,048 large merge, mean of 2 inserts | 19.029 | 9.411 | 7.975 |
| Budget 2,048 execution sum, all 4,161 inserts | 255.671 | 189.096 | 165.758 |

These warm, single-writer measurements exclude commit/fsync and include index
inspection between inserts. The measured INSERT plans recorded only one shared-buffer read per window.
They support the codec optimization but cannot establish cold-cache performance,
separate CPU from WAL/write costs, or justify concurrent unlocked folds. Repeat
and rotate these trials before treating the merge-event percentages as stable.
See the [probe description](merge-costs.md) for reproduction commands and caveats.

## Correctness validation

Local checks passed 441 core unit tests and four documentation tests, 60 Python
harness tests, and 111 extension tests on each of PostgreSQL 17.11 and 18.6.
Formatting and warnings-denied Clippy passed on both PostgreSQL majors. Lifecycle
validation included 340 correct standby answers, zero wrong answers, and one
expected recovery conflict. Ranked concurrency fuzzing passed 782 comparisons.
Both source PRs passed CI on x86-64 and ARM64 with PostgreSQL 17 and 18, including
the upstream Lead reference oracle.

Five deterministic fold races cover same-epoch appends, competing folds, VACUUM
buffer rewrites, dead-list changes with an unchanged buffer, and repeated
invalidation reaching the locked fallback. Codec tests compare complete output
bytes across all three formats and preserve malformed-input error atomicity.

## Remaining work

1. Preserve the independent codec improvement. Keep unlocked folds in draft until
   a revised protocol demonstrates a worthwhile concurrency benefit.
2. Measure speculative build attempts/discards, construction time and exclusive
   lock reacquisition time. Readers decode unseen buffered records under a shared
   metadata guard; releasing a full buffer may admit expensive reader work before
   publication. Multiple writers may also build the same buffer. Current samples
   cannot distinguish these costs.
3. Attribute remaining merge stalls to input decoding, encoding and WAL-backed
   publication before choosing the next optimization. The merge-budget probe
   measures complete INSERT execution, not individual phases.
4. Design explicit unpublished-run ownership and reclamation before moving run
   writes or merges outside the metadata lock. The current orphan collector relies
   on that lock; simply releasing it around I/O is unsafe.
