# Deletion-heavy merge validation cost

This diagnostic isolates codec work behind PR #16's deletion-heavy VACUUM
regression. It does not measure PostgreSQL, WAL, heap access or concurrent readers.
Base: `7f40c2e`; the shared-validation extraction is `536ffc2`.

## Reproduction

Build `segment/examples/vacuum_diagnostic.rs` in release mode on both revisions.
Prepare eight sources once, outside the timed process:

```sh
cargo build -p segment --release --example vacuum_diagnostic
target/release/examples/vacuum_diagnostic prepare /tmp/vacuum-fixture 4096 400 2
DIAG_DELETION=3 target/release/examples/vacuum_diagnostic direct /tmp/vacuum-fixture /tmp/direct.segment
DIAG_DELETION=3 target/release/examples/vacuum_diagnostic reference /tmp/vacuum-fixture /tmp/reference.segment
cmp /tmp/direct.segment /tmp/reference.segment
```

`DIAG_DELETION` selects quarters removed from each source (0 through 4).
`validate` measures the verifier alone. Run five separate processes for each mode
and density, alternating baseline/candidate order. Timings include input file
reads; direct timing also includes constructing its dead sets. Thus comparisons
between direct and reconstruction include this additional direct setup cost.

The local run used an M4 Max, macOS, Rust 1.96.0, release LTO, and the shared
machine-wide benchmark lock. The fixture has 32,768 interleaved documents,
400 positions per document and two common terms. It lacks the unique terms and
short documents of the database benchmark. Five trials per cell are diagnostic,
not a confidence interval or a universal strategy-selection threshold.

## Hypotheses and changes

1. Binary membership searches for every posting repeat work on dense terms.
   Galloping from the previous document ordinal preserves sparse logarithmic
   searching while making adjacent matches constant work. Alone this reduced
   verifier time by approximately 0–5% in the first paired probe.
2. Enqueueing dead postings makes the cross-input heap order values that will be
   discarded. Advance them before enqueueing. They are still fully validated,
   their payloads are advanced, and each skipped posting has a cancellation check.
   With the membership change, the second probe reduced 75%-dead direct merging
   from 46.35 to 41.81 ms.
3. Validation decodes every position into a vector only to inspect its count.
   Reuse the exact checked position decoder with a no-op visitor. This validates
   all varints and cumulative overflow, including dead documents, without writing
   decoded positions to memory. This was the largest measured improvement.

No validation checks, format limits or publication checks were removed. The
verifier retains its report and malformed-input behavior. The shared validation
function admits alternative executors without duplicating that contract.

## Final isolated results

Medians in milliseconds, baseline → candidate:

| Dead documents | Full verifier | Validated direct merge | Reconstruction control |
| --- | ---: | ---: | ---: |
| 0% | 24.98 → 13.64 | 56.91 → 47.00 | 92.94 → 95.15 |
| 50% | 23.09 → 12.66 | 53.15 → 43.87 | 60.89 → 59.97 |
| 75% | 23.05 → 13.57 | 49.77 → 34.66 | 48.14 → 47.92 |
| 100% | 23.30 → 13.10 | 37.95 → 30.23 | 32.77 → 33.72 |

All direct/reconstruction outputs matched complete SHA-256 digests within each
density across revisions and trials, including the 15-byte empty segment. Raw
local records are `/tmp/vacuum-compare.json`; the two preceding probes are
`/tmp/vacuum-compare-gallop.json` and `/tmp/vacuum-compare-deadheap.json`.
These temporary files are provenance for this run, not portable release artifacts.

## Correctness and remaining gate

The segment suite passed 86 tests, including generated mixed-format byte
comparisons, every-byte mutation verification, unknown/dead/duplicate TIDs,
resource limits, and malformed payloads. New count/materialization differential
coverage traverses LSG1/2/3, seeks over skip boundaries, every truncation and
single-byte mutations, comparing errors and cursor ordinals. Cancellation is
exercised at every checkpoint for live, partly dead and entirely dead inputs.

End-to-end PostgreSQL tests and alternating VACUUM/reader benchmarks must still
validate the production improvement, particularly short documents, unique terms,
ARM/x86, and PG17/18. These isolated results do not establish that every
PR #16 database regression is fixed and must not replace that merge gate.

## Unique-term follow-up

The first PostgreSQL rerun did **not** eliminate the deletion-heavy regression.
The two-term fixture was insufficient: SQL adds one unique term per document and
uses contiguous source ranges. The diagnostic now supports `DIAG_LAYOUT=sql`,
which builds `wN`, alternating common/filler positions, and a unique deterministic
32-hex-character term (not an actual MD5 digest), with sixteen tuple offsets per
heap block. `DIAG_PARTS=1|8` selects contiguous sources; supply 32,768 or 4,096
documents per source respectively. Set both variables when preparing and running.
This emulates key codec workload properties, not exact PostgreSQL heap placement.

The narrowed workload reproduced the gap. On the first pass, eight sources with
75% dead took 69.72 ms in the pre-optimization direct path, 67.17 ms after the
first optimization, and 57.79 ms in reconstruction. Unique terms largely erased
the earlier two-term verifier benefit.

The next candidate (`cfe57ee`) uses binary lookup for singleton terms, reuses
per-term input/cursor/heap and verifier TID/bound buffers, retains the already
scanned postings cursor for bounds, and avoids creating payload cursors for
fully validated dead-only singleton terms. Bounds still use their original
checked decoder, and every skipped singleton retains its cancellation callback.

Five alternating trials against `7f40c2e` yielded these diagnostic medians (ms):

| Sources | Dead | Old direct | Updated direct | Old reconstruction control |
| --- | ---: | ---: | ---: | ---: |
| 1 | 0% | 84.09 | 78.60 | 131.67 |
| 1 | 75% | 69.04 | 62.57 | 60.35 |
| 8 | 0% | 85.79 | 83.58 | 122.47 |
| 8 | 75% | 70.55 | 64.68 | 59.05 |

All complete output hashes matched. The remaining gap means this is an
incremental candidate, **not proof the database regression is resolved**.
Raw records: `/tmp/vacuum-sql-compare.json`; earlier fixture/singleton/scratch
probes use `/tmp/vacuum-sql-compare-{first,singleton,scratch}.json`. Whole-segment
validation passed 88 tests, including reused bounds after compact/long scans,
corruption and unbounded input. End-to-end and short-document measurements remain
the promotion gate.
