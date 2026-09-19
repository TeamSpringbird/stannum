# Validated VACUUM merging, strategy controls, and workload coverage

This report records the integrated follow-up to PR #16's deletion-heavy
regressions. The earlier codec-only improvement did not resolve the database
workload: adding each document's unique term to the diagnostic reproduced the
remaining overhead. The implementation now combines count-only checked position
validation, ordered document membership, reused per-term scratch and decoded
cursors, dead-singleton avoidance, and source-aware live-document lookups. All
input validation remains enabled, including payloads belonging to dead documents.

The live lookup records its source owner so a dead occurrence cannot be confused
with a live reused CTID in another input. The map values are larger; these changes
do not establish a bounded-memory guarantee. No on-disk format, WAL/publication
protocol, or locking policy changed.

## Strategy selection

`stannum.experimental_vacuum_merge_strategy` accepts `auto`, `direct`, and
`reconstruct`. Selection is an explicit plan with a DEBUG1 explanation. Auto
continues to select direct merging; no adaptive density threshold is enabled.
Forced reconstruction shares complete input validation and checks duplicate live
CTIDs, output limits, and cancellation. The oversized aggregate compatibility
fallback is explicitly separate. Reconstruction still performs whole-source
record decoding and final encoding between checkpoints.

## Correctness and provenance

Local integration passed 463 core unit tests and four doc tests, 117 PostgreSQL
tests on each of 17 and 18, warnings-denied Clippy, release schema compatibility,
lifecycle/recovery/standby tests, and ranked concurrency fuzz. The integrated
Python harness suite passed 79 tests after correcting the ranked oracle.
Independent reviews found no correctness
issues in the decoder, bounds/cursor reuse, source-owner filtering, or workload
accounting. The reviews supplement the executed tests.

Runtime and initial protocol source: `12ed63f8144500ac3b48a951427042a5fecc764c`
(top of the three-PR stack). The ranked oracle correction described below does
not change the measured runtime or the six selective-query profiles.
Local machine: M4 Max ARM64, PostgreSQL 18.6, release Rust 1.96.0.

| Build | SHA-256 |
| --- | --- |
| Main before VACUUM integration, runtime at `80e5f85` | `cac665c501af1154a661a2f04c579deda38f12bac0e27f2abfeb2494c2df46ba` |
| Original PR #16 | `795f786bd28db5b2f878f1b6ce3f5d82d29403516905965ea93ec8d57da3a981` |
| Final integrated release | `8c7618c4ca8d9956b0eb00716f2f455c3110474c48f2167c4d96412e509dd400` |

Raw local results are retained under `benchmarks/results/vacuum-final/` (ignored
by Git); validation logs and the final release are under
`/tmp/stannum-next-validation-stage2`. The earlier, insufficient optimization is
retained separately under `benchmarks/results/vacuum-optimized/` with release
hash `458d3accae0c62930837258e6e869bda9ab6fc47904db7188825157f4a9de3cd`.

## Corrected comparison protocol

Every pair uses the same harness and dataset, with the private server stopped
before each library swap. A checkpoint completes before traffic. Three alternating
pairs compare each case; no overlapping CPU-heavy builds or tests ran locally.
This reruns the baseline under the corrected lifecycle rather than comparing
against earlier timings produced by the old driver. See
[vacuum workload coverage](vacuum-workload-coverage.md) for reader oracles,
scheduled-arrival accounting, and the difference between full-window and
conditional phase latency. Failed setup probes are retained separately and are
not performance samples.

## Local comparison with main

Each case starts with 32,768 documents. Repetition count 20 means short documents;
200 means long documents. Rewrite/mixed cases delete 75%. Values are medians of
three trials, baseline → candidate.

| Scenario | Repetitions | VACUUM ms | Sampled backend RSS MiB |
| --- | ---: | ---: | ---: |
| Live merge | 20 | 85.80 → 59.47 | 69.50 → 53.69 |
| Live merge | 200 | 182.03 → 131.72 | 187.36 → 124.05 |
| Deletion rewrite | 20 | 45.37 → 39.13 | 41.69 → 38.20 |
| Deletion rewrite | 200 | 87.31 → 76.74 | 72.45 → 57.44 |
| Mixed cleanup | 20 | 41.45 → 40.14 | 37.39 → 37.50 |
| Mixed cleanup | 200 | 84.88 → 82.41 | 55.55 → 64.81 |

All 36 trials passed correctness. Live merges improved by 28–31%, rewrites by
12–14%, and mixed cleanup by about 3% by median. The mixed gain is small and
should not be presented as a universal production guarantee. WAL deltas were
essentially unchanged. Sampled memory improved in four cases but increased in
long mixed cleanup; sparse RSS sampling includes shared mappings and is not a
true peak-allocation measurement.

## Ranked oracle correction

The first larger mixed-query campaign exposed an invalid benchmark assumption:
REPEATABLE READ freezes heap visibility, but it does not freeze the physical
index statistics used by BM25. A deterministic reproduction on both main and the
candidate kept the visible heap count at 65,523 while VACUUM changed the index's
document count from 131,072 to 65,523. Top-ten scores consequently changed from
8.506372 to 8.416039. Exhaustive and custom ranked scans agreed within each stable
phase, and both binaries produced identical diagnostic results.

The original comparison incorrectly required equal scores across that
publication. Its failed larger-workload trials are retained and excluded from
performance results. The same issue stopped the initial
[cross-platform campaign](https://github.com/TeamSpringbird/stannum/actions/runs/35423858239).
This is separate from the completed selective-query comparisons above. The
replacement oracle brackets the comparisons with complete immutable segment
directory fingerprints in separate statements. It reports transitions as
invalidations, requires stable comparisons of IDs and scores, and allows valid
cutoff ties. Its guard applies to this fixed-corpus, no-writer/no-DDL fixture;
it is not a general snapshot mechanism. See the
[coverage report](vacuum-workload-coverage.md) for accounting and limitations.

An ordinary two-window PostgreSQL smoke completed 184 stable ranked checks per
window. A controlled transition diagnostic completed 68 stable plus four
invalidated comparisons per window. Both require stable pre/post checks, exact
transaction accounting, and all membership/structural gates. The regular CI
matrix now exercises a small mixed-query direct/reconstruction workload.

## Larger fixed-arrival comparison

Using corrected protocol source `d656257`, six alternating trials compared main
and the final release on 131,072 short documents, 50% deletion, a skewed
997-term vocabulary, mixed queries, and 200 offered transactions/second over
12 seconds. All passed. Median VACUUM time was 247.36 → 206.91 ms; individual
ranges were 235.68–359.09 → 203.70–331.92 ms. The variability limits the precision
of that improvement.

Across the six windows, 3,524 timed ranked comparisons were stable and checked;
ten were explicitly invalidated by physical scoring-state transitions. Every
window also passed stable ranked checks before and after maintenance. These are
full-window counts, not a claim of ranked checks wholly inside VACUUM. Raw
results are under `benchmarks/results/vacuum-oracle-corrected/`. Oracle work,
fingerprint reads, and marker emission are included in reader latency.

Using the same final binary, three forced-direct/forced-reconstruction pairs on
32,768 short documents with 75% deletion completed successfully under the same
mixed-query offered load. Median VACUUM time was 34.67 ms for direct merging and
49.60 ms for fully validated reconstruction (ranges 33.82–37.64 and
49.21–56.08 ms). All 3,534 timed ranked comparisons were stable, with zero
invalidations and stable checks before/after each window. This supports keeping
Auto on direct merging for this workload; it does not calibrate an adaptive
crossover rule.

## Cross-platform comparison

The initial campaign completed all 144 selective-query trials across PostgreSQL
17/18 and ARM64/x86-64 before encountering the ranked-oracle issue in the larger
profile. It showed broad live-merge gains, but x86-64/PG17 long mixed cleanup
was 138.94 → 162.25 ms by median. All three candidate samples were slower than
all baseline samples. Dataset, output metadata and WAL matched, and no
checkpoint overlapped VACUUM. Reader latency and sampled RSS did not explain
the slowdown; it must not be dismissed merely as shared-runner noise.

The first corrected-oracle CI attempt stopped in the newly added smoke: its
1,024-document VACUUM finished before RSS sampling on x86 or a complete reader
transaction on ARM. No performance comparisons ran in that attempt. The smoke
now uses 32,768 documents and 200 offered transactions/second, preserving the
sampling and overlap gates instead of waiving them.

The [corrected-protocol rerun](https://github.com/TeamSpringbird/stannum/actions/runs/35425193502)
retains a fresh matrix plus larger mixed-query and forced-strategy comparisons.
These workflow artifacts contain the individual paired samples needed to judge
recurrence and variability. Neither campaign applies a hard timing threshold on
shared runners, and local gains do not imply gains on every platform/workload.

The rerun passed all 192 comparisons: 144 selective-query, 24 larger mixed-query,
and 24 forced-strategy trials. Median VACUUM milliseconds (three pairs/cell):

| Profile | ARM PG17 | ARM PG18 | x86 PG17 | x86 PG18 |
| --- | ---: | ---: | ---: | ---: |
| Short live merge | 141.98 → 83.84 | 157.40 → 90.86 | 236.94 → 112.38 | 171.25 → 118.41 |
| Short rewrite | 68.41 → 57.88 | 72.63 → 61.04 | 97.96 → 94.44 | 114.20 → 89.91 |
| Short mixed | 68.95 → 60.59 | 70.95 → 60.77 | 93.09 → 88.32 | 112.13 → 99.46 |
| Long live merge | 280.52 → 183.06 | 299.41 → 194.45 | 380.11 → 312.60 | 451.42 → 245.28 |
| Long rewrite | 123.84 → 124.40 | 130.90 → 128.48 | 217.20 → 160.32 | 216.92 → 194.99 |
| Long mixed | 127.84 → 124.80 | 134.96 → 126.45 | 225.05 → 174.49 | 227.94 → 156.88 |
| Larger mixed queries | 398.82 → 276.31 | 455.87 → 331.33 | 464.35 → 318.63 | 622.35 → 468.08 |
| Direct → validated reconstruction | 59.58 → 85.46 | 60.81 → 87.82 | 65.05 → 100.04 | 67.36 → 102.41 |

The earlier x86/PG17 long-mixed slowdown did not recur: every candidate in the
new three pairs was faster. However, its baseline median moved from 138.94 to
225.05 ms between campaigns, while the candidate moved from 162.25 to 174.49 ms.
The evidence supports a nonreproduced slowdown under substantial shared-runner
variation, not a proven explanation or a universal performance guarantee. Long
rewrite on ARM/PG17 was approximately unchanged (+0.5%).

The larger CI workload was overloaded: only 1,337–1,414 transactions completed
per window versus a nominal 2,400 offered, and p95 scheduling lag was 4.77–5.05
seconds. Un-emitted arrivals at shutdown are excluded from logged schedules.
These are oracle-inclusive pressure measurements, not steady-state throughput
or query-only latency evidence. The smaller forced-strategy profile completed
2,422 transactions in every window (seeded Poisson traffic), with p95 scheduling
lag around 6.6–7.8 ms. Fully validated reconstruction was 43–54% slower than
direct merging in those comparisons.

## Remaining evidence

The fixed-corpus workload measures one VACUUM alongside readers. It does not
establish sustained write/maintenance capacity or bounded peak memory. Those are
the next end-to-end gates before using load or resource budgets to select a
different strategy automatically. An integer-block/SIMD format remains a separate
prototype with compatibility and recovery gates; no format migration is included.
