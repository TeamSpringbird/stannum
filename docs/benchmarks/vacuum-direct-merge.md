# Direct posting merges in VACUUM

VACUUM now uses the validated direct-merge API for deferred merges and deletion
rewrites. It preserves LSG3, source selection, WAL, reclamation, and publication
ordering. This completes integration into the existing maintenance paths without
changing the on-disk format.

## Publication and cancellation

Source bytes and dead sets are owned throughout unlocked construction. A decoder
failure is corruption only while the index identity and every complete captured
entry remain current. Retired inputs discard the attempt and retry. Publication
rechecks those same entries under the existing metadata lock; stale output is
freed. All-dead inputs publish no successor. Aggregate sizes beyond the API's
format limits retain reconstruction fallback; these limits are not a memory cap.

VACUUM checkpoints check PostgreSQL interrupts without a held content lock.
Cancellation can therefore stop construction before output allocation. A single
codec/validation operation remains non-interruptible. Foreground merges retain
their existing interrupt deferral while locked.

Four new PostgreSQL tests cover cancellation before writing and successful retry,
source retirement during construction, current versus stale decoder failures,
and all-dead removal followed by insertion. The existing publication-race test
covers retirement after output allocation.

## Local validation

M4 Max ARM64, PostgreSQL 18.6 for timings; correctness also checked on PostgreSQL
17. Both majors passed 113 extension tests and Clippy with warnings denied.
The pure crates passed 454 unit tests and four doc tests. All 63 Python harness
tests passed. Release lifecycle/recovery checks and the ranked concurrency smoke
suite (five runs, 818 comparisons) passed.

Baseline: main `80e5f85e05b178c58e24a73d30a794a9004c4a18`, using the retained
release artifact from its identical production source before PR #15's docs merge.
Baseline SHA-256: `cac665c501af1154a661a2f04c579deda38f12bac0e27f2abfeb2494c2df46ba`.
Candidate SHA-256: `795f786bd28db5b2f878f1b6ce3f5d82d29403516905965ea93ec8d57da3a981`.
Raw local artifacts: `benchmarks/results/vacuum-direct/{merge,rewrite,mixed}`
(ignored by Git); validation logs: `/tmp/stannum-vacuum-direct-validation`.
A failed setup with an out-of-range merge-tier setting is retained separately
under `failed-setup`; it produced no performance result.

## Paired end-to-end protocol

The original `benchmarks/paired_libraries.py --workload vacuum` campaign replaced
retained release binaries between windows, with fresh backend connections while
the private postmaster remained running, and restored the original on exit.
An earlier version of this report incorrectly said the server stopped before each
swap. The expanded coverage driver adds explicit stop/swap/start sequencing;
new measurements must rerun both baseline and candidate under that protocol.
Each original scenario ran three alternating pairs against newly created databases.
There are 32,768 documents, each with 200 repetitions of two common terms, a
97-way selective term, and a unique hash. No logical writes occur during timing.

- `merge`: eight live segments become one.
- `rewrite`: one segment with 75% deleted documents becomes one live successor.
- `mixed`: eight segments with 75% deleted documents undergo cleanup to one.

Two pgbench readers run for four seconds. Every transaction checks the search
count against a heap-derived arithmetic oracle. Final checks compare full ID
multisets, assert segment/document/dead counts, and require a clean index verifier.
VACUUM must finish while readers are running. The harness retains full-window
reader logs and the subset of transactions entirely contained in the VACUUM
window. The latter excludes transactions crossing either boundary; full-window
logs retain those transactions. Backend RSS is sampled every 20 ms and includes
shared mappings: it is not allocation accounting or a reliable true peak.
WAL deltas cover the whole VACUUM. A checkpoint precedes traffic; the paired
driver disables automatic checkpoints during these short trials.

## Local results

Medians of three trials, baseline → candidate:

| Scenario | VACUUM ms | Sampled backend RSS MiB | WAL bytes | Reader p95 during VACUUM, ms |
| --- | ---: | ---: | ---: | ---: |
| Merge | 178.44 → 123.71 | 190.20 → 94.38 | 19,955,696 → 19,955,696 | 0.098 → 0.095 |
| Rewrite | 88.78 → 88.57 | 69.48 → 58.64 | 5,442,464 → 5,442,440 | 0.101 → 0.101 |
| Mixed | 85.86 → 93.12 | 76.09 → 58.20 | 5,454,800 → 5,454,800 | 0.117 → 0.125 |

Deferred merges were 31% faster by median; rewrites were effectively unchanged.
Mixed cleanup was 8.5% slower, with all three candidate times above their paired
baseline. This is a performance tradeoff requiring cross-architecture evidence
before promotion, not a universal speedup. Sampled memory was lower in each
scenario, but only 3–7 RSS samples were captured per VACUUM.

Each phase had 1,620–4,126 checked reader transactions. Phase p99 ranged from
0.105–0.172 ms for baseline and 0.109–0.241 ms for candidate; three short trials
do not establish a tail-latency improvement. All 18 trials passed correctness.

The optional CI `vacuum_performance` dispatch repeats all three scenarios on
PostgreSQL 17/18 and x86-64/ARM64 against the pinned main baseline, retaining raw
artifacts. No timing threshold is asserted on shared runners. These synthetic
measurements do not compare Stannum with Tin or establish production behavior.

## Deletion-density diagnosis

A diagnostic copy of the end-to-end harness changed only the mixed scenario's
deletion predicate and expected remaining count from 75% dead to 50% dead.
Three pairs measured 112.81 → 114.26 ms (median, about 1.3% slower), compared
with 8.5% slower at 75% dead. All checks passed. Artifacts are retained under
`benchmarks/results/vacuum-direct/diagnostic-half-dead`; the diagnostic harness
is under `/tmp/stannum-vacuum-density-probe` and its hash is in each manifest.

A separate throwaway codec probe adapted `segment/examples/merge_memory.rs` to
filter 0%, 50%, or 75% of each source's documents and time the verifier separately.
Eight sources each contain 4,096 documents with 400 positions over two terms.
Five alternating trials per deletion density produced identical complete output
bytes for reconstruction and direct merging. Median times (ms):

| Dead documents | Reconstruction | Validated direct merge | Verifier alone |
| --- | ---: | ---: | ---: |
| 0% | 93.14 | 55.60 | 25.10 |
| 50% | 59.23 | 53.22 | 23.02 |
| 75% | 47.99 | 47.76 | 22.96 |

This supports the hypothesis that validating all input, including deleted
postings, creates a fixed cost while reconstruction gets cheaper as fewer
records survive. It does not isolate all costs in the PostgreSQL mixed scenario:
the minimized fixture lacks its unique terms, heap/WAL work, and concurrent
readers. Raw diagnostic results are `/tmp/vacuum-codec-diagnostic.json`; the
throwaway source is retained outside the repository in the diagnostic directory.
Validation must remain intact; bypassing checks on dead postings would weaken
the API's corruption contract.

## Short-document follow-up

Repeating the same three scenarios with 20 repetitions rather than 200 yielded
these medians of three pairs. All 18 additional trials passed correctness.

| Scenario | VACUUM ms, baseline → candidate | Sampled backend RSS MiB |
| --- | ---: | ---: |
| Merge | 88.75 → 65.27 | 66.28 → 50.08 |
| Rewrite | 48.01 → 52.84 | 47.80 → 40.47 |
| Mixed | 43.84 → 50.40 | 38.36 → 39.52 |

Live merges still improved by 26%; deletion rewrites slowed by 10% and mixed
cleanup by 15%. The mixed workload had no sampled-memory benefit. Results are
under `benchmarks/results/vacuum-direct/short-{merge,rewrite,mixed}`. These findings
keep promotion blocked pending improvement of the deletion-heavy path while
preserving full validation. The first cross-platform campaign uses 200 repetitions;
it must not be described as validating the short-document performance results.

## Standby test synchronization

One initial ARM/PG17 CI lifecycle run reported orphan-page warnings while checking
the standby immediately after its reader was cancelled for a recovery conflict.
Reader completion does not imply replay has reached the primary's final cleanup
WAL. The lifecycle test now waits for that WAL position before requiring a clean
structural verifier result, preserving the strict empty-findings assertion. This
changes test synchronization, not storage behavior or query-result checks.
