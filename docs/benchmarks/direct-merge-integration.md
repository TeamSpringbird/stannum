# Foreground direct-merge integration

Foreground merges now call `segment::merge::merge` under the existing exclusive
metadata lock. They retain tier selection, budgets, generation allocation, LSG3
output, WAL publication and retirement. VACUUM retains its separate unlocked
reconstruction/revalidation path. The unlocked-fold experiment in PR #10 is not
included.

Each selected source blob and dead set is owned through validation and ordered
merging, then released before output-run allocation. Aggregate encoded bytes or
document counts exceeding `u32::MAX` retain the previous reconstruction fallback:
deletion can still produce a representable output from oversized aggregate input.
These format limits are not a peak-memory budget. Existing infallible codec
allocations remain; process-wide OOM is not recoverable through this API.

## Correctness and cancellation

PostgreSQL defers interrupts while the metadata buffer content lock is held.
Merge checkpoints respect that deferral; insert checks again after publication
releases the lock. A pending cancellation can therefore finish construction and
publication before aborting the SQL statement. This is not bounded-latency
cancellation.

New PostgreSQL tests cover both a pending cancellation delivered after unlock
and an injected ERROR after construction, before publication. They verify visible
results, directory integrity, successful retry and reclamation of unpublished
fold pages by existing VACUUM cleanup. The codec independently tests cancellation
at every exposed checkpoint. An admission test covers oversized aggregate fallback.

Local validation passed 454 core unit tests, four documentation tests, all 109
extension/storage tests on each of PostgreSQL 17 and 18, formatting, warnings-denied
Clippy and 63 Python harness tests. The final runtime passed lifecycle recovery,
CTID reuse, ranked queries, standby replay and promotion, plus five ranked-fuzz
smoke runs with 771 comparisons. The
[Linux validation campaign](https://github.com/TeamSpringbird/stannum/actions/runs/35419912451)
passed on ARM64 and x86-64 with both PostgreSQL majors, including the upstream
Lead compatibility oracle and all 48 contention windows.

The first local lifecycle run exposed a harness mismatch: it recognized only
statement cancellation, although PostgreSQL can also terminate a session for a
recovery conflict. The harness now accepts both exact recovery-conflict messages.
Unlimited-delay and feedback-enabled cases still require every answer and no
cancellation; unrelated connection failures still fail. This matches PostgreSQL's
[recovery-conflict handling](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/tcop/postgres.c).
The complete lifecycle reruns passed.

## Runtime and protocol

Baseline runtime: main `26b9d1e`. Final runtime: `154868c`, including verifier
scratch reuse and lazy diagnostic labels. Later harness/documentation commits do
not change the extension library. Local release-library SHA-256 identities:

- Baseline: `ae2b0559b84bd4e2108068a6c88d9b0aba4434c7cba383d8081ccb27eeb5665a`
- Final: `cac665c501af1154a661a2f04c579deda38f12bac0e27f2abfeb2494c2df46ba`

Each contention campaign uses three alternating pairs of 20-second windows, two
writers, two readers, 32-document buffers and a 1,024-document merge budget.
Short/long documents repeat `common filler` 20/200 times, with a shared term and
a unique term per document. Private clusters use 512 MB shared buffers, a
30-minute checkpoint timeout and 16 GB maximum WAL size, with an explicit
checkpoint before each harness invocation. Retained server logs show no checkpoint
starts or completions inside measured traffic windows. Membership is checked
during traffic; final insert accounting and index verification must also pass.

These are closed-loop runs against a growing table. Faster writers increase
reader work, so reader throughput is not a comparison at equal corpus size.
Wait samples are observations, not metadata-lock durations. Shared CI runners
also introduce timing noise. Percentages below compare medians of three windows
per build; they are not universal speedup guarantees or a comparison with TIN.

## Linux results

Changes relative to baseline; negative latency changes are improvements:

| Architecture | PG | Documents | Writer tx/s | Reader tx/s | Writer p99 | Reader p99 |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| x86-64 | 17 | Short | +2.4% | +1.4% | -5.4% | -5.3% |
| x86-64 | 17 | Long | +2.2% | +1.0% | -8.7% | -5.6% |
| x86-64 | 18 | Short | +1.0% | +0.2% | -1.9% | -2.9% |
| x86-64 | 18 | Long | +0.7% | +2.8% | -7.4% | -6.4% |
| ARM64 | 17 | Short | +7.0% | -6.9% | -23.1% | +4.4% |
| ARM64 | 17 | Long | -0.2% | +0.5% | -0.4% | -2.1% |
| ARM64 | 18 | Short | +1.1% | -0.4% | -5.1% | -4.0% |
| ARM64 | 18 | Long | +2.5% | -2.1% | -7.4% | -6.0% |

The ARM64/PG17 short workload has a mixed tradeoff: more writes, fewer reads and
higher reader p99. This is not an across-the-board latency improvement. The
other seven reader p99 medians and all eight writer p99 medians improved.

## Local macOS results and equal-arrival control

Apple M4 Max, ARM64, PostgreSQL 18.6, Rust 1.96.0 release builds:

| Workload | Writer tx/s change | Reader tx/s change | Writer p99 ms, before → after | Reader p99 ms, before → after |
| --- | ---: | ---: | --- | --- |
| Long, unrestricted writers | +5.7% | +3.9% | 4.446 → 3.811 | 6.281 → 5.931 |
| Short, unrestricted writers | +7.4% | -11.9% | 3.124 → 2.674 | 5.088 → 4.768 |
| Short, writers targeted at 3,000/s | approximately flat | +1.9% | 1.952 → 1.894 | 1.106 → 1.049 |

The additional controlled campaign uses `pgbench --rate 3000 --random-seed=42`
for writers while readers remain unrestricted. All six windows passed. Final
corpus sizes were 60,638–60,640 rows, removing almost all corpus-size differences.
Writer scheduling-lag p95 was 1.007–1.058 ms across builds; rate-limited latency
includes scheduling delay, which the harness reports separately. This supports
the growth explanation for the unrestricted reader result, but does not prove
it for every platform or workload.

## Isolated peak memory

Each method runs in a separate release process; fixture construction runs in a
prior process. `/usr/bin/time -l` measures peak resident memory on macOS. Eight
segments have interleaved TIDs and no deletions. Reference reconstruction retains
one encoded source at a time; direct merging retains every encoded source.
Parsing, file I/O and construction are timed. Output-file writing is excluded
from the timer but included in process RSS. Every output matches byte for byte.
Medians of three alternating pairs, RSS in MiB:

| Docs/segment | Tokens/doc | Vocabulary | Reference ms | Direct ms | Reference peak RSS | Direct peak RSS |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 32 | 40 | 4 | 0.486 | 0.252 | 2.19 | 1.98 |
| 512 | 400 | 4 | 13.742 | 8.185 | 16.44 | 9.23 |
| 512 | 400 | 400 | 310.651 | 118.030 | 172.77 | 29.16 |

Retaining encoded sources cost less than reconstructed document state in these
fixtures. Small-process RSS includes startup overhead. These are codec-process
measurements, not PostgreSQL backend peak-memory figures or bounds on adversarial
inputs.

## Small-merge diagnosis and fixed-work limits

The initial integration exposed a smaller-merge cost: at budget 256, median total
execution time in 16 fold-and-merge INSERTs rose from 19.657 to 21.494 ms. A codec
probe reproduced the overhead with unique terms: eight contiguous 32-document
segments took 1.002 ms to validate/merge versus 0.937 ms to reconstruct; standalone
verification took 0.417 ms. Verification and singleton-term costs were material.

The final runtime reuses verifier ordinals, scores, positional scratch and
expected-bound buffers across terms, avoids cloning each dictionary term, and
formats diagnostic labels only when emitting findings. Every consistency check
remains. Scratch consumers clear state before use, including after a malformed
term skips later checks. Existing mutation and malformed-input tests pass.

Fixed-work campaigns insert the same 4,161 long documents per window, rotating
budgets 0/256/2048 over three rounds. The last campaign alternates binaries
immediately within each budget, rather than running all budgets for one build
first. All 18 windows passed:

| Budget | Total INSERT ms, baseline → final | Fold-and-merge INSERT ms, baseline → final |
| ---: | --- | --- |
| 0 | 146.920 → 166.920 | 1.070 → 1.305 |
| 256 | 158.657 → 158.531 | 21.105 → 20.102 |
| 2048 | 177.670 → 181.155 | 37.771 → 32.530 |

Budget zero still forces two emergency merges at the hard directory bound. Its
approximately 14% total-time variation, mostly outside those merges, and an earlier
campaign's different totals show substantial noise in complete INSERT timings.
The final merge-bearing groups at budgets 256 and 2048 improved, but these data
do **not** establish an overall fixed-work INSERT speedup. WAL medians were equal
at budgets 0/2048 and differed by 16 bytes out of 7.4 MB at budget 256. These are
complete INSERT observations, not instrumentation of individual merge phases.

## Reproduction and remaining limits

The opt-in CI campaign builds the pinned baseline and candidate on each runner,
restores the candidate library, and uploads raw pairs. There is no timing assertion
on shared CI hardware:

```sh
gh workflow run ci.yml --ref perf/integrate-direct-merge -f direct_merge_performance=true
python3 benchmarks/paired_libraries.py --baseline /tmp/baseline.so \
  --integrated /tmp/integrated.so --installed-library /path/to/stannum.so \
  --checkpoint-control --seconds 20 --rounds 3 --repeat 20 --writer-rate 3000 \
  --output benchmarks/results/direct-merge-pairs
cargo build --release -p segment --example merge_memory
target/release/examples/merge_memory prepare /tmp/merge-fixture 512 400 400
/usr/bin/time -l target/release/examples/merge_memory reference /tmp/merge-fixture /tmp/reference.segment
/usr/bin/time -l target/release/examples/merge_memory direct /tmp/merge-fixture /tmp/direct.segment
cmp /tmp/reference.segment /tmp/direct.segment
```

Use the installation lock on a shared development host. Raw local/CI artifacts
are retained under ignored `benchmarks/results/direct-merge-integration*`.
Initial-build measurements remain there; the tables above describe the final
runtime. Further work should measure backend memory on larger production-shaped
inputs and equal-arrival workloads on Linux, particularly the ARM64/PG17 reader
tradeoff. The separately gated SIMD-format POC and unlocked-fold experiment remain
independent changes.
