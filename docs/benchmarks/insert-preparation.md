# Preparing inserts outside the exclusive metadata lock

Implementation: `03710ab`, following the
[concurrent contention harness](concurrent-write-contention.md).

## Change and invariant

An insert used to hold the metadata page exclusively while tokenizing its text,
constructing a forward record, and encoding it. It now captures the index
identity and persisted tokenizer settings under a short shared lock, prepares
the encoded record without a page lock, and drops temporary token/record
allocations before acquiring the exclusive lock. It then validates the captured
identity/settings and appends using freshly read metadata.

Concurrent appends, folds and VACUUM may change the buffer during preparation;
that does not invalidate a single prepared record. Requiring an unchanged buffer
version would retry unnecessarily and could starve under load. Conversely, using
the original captured metadata for publication could lose intervening writes.
The implementation does neither. A change in identity/settings retries encoding.

The extra shared metadata read is an overhead, especially for short documents.
This is why both short and long document fixtures are measured. Folding, merging,
run writes, publication, WAL and reclamation remain inside their existing lock
boundaries; this change does not eliminate maintenance stalls.

## Validation

- A deterministic hook performs indexed reads and inserts that force folds
  between preparation and publication. Exact case/phrase matches, document
  totals and deep verification prove the intervening records survive. Altered
  reloptions also prove the persisted tokenizer remains authoritative.
- The regression failed on the old implementation (the preparation hook never
  fired), then passed with the new path. Final suites passed 106 extension tests
  on each of PostgreSQL 17.11 and 18.6.
- Formatting, warnings-denied Clippy on both majors, 438 core unit tests and four
  doc tests passed; three optional probes remain ignored. All 55 Python harness
  tests passed.
- Lifecycle/recovery validation passed, including 340 correct standby answers,
  zero wrong answers and one expected recovery conflict. Five concurrent ranked
  smoke scenarios passed 786 comparisons covering 68,007 rows.

All six [implementation CI jobs](https://github.com/TeamSpringbird/stannum/actions/runs/35406198296)
passed at `03710ab`, including PostgreSQL 17/18 on ARM64 and x86-64 and the
upstream Lead oracle. The final report commit changes documentation only.

## Measurement protocol

Baseline release binary `4faff8f` has the same production Rust/Cargo/SQL files as
main `1f79095`; intervening merges added documentation and the write probe. The
changed release binary contains `03710ab`. Libraries are swapped atomically under
the shared installation lock and loaded by fresh sessions in fresh databases.
A dedicated PostgreSQL 18.6 server runs on native ARM64 macOS with 512 MiB shared
buffers; no container resource limits are applied.

Each fixture has three baseline/changed pairs in AB, BA, AB order. Each window
runs 20 seconds with two writers and two readers, 512 initial documents, a
32-document/64-MiB buffer cap, merge budget 1,024, and JIT disabled. The long
fixture repeats `common filler` 200 times per document; the short fixture uses
20 repetitions. Sampling is requested every 50 ms and correctness checks every
second. Tables disable autovacuum. There is no discarded warmup phase.

These are short closed-loop growth tests, not steady-state fixed-corpus tests.
Faster writers change the number of rows readers encounter. Wait sampling and
oracle execution add overhead; directory sampling can itself block on metadata.
Timing differences therefore do not directly measure metadata lock-hold time.

## Paired results

All twelve windows passed concurrent and final correctness checks, with zero
reported transaction failures. Together they completed 228 correctness checks
fully contained within concurrent traffic. Final heap/index totals matched
logged writer completions in every run. Saved plans use the Stannum index for
the measured reader and both indexed oracle branches.

Values below are medians of three per-run statistics (not a pooled percentile).
Percentage changes compare these medians.

| Fixture / role | Baseline tx/s | Changed tx/s | Change | Baseline p95 / p99 ms | Changed p95 / p99 ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Long reader | 1,269 | 1,618 | +27.5% | 7.700 / 9.739 | 7.093 / 9.071 |
| Long writer | 4,815 | 4,846 | +0.6% | 1.917 / 7.604 | 1.870 / 7.363 |
| Short reader | 1,365 | 1,463 | +7.2% | 5.372 / 7.377 | 5.142 / 6.766 |
| Short writer | 8,159 | 8,163 | +0.0% | 1.383 / 3.773 | 1.405 / 3.645 |

Per-pair throughput and corpus growth, baseline → changed:

| Fixture / pair | Reader tx/s | Writer tx/s | Final rows |
| --- | ---: | ---: | ---: |
| Long 1 | 978 → 1,517 | 4,923 → 4,846 | 99,058 → 97,578 |
| Long 2 | 1,269 → 1,618 | 4,802 → 4,824 | 96,683 → 97,122 |
| Long 3 | 1,360 → 1,833 | 4,815 → 5,014 | 96,908 → 100,930 |
| Short 1 | 1,190 → 1,293 | 8,540 → 8,178 | 171,537 → 164,354 |
| Short 2 | 1,399 → 1,479 | 8,093 → 8,052 | 162,551 → 161,730 |
| Short 3 | 1,365 → 1,463 | 8,159 → 8,163 | 163,906 → 164,002 |

Reader throughput improved in every pair. Writer throughput was effectively
neutral by median; short-document paired changes were −4.24%, −0.51%, and
+0.04%, with a small median p95 increase. This leaves a possible small
short-document writer tradeoff. These runs support better reader progress
under this workload, not a universal throughput improvement.

Substantial sampled `BufferContent` waiting remains. For the long fixture,
reader BufferContent observations were about 69% and 66% of active reader
observations before/after, while writer observations were about 36% in both.
These are descriptive sample fractions, not time percentages or lock-duration
measurements. The remaining locked fold/merge work is still a major target.

## Artifacts and next step

Raw artifacts and `comparison.json` are retained locally in the ignored
`benchmarks/results/concurrent-write-contention/`. Validation logs and campaign
drivers are in its `validation/` directory. The initial failed baseline probe
is retained: pgrx had left test-only SQL functions installed while the release
library was restored. A complete release reinstall fixed the measurement setup
before all twelve reported windows. That attempt produced no valid timing result.

Release-library SHA-256:

- Baseline: `7f134858587029c2eb57bb3a0aca9ceec9fea57bf09613b839c2534b677b2ee5`
- Changed: `e8be4248de814a44791955e38055b940e99feaa6ef57c5abda6e93f4c2a2bbf3`

The next candidate is CPU-only fold construction from copied buffer bytes
outside the lock, followed by full buffer-state validation before locked output
writes/publication. It needs bounded retries and a locked fallback to avoid
starvation. Fully unlocked output writes need an explicit reservation mechanism:
orphan reclamation currently relies on the metadata lock to exclude unpublished
foreground runs. Epoch-only validation is insufficient because an append does
not change the epoch.
