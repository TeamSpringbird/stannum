# First measured retrieval milestone — 2026-09-17

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../README.md) and [BENCHMARKS](../BENCHMARKS.md).

The fork now prunes persisted term, Boolean and phrase candidates. Five paired
release-build trials completed successfully on 10,000 synthetic documents with
ranked results, counts, phrases and scheduled writes. Median mixed read throughput
was **129.25 queries/s versus 41.17 for original Lead** (3.14x ratio of medians).
The median of the five paired throughput ratios was 3.14x.
This demonstrates a local structural improvement, not competitiveness with Tin or
other engines, and does not establish scaling on Wikipedia-sized documents.

| Measurement | Original | Fork |
| --- | ---: | ---: |
| Read QPS median | 41.17 | 129.25 |
| Read QPS trimmed mean | 40.73 | 130.16 |
| Read QPS minimum–maximum | 39.51–41.84 | 95.63–135.56 |
| Achieved writes/s median (20 scheduled) | 19.76 | 19.59 |
| Write p95 ms, median across runs | 53.844 | 30.331 |
| Index build seconds, median | 0.021 | 0.131 |
| Total index bytes, including primary key | 245,760 | 1,794,048 |

Median paired p50 latency speedups: rare count **48.78x**, AND count **50.16x**,
phrase count **107.94x**. Ranked rare/AND/phrase searches improved **2.88x / 4.26x /
3.25x**. Broad OR count/ranking stayed around **1.0x**. Individual query throughput
belongs to the shared mixed stream and is not isolated-query capacity.

Plans explain the difference: original Lead rechecked all 10,000 rows across 164
lossy heap pages. The fork retrieved 100 exact candidates for rare/AND and 10 for
the phrase. Phrase positions still come from heap rechecks; no persisted positional
or ranking statistics were added. Real postings introduce disk space and build cost.
The synthetic corpus has few unique terms and does not expose realistic build cost.

## Method and validation

Five pairs alternate original/fork order. Each uses a fresh container and named
volume, PostgreSQL 18.6 ARM64, four virtual CPUs, 4 GiB memory, no swap, 1 GiB shared
buffers, identical durable settings, two reader clients, 30 seconds of traffic and
five seconds of warmup. Paired seeds match. Host/VM caches are not forcibly cleared.
All samples are retained; each trimmed mean removes one minimum and maximum for
that metric. The fork's 95.63-QPS trial remains included. These are five independent
runs, not a significance test. No timed OOMs, memory-limit events or CPU throttling
were observed; host scheduling remains a source of variation.

All ten trials passed exact membership checks before and after writes. Paired
ranked top-k score vectors agree before and after writes; tied document order is
not required to match. All 43 PG18 tests, strict workspace Clippy, the isolated
crash/recovery/concurrency/standby lifecycle suite and 18 benchmark unit tests pass.
Independent review found no concrete Boolean/bitmap safety regression. PG17 and
exhaustive crash/fault injection remain untested for this slice.

The large frozen baseline launcher was paused between trials during builds and
this experiment. Its running trial finished before the experimental work began.
Neither its image nor its frozen protocol/data changed.

## Reproduction and retained artifacts

Both builds start from commit `3fcf441ac7c3d183de179b1f846ceb0ef83e1358`; the fork is
an uncommitted working tree identified by source SHA-256
`7459fd51ebdf8af447735dcd1315bf95493caefa72f951250619822f8d72f591`.

- Original image: `sha256:028b8e940aa33846aeb537e9097532e76e42b58dc3a03b32b2f8cba453728532`.
- Fork image: `sha256:436f6aaf9348f320c84afbeecbf2b252d286828fd4526b40e9451bf9b2d89a7e`.
- Full paired report (`benchmarks/results/paired-boolean-mixed-01/report.md`, local historical artifact),
  with JSON summaries, raw logs, SQL, plans, source identities and frozen protocol.
- The build artifact includes `source-tree.tar.gz`, containing tracked and untracked
  engine files verified against the source manifest. A commit plus tracked patch
  alone would not reconstruct this milestone.
- Persistent archive: `/Users/uri/Library/Application Support/LeadBenchmarks/archives/paired-boolean-mixed-01-20260917.tar.gz`.
- Archive SHA-256: `a107de6c32b2ecf844ad5d2af50b23bfd09d12f056ef9f39a6e14df63659f647`.

The frozen experiment ran the first paired driver under the external pause and
sleep-prevention supervisor. Subsequent driver review added explicit incomplete
report withholding, checked cleanup, a paired-run lock and signal handling;
those improvements did not change the timed SQL/harness or engine binaries.

Next: measure this image against the frozen 100k Wikipedia workload, profile bulk
insertion costs, and implement page reuse before sustained write claims. Persisted
ranking statistics remain a separate correctness-sensitive milestone. Reuse this
paired protocol to check each change and retain broad-query regressions visibly.
