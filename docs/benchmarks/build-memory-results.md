# Build batches and merge allocation peaks

The default batch fails with a 1 GiB construction cap on the exact first 500,000
Wikipedia rows. Smaller batches can help at that size, but change merge tiers
and allocated index size. These runs do not justify changing the default.

| Rows | Batch docs | Build cap | Build status | Build seconds | Sampled anonymous MiB | Query QPS | Index file MiB |
|---|---:|---|---|---:|---:|---:|---:|
| 500000 | 8192 | 2g | complete | 64.4 | 488.8 | 629.5 | 1068.8 |
| 500000 | 32768 | 2g | complete | 72.5 | 1236.4 | 632.2 | 766.0 |
| 500000 | 32768 | 1g | failed | — | 723.9 | — | — |
| 500000 | 8192 | 1g | complete | 66.1 | 439.1 | 626.6 | 1068.8 |
| 1000000 | 8192 | 2g | failed | — | 1302.1 | — | — |

Failed builds have no query result or complete construction duration.
Successful query measurements use 2 GiB after construction, two readers,
60 seconds of traffic and 15 seconds of warmup. They use all 906 forms and
explicitly sample 18 forms for membership/exhaustive same-engine ranking checks.
These are single configuration probes, not paired statistical performance claims.

The first two runs used the initial runner override and preserve the applied
Docker command. Subsequent runs additionally record the effective batch setting
in the CREATE INDEX session and capture segment layout after construction.

## The larger-data counterexample

The 8,192-document setting is OOM-killed at one million documents under 2 GiB.
Docker records OOMKilled=true and the server records signal 9 during CREATE INDEX.
The [earlier default-batch million-document run](repeated-memory.md#one-million-document-scale-probe)
completed under the same cap, using the same immutable engine image.
A diagnostic replay completes eight merges, starts a ninth, then the same
backend is killed by signal 9 without emitting `merge-end`. This confirms the
failure is inside merge execution, consistent with the next tier. No final
allocation counter survives the kill, so its exact requested-byte peak is unknown.
This cannot safely be adopted on the strength of the 500k result.

## Allocation diagnostic

A throwaway image wraps the Rust system allocator, tracking requested live bytes
and phase peaks. It excludes PostgreSQL palloc, allocator overhead and RSS.
Instrumentation adds overhead; its timings are not performance measurements.
The exact patch and source/image provenance are retained in the evidence archive.

| Phase | Maximum live MiB at marker | Maximum phase peak MiB |
|---|---:|---:|
| accumulation-end | 727.6 | 727.6 |
| encode-end | 150.2 | 729.3 |
| merge-end | 890.2 | 1298.1 |

This identifies a real live-allocation peak in merging, beyond the approximately
728 MiB accumulated builder. Lowering document count alone is not a general
memory bound. The direct merge retains encoded inputs while producing output,
and its admission limits are format limits rather than memory budgets.

Tier size depends on the count of nonempty indexed documents. With fan-in eight,
8,192-document batches can eventually merge eight 65,536-document segments;
the 500k prefix has not reached that next tier. A larger-corpus control is
therefore essential before recommending smaller batches.

Index file size includes allocation/reclamation history; it is not a measure
of live encoded postings alone. The smaller-batch 500k run increases allocated
file size despite similar query throughput. Segment and query costs must remain
part of any memory optimization decision.

## Next implementation work

Design a merge admission and execution budget that accounts for retained input
blobs, live-document metadata and output construction. Deferring oversized
merges must also address the segment-directory limit and read amplification;
a bounded streaming or spill-capable merge may be necessary. Preserve the
current default until those tradeoffs have correctness and sustained-write proof.

[Machine-readable evidence](build-memory-results.json) retains successes, failures,
source/data/trace identities, allocation markers and compact query metrics.
Full raw evidence is retained locally under the run directories named there.

## Reproduction and evidence

Use the command in [the repeated-memory report](repeated-memory.md#reproduction),
adding `--build-segment-docs 8192` or `32768`. For the 1 GiB construction probe,
set `--build-memory 1g --memory 2g`. For the larger counterexample, use
`--rows 1000000 --build-memory 2g --memory 2g --build-segment-docs 8192`.
Failed builds must retain their manifests, Docker state and server logs.

The diagnostic allocator is absent from production source. Its exact build patch,
source/image identities and markers are retained locally. The combined raw archive
(CSV copies excluded) is `benchmarks/results/build-and-targeted-memory-evidence.tar.gz`,
SHA-256 `b62ba840393facc3314af622cbb21339902f338a7bdcf777aff993655ef608d7`.
Compact measurements are committed; the raw archive is local.
