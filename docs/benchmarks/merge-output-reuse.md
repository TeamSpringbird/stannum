# Reusing the merge output allocation

The merger previously retained its encoded sections while assembling a separate
output vector. It now reuses the largest section allocation, moves that section
to its final offset, and copies the remaining sections into place. Document
metadata is released before assembly. Serialized bytes, batch defaults and the
merge-tier policy remain unchanged.

The segment suite passes byte-for-byte comparisons against the reference merger
for every supported format, deletions and interleaved inputs. Assembly tests cover
every reused section, empty-section combinations, growth and output limits.

## Allocation profile

Requested Rust bytes from isolated instrumented images; excludes PostgreSQL
palloc, allocator overhead and retained arenas. Diagnostic timings are withheld.

| Phase | Before peak MiB | After peak MiB |
|---|---:|---:|
| accumulation-end | 727.6 | 727.6 |
| encode-end | 729.3 | 729.3 |
| merge-end | 1298.1 | 831.4 |

The merge peak falls about 36%, from 1,298 to 831 MiB. Sampled anonymous memory
during the default 500k construction falls much less (the earlier control was
about 1,236 MiB; the candidate control is about 1,181 MiB). This is not a 36%
reduction in resident RAM. Accumulation/encoding peaks remain unchanged.

## End-to-end boundaries

| Candidate case | Construction cap | Outcome |
|---|---|---|
| 500k rows, default 32,768-document batches | 1 GiB | OOM, as before |
| 500k rows, default batches | 1.25 GiB | OOM; fresh baseline also OOMs |
| 500k rows, default batches | 2 GiB | Pass |
| One million rows, 8,192-document batches | 2 GiB | Pass; prior image OOMs |

The million-document stress case builds in 145.7 seconds and completes all 906
timed query forms, passing the selected 18-form membership and exhaustive
same-engine ranking checks. Its 265.6 QPS / 18.97 ms p95 belongs to the smaller
batch configuration; it is not comparable to the default-batch million-row
query result. This preserves the reason for keeping the current default.

## Fresh default-batch query controls

An initial candidate window was about 5% below the earlier baseline. Four fresh
baseline/candidate/candidate/baseline runs check that observation under current
host conditions. Every run uses the same 500k prefix, 2 GiB build/query caps,
two readers, 4 Docker CPUs, 128 MiB shared buffers, 64 MiB maintenance_work_mem,
10-second warmup, 30-second measurement, default batch size and all 906 forms.
These controls explicitly sample three forms for untimed correctness; the
separate successful end-to-end cases use 18. Full-corpus Lead is not the oracle.

| Variant | Median QPS | QPS range | Median p95 ms |
|---|---:|---:|---:|
| baseline | 618.5 | 615.7–621.4 | 5.93 |
| candidate | 618.4 | 613.9–623.0 | 6.09 |

Paired candidate/baseline throughput ratios: 0.988–1.012.

Median throughput is effectively unchanged (618.5 versus 618.4 QPS). Candidate
p95 is about 0.16 ms higher in these short windows; this is retained rather than
claimed as a query-speed improvement. The initial 5% throughput drop did not
repeat in the fresh alternating controls.

These local Docker observations are not production-capacity estimates. Client
transport is included; guest reads can be served by host caches. Two repetitions
per variant show observed variation rather than statistical confidence intervals.

## Remaining work

This is allocation reuse, not a hard memory budget. Inputs and output still coexist
during merges; retained allocator memory and accumulation remain significant.
Next release the newly published segment buffer before maintenance reads it back,
then evaluate streaming or spill-capable merging with directory/read-cost controls.

[Compact evidence](merge-output-reuse-results.json) retains successful and failed
runs, configuration/source/data identities, allocation peaks and paired metrics.

## Validation and retained artifacts

The segment suite passes 94 tests, including byte-equivalence checks for every
supported format and allocation-reuse checks for each section and empty-section
combination. Clippy passes with warnings denied. PostgreSQL version/architecture
and upstream Lead oracle checks run in PR CI.

Local raw evidence is retained in
`benchmarks/results/reuse-merge-evidence.tar.gz` (CSV copies excluded), SHA-256
`0ab12877bc298ee5d2260150b5aed3fa4931ab8f938be629947fb4db187e0db4`.
It contains source patches, immutable image identities, plans, samples, failures,
diagnostic instrumentation and the paired campaign's `orchestration.sh`.
The archive is local and is not committed; the compact JSON report is committed.
