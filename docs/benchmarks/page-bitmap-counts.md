# Page-bitmap count execution

Implementation: `60f0e89`, on `perf/page-bitmap-counts`, based on the validated
round-four integration branch. This change targets the custom Count node.

## What changed

Dense grouped postings can now expose exact heap-page offset masks without
expanding their stored tuple bitmaps into a vector of CTIDs. AND and OR combine
five `u64` words per page; NOT subtracts from the existing non-empty-document
universe. A streaming union deduplicates across index sources after subtracting
each source's dead list. All-visible pages use popcount; other pages use the
same snapshot/HOT heap-fetch and text-recheck rules as before.

A density heuristic keeps purely sparse and positional counts on the existing
scalar path. At least one Boolean term must have grouped postings averaging
four or more tuples per occupied page to select the bulk path. Group headers
provide this estimate without decoding offsets. Positional or capped expansion
subexpressions inside a bulk plan adapt the existing scalar evaluator and retain
its exactness flag. Inexact complements still use conservative candidates.

`EXPLAIN ANALYZE` reports `Count Strategy: page bitmaps` or `scalar`. The on-disk
format does not change. Ordinary search, ranked retrieval, and PostgreSQL bitmap
scans keep their existing execution strategies. There are no architecture-specific
SIMD intrinsics; this establishes the bulk word operations that can be profiled
for later SIMD work.

The page path bounds *candidate* buffering by the number of input streams and
query nodes. Encoded postings, dictionaries, and the mutable index retain their
existing memory behavior. The sparse fallback still materializes scalar candidates.

## Validation

- 438 core unit tests and four documentation tests passed; three optional
  probes/fixture writers remain ignored.
- 103 PostgreSQL extension tests passed on each of PostgreSQL 17.11 and 18.6
  on local ARM64 macOS.
- Lifecycle/recovery checks passed, including 340 correct standby answers,
  zero wrong answers, and one expected recovery conflict.
- Five concurrent ranked-smoke scenarios passed 750 comparisons covering
  67,296 rows, with no skipped comparisons.
- Formatting and Clippy with warnings denied passed for both PostgreSQL majors.
- New randomized page-algebra tests compare against independent ordered sets;
  dense boundary tests cover offset 291 and seeks across 256-page groups.
- Page plans run against the existing query/reference-evaluator tests, including
  expansion caps and nested Boolean/positional expressions.
- A new SQL regression compares dense counts with independent heap predicates
  after HOT updates, deletes, and indexed-text changes. The lifecycle harness
  now also compares Boolean counts against regex predicates in one snapshot.

[CI at the implementation commit](https://github.com/TeamSpringbird/stannum/actions/runs/35401341868)
passed all six jobs: formatting/harness tests, the upstream Lead oracle, and
PostgreSQL 17/18 on Linux x86-64 and ARM64.

## Measurement protocol

The baseline is the release library from `0ab416a`, the validated round-four
code, not Lead, GIN, or TIN. Both versions read the same indexes, built by the
baseline, in disposable databases. Each measurement uses fresh backends; the
installed library hash is checked before and after the window. Extension
installation, tests, and benchmarking use the machine-wide pgrx lock.

The machine is native ARM64 macOS with PostgreSQL 18.6, 512 MB shared buffers,
and 256 MB maintenance memory, without container CPU or memory limits. Both
fixtures contain 100,000 documents: the existing synthetic fixture and the
verified Wikipedia dataset. Indexes are vacuumed before measurements.

Each fixture has five alternating baseline/changed pairs, two readers, two
seconds of warmup and ten seconds of measurement per window, without writers.
The workload uses the existing count cases from `benchmarks/run.py`. The
synthetic workload adds these three queries, weighted equally with the others:

```sql
SELECT count(*) FROM documents WHERE body ==> 'common AND filler';
SELECT count(*) FROM documents WHERE body ==> 'common OR filler';
SELECT count(*) FROM documents WHERE body ==> 'common AND NOT rare';
```

Every window checks answers against the fixture's expected counts and records
query plans and per-query pgbench latency samples. These are short, warm-cache,
read-only measurements. They do not establish sustained write performance,
out-of-memory-scale behavior, or comparative performance against TIN.

## Why selection is adaptive

An initial always-page execution experiment improved dense synthetic queries
but regressed the sparse Wikipedia workload. Building a page mask around an
isolated posting adds work. Those exploratory results motivated the density
selection above; they are not the final implementation's benchmark results.

## Final results

These results measure the adaptive implementation, `60f0e89`.

| Fixture | Baseline median queries/s | Changed median queries/s | Interpretation |
| --- | ---: | ---: | --- |
| Synthetic count mix | 2,551.8 | 10,443.8 | 4.09x throughput; all five pairs improved |
| Wikipedia count mix | 6,112.1 | 6,264.4 | Roughly unchanged; see run-order limitation below |

Per-pair throughput, queries/second:

| Pair | Synthetic baseline | Synthetic changed | Wikipedia baseline | Wikipedia changed |
| --- | ---: | ---: | ---: | ---: |
| 1 | 2,523.2 | 10,494.7 | 6,024.7 | 6,264.4 |
| 2 | 2,655.8 | 10,201.2 | 6,390.2 | 5,933.3 |
| 3 | 2,499.1 | 10,462.2 | 6,018.1 | 6,293.2 |
| 4 | 2,643.6 | 10,255.0 | 6,347.5 | 6,005.4 |
| 5 | 2,551.8 | 10,443.8 | 6,112.1 | 6,325.3 |

Median per-window query latencies, milliseconds:

| Fixture / query | Baseline p50 | Changed p50 | Baseline p99 | Changed p99 |
| --- | ---: | ---: | ---: | ---: |
| Dense AND | 1.726 | 0.147 | 2.059 | 0.185 |
| Dense OR | 1.287 | 0.154 | 1.684 | 0.196 |
| Common AND NOT rare | 2.517 | 0.873 | 3.001 | 1.389 |
| Synthetic rare term | 0.058 | 0.060 | 0.104 | 0.077 |
| Wikipedia common term | 0.301 | 0.302 | 0.393 | 0.381 |
| Wikipedia common phrase | 1.854 | 1.817 | 2.209 | 2.176 |

All twenty windows passed correctness checks and recorded zero transaction
failures. Dense synthetic queries selected page bitmaps with zero heap fetches;
rare and phrase queries selected scalar execution. Every measured Wikipedia
query selected scalar execution.

The Wikipedia median throughput is 2.49% higher, but the changed build wins only
three of five pairs, and the second run in each pair is consistently faster.
Treat that as measurement/order variability, not evidence of a Wikipedia speedup.
Synthetic rare-term p50 increased by 0.002 ms; this implementation does not claim
that every individual query gets faster. The large, consistent gains apply to
dense Boolean counts on all-visible pages.

Raw final logs, query plans, per-query latency samples, binary hashes, and the
local campaign driver are retained in ignored
`benchmarks/results/page-bitmap-counts/`. The baseline library SHA256 is
`73ecbdfa7417c101fd05d1aabd50ad499eeac4b46b747e6fa578a1078ef12ed6`;
the measured changed library SHA256 is
`7f5b8aff06719e7fd68bdb64b26962e064b8c5a620775bd86faf30fa96d131fa`.
The changed library was restored after measurement. No release or tag was made.
