# Foreground direct-merge integration

Foreground merges call `segment::merge::merge` while retaining the existing
exclusive metadata lock. Tier selection, merge budgets, generation allocation,
LSG3 output, WAL publication and retirement remain unchanged. VACUUM retains its
separate unlocked reconstruction/revalidation path. This does not include the
unlocked-fold experiment in PR #10.

The new path loads each selected source blob and dead set, verifies all sources,
and traverses their sorted dictionaries and postings. Those owned inputs are
released before output-run allocation. Aggregate encoded bytes and document
counts must fit `u32::MAX`; otherwise the existing per-source reconstruction path
runs. This fallback preserves merges whose aggregate input exceeds one run's
format limit but whose live output still fits. The admission limits are format
bounds, not a peak-memory budget. Existing infallible codec allocations remain;
process-wide OOM is not recoverable through this API.

## Cancellation and failure recovery

PostgreSQL's buffer content LWLock holds off interrupts. The merge callback
checks for interrupts but respects that deferral. Insert checks once more after
metadata publication and lock release. A pending cancellation can therefore
complete merge construction and publication before aborting the SQL statement.
This does not promise bounded cancellation latency.

Two PostgreSQL regression tests exercise distinct cases:

- An injected ERROR after merge construction, before publication, leaves the
  directory and visible results intact. A retry succeeds. Unpublished fold pages
  are the only verifier findings, and existing VACUUM cleanup reclaims them.
- A pending query cancellation at a merge checkpoint is deferred under the lock,
  then delivered after unlock. Construction completes; the statement rolls back
  and the next insert succeeds with correct visible results and a clean index.

The codec API independently tests cooperative cancellation at every exposed
checkpoint. Existing lifecycle tests cover dead lists, CTID reuse, ranked
queries, crash recovery and standby WAL replay.

## Measurement protocol

Compare a release build of main `26b9d1e` against this integration, using retained
library copies and SHA-256 identities. Run the existing contention harness with
20-second windows, two writers, two readers, 32-document buffers and a 1,024-document
merge budget. Use three alternating pairs each for short and long documents
(repetition counts 20 and 200). The private PostgreSQL 18 cluster uses 512 MB
shared buffers, a 30-minute checkpoint timeout and 16 GB maximum WAL size, with
an explicit checkpoint before each harness invocation. Retain the server log to
check for checkpoints during traffic.

The memory probe runs each method in a separate release process, with fixture
construction in a prior process. `/usr/bin/time -l` measures peak resident memory
on macOS. Each fixture contains eight segments with interleaved TIDs and no dead
documents. Reference reconstruction retains one encoded source at a time;
direct merging retains all encoded sources. Parsing, file I/O and output
construction are timed, with output-file writing excluded from the timer but
included in process RSS. Every output must match byte for byte. These are
isolated codec-process measurements, not PostgreSQL backend peak-memory figures.

```sh
cargo build --release -p segment --example merge_memory
target/release/examples/merge_memory prepare /tmp/merge-fixture 512 400 400
/usr/bin/time -l target/release/examples/merge_memory reference /tmp/merge-fixture /tmp/reference.segment
/usr/bin/time -l target/release/examples/merge_memory direct /tmp/merge-fixture /tmp/direct.segment
cmp /tmp/reference.segment /tmp/direct.segment
```

## Initial integration results

Apple M4 Max, ARM64, PostgreSQL 18.6, Rust 1.96.0 release builds. Baseline runtime
source is `26b9d1e`; integrated runtime source is `a6269f4`. Subsequent lifecycle
harness changes do not change either library. Retained library SHA-256 values:

- Baseline: `ae2b0559b84bd4e2108068a6c88d9b0aba4434c7cba383d8081ccb27eeb5665a`
- Integrated: `03caf61cb9152957dae8d89a7abc939d3fc6e848532569e9d3a7af9f13ec9a3c`

All twelve contention windows passed membership oracles during traffic, final
insert accounting and index verification. Server logs show no checkpoint starts
inside the shared traffic windows. Medians of three windows per build:

| Workload | Metric | Baseline | Integrated | Change |
| --- | --- | ---: | ---: | ---: |
| Long documents | Writer transactions/s | 7,021 | 7,294 | +3.9% |
| Long documents | Reader transactions/s | 1,918 | 2,062 | +7.5% |
| Long documents | Writer p99 ms | 4.649 | 4.259 | -8.4% |
| Long documents | Reader p99 ms | 6.691 | 6.243 | -6.7% |
| Short documents | Writer transactions/s | 9,529 | 9,904 | +3.9% |
| Short documents | Reader transactions/s | 1,709 | 1,689 | -1.1% |
| Short documents | Writer p99 ms | 2.935 | 2.836 | -3.4% |
| Short documents | Reader p99 ms | 4.904 | 4.833 | -1.4% |

Writer p95 was approximately unchanged (long: 1.430 → 1.425 ms; short: 1.192 →
1.185 ms). Reader p95 fell from 4.676 to 4.278 ms for long documents and 4.028 to
3.961 ms for short documents. The long-document paired writer throughput changes
were -1.4%, +3.2%, +4.1%; the aggregate median is not a guarantee for every run.
These are closed-loop tests against a growing table: different throughput means
different final table sizes and reader work. Wait-event samples do not measure
metadata-lock duration. This supports a modest local improvement, not a general
4% throughput guarantee or a comparison with TIN.

Isolated process medians from three alternating pairs, identical output bytes in
every run (RSS in MiB):

| Docs/segment | Tokens/doc | Vocabulary | Reference ms | Direct ms | Reference peak RSS | Direct peak RSS |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 32 | 40 | 4 | 0.693 | 0.396 | 2.16 | 2.03 |
| 512 | 400 | 4 | 11.471 | 7.136 | 16.34 | 9.30 |
| 512 | 400 | 400 | 267.531 | 100.597 | 153.59 | 29.02 |

Retaining encoded sources cost less than reconstructed document state in these
fixtures. Small-process RSS includes startup overhead. This is not a bound on
large or adversarial inputs, and does not establish PostgreSQL backend peak RSS.

## Validation status

Local formatting, all 62 harness tests, 454 core unit tests, four documentation
tests, warnings-denied Clippy for PostgreSQL 17/18, and all 109 tests on each major
passed. Lifecycle validation passed, including crash/replay, CTID reuse, ranking,
promotion and 338 standby snapshot comparisons with zero wrong answers.

The first lifecycle run hit a valid PostgreSQL recovery-conflict session
termination that the harness recognized only as statement cancellation. The
harness now accepts both exact recovery-conflict messages; unlimited-delay and
feedback-enabled cases still require all 160 answers and no cancellation, and
unrelated connection errors still fail. PostgreSQL's
[recovery-conflict handling](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/tcop/postgres.c)
explicitly supports both outcomes. The full lifecycle rerun passed.

All five ranked-fuzz smoke runs passed (760 ranked comparisons). Cross-architecture
performance validation remains a promotion gate; local ARM64 timing does not
establish x86-64 behavior. Raw local samples, binary identities, server logs and
probe outputs are retained under ignored
`benchmarks/results/direct-merge-integration/`.

The CI workflow supports an opt-in paired contention campaign across Linux
ARM64/x86-64 and PostgreSQL 17/18. It compares two release libraries on each
runner, restores the integrated library, and uploads raw results. Shared-runner
timings are evidence for review, not a timing assertion in the correctness suite.

```sh
gh workflow run ci.yml --ref perf/integrate-direct-merge -f direct_merge_performance=true
```

The portable driver can also compare retained, SQL-compatible libraries on a
private local cluster (use the development installation lock on a shared host):

```sh
python3 benchmarks/paired_libraries.py --baseline /tmp/baseline.so \
  --integrated /tmp/integrated.so --installed-library /path/to/stannum.so \
  --checkpoint-control --seconds 20 --rounds 3 --repeat 200 \
  --output benchmarks/results/direct-merge-pairs
```

## Fixed-work follow-up

The initial integration's fixed-work probe inserted the same 4,161 long documents
per window, using three alternating build pairs and rotating budgets 0/256/2048.
All 18 windows passed correctness checks. Median total INSERT execution times:

| Merge budget | Baseline ms | Initial integration ms | Change |
| ---: | ---: | ---: | ---: |
| 0 | 151.070 | 148.412 | -1.8% |
| 256 | 154.554 | 162.775 | +5.3% |
| 2048 | 189.884 | 169.193 | -10.9% |

Budget zero still incurs two emergency merges at the hard directory bound.
At budget 256, median total execution time in the 16 fold-and-merge INSERTs rose
from 19.657 to 21.494 ms. This is a real tradeoff hidden by broader throughput
medians, although non-merge timing also varied. WAL totals were identical at
budgets 0 and 256; at 2048 they differed by 34 bytes out of approximately 9.3 MB.
These are complete INSERT measurements, not isolated merge phase timings.

A separate in-process diagnostic reproduces the smaller-merge overhead when each
document has a unique term, alongside 400 alternating common/filler positions
and a term shared by document ID modulo 97. With eight contiguous 32-document
segments, initial validated merging took 1.002 ms versus reconstruction's 0.937 ms;
standalone verification took 0.417 ms. This implicates verification overhead and
the many singleton terms, not just server timing noise. Timings alone do not
attribute all overhead to verification.

The follow-up reuses verifier ordinals, score tuples, positional scratch and
expected-bound buffers across terms. It also avoids cloning each dictionary term
and formats its diagnostic label only when emitting a finding. Every consistency
check remains, and scratch consumers clear state before use, including after a
malformed term skips later checks. Full validation and fresh measurements of this
revised runtime are in progress; the tables above describe the initial build.
