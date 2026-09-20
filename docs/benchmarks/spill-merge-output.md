# Experimental spilling of merge output

Status: prototype, disabled by default. This is an output-buffer budget, not a
total merge working-memory budget. See the [proposed architecture](../adr/0002-bound-merge-working-memory.md).

## What changes

The direct merger now supports a destination for accumulated postings/payload
bytes. The ordinary destination keeps allocation reuse. The experimental
PostgreSQL destination spills both growing sections to temporary BufFiles when
their combined retained Vec capacity would exceed the configured budget. Final
publication copies one index page at a time, preserving the existing format,
page map, merge selection and directory shape. It never rebuilds a full output Vec.

`stannum.experimental_merge_output_kb = 65536` selects a 64 MiB budget for those
two buffers. Zero disables the experiment. PostgreSQL temporary-file buffers,
input blobs, the document map, dictionary, lengths, per-term codec buffers and
publication page map are outside this number. Foreground merges only use it;
VACUUM and oversized-input fallback are unchanged. No default is enabled and
`maintenance_work_mem` is not falsely advertised as a total execution cap.

## Verification

- 94 segment tests pass, preserving legacy input handling and direct-merge bytes.
- 129 PostgreSQL 18 tests pass, including byte-for-byte spill output, reads across
  area boundaries and in reverse order, output limits, cancellation after output
  spilling, build/fold/reindex correctness and temporary-file quota rollback/retry.
- Clippy with warnings denied passes.

The quota test initially aborted the backend: dropping a BufFile during error
unwinding attempted another fallible flush. The prototype now leaves cleanup to
PostgreSQL's resource owner during Rust unwinding, avoiding a second error. The
retained test catches the original quota error, verifies no index was published,
then builds and checks the index successfully after removing the quota. The
initial benchmark image predates this repair and is labeled separately below.

## Measurement contract

500,000 original Wikipedia documents, default 32,768-document batches, four
Docker CPUs, 128 MiB shared buffers, 64 MiB maintenance_work_mem, two readers,
15-second warmup and 60-second measurement. Query memory is 2 GiB after the
construction cap is applied. All 906 forms must be observed; 18 selected forms
receive membership and exhaustive same-engine ranked checks. These checks are
not a full-corpus Lead oracle. Every successful run has zero selected mismatches.

Results are single windows with retained artifacts, not paired confidence
intervals or a query-speed claim. Container I/O includes WAL/index activity and
host caching; it does not isolate temporary-file traffic. Anonymous peaks are
sampled, not guaranteed maxima. Build memory pressure can alter the page cache.

| Version | Build cap | Build seconds | Sampled anonymous peak MiB | QPS | p95 ms |
|---|---|---:|---:|---:|---:|
| Prior release-buffer fix | 1.25 GiB | OOM | — | — | — |
| Prior release-buffer fix | 2 GiB | 66.26 | 1130.2 | 619.2 | 5.867 |
| Initial spill prototype | 1.25 GiB | 69.99 | 934.0 | 624.8 | 5.909 |
| Initial spill prototype | 2 GiB | 65.93 | 931.5 | 628.8 | 5.799 |
| Repaired spill prototype | 1.25 GiB | 67.31 | 947.6 | 627.8 | 5.938 |

Every successful run allocates 803,217,408 index bytes (766.0 MiB). The repaired
1.25 GiB run records 1.68 GB of container reads and 3.52 GB of container writes
during construction; the prior 2 GiB run records 1.07 GB / 3.10 GB. These caps
differ, and these counters are not isolated temp-file costs. The initial spill
2 GiB control records 0.78 GB / 3.19 GB. No paired I/O improvement is claimed.

The previous implementation OOMs under 1.25 GiB. Both prototype versions complete
that retained reproducer. The useful result is crossing that measured capacity
boundary without changing index geometry, not outperforming TIN or a guaranteed
percentage reduction in total RSS.

## What remains before default use

Bounded input/validation cursors, metadata and oversized-term accounting,
VACUUM snapshot/revalidation integration, explicit read/write fault injection,
crash/replay cleanup checks, and repeated performance controls. A page-backed
reader with an arena that retains every fetched extent is not bounded input
streaming. Keep the experiment off until those requirements have evidence.

[Compact evidence](spill-merge-output-results.json) includes immutable source/image
identities, corpus/trace, effective PostgreSQL settings, successes, the prior OOM,
correctness, samples summaries and aggregate container build I/O. Raw campaigns
and image source patches are retained locally under `benchmarks/results/spill-merge-*`.

The local archive `benchmarks/results/spill-merge-evidence.tar.gz` excludes CSV
copies and has SHA-256
`8278a781f9f6330830df17fc69fd939ebc70f17731f55551e6af951f77cc4e16`.
It includes both image source manifests/patches, hash-verified copies of the then
untracked spill module, orchestration, samples/plans, and failing/passing test
logs. It is not committed. The final image predates only a test-helper plan-setting
fix; production spill code matches the recorded image source. Directory entries
after construction match the prior successful run exactly.
