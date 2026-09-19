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

`benchmarks/paired_libraries.py --workload vacuum` swaps retained release binaries
only while its private server is stopped and restores the original on exit.
Each scenario runs three alternating pairs against newly created databases.
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
