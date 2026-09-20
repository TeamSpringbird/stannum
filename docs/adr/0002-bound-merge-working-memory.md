# Bound merge working memory without changing segment geometry

Status: proposed; output-spill prototype only, disabled by default.

## Problem and constraints

The 500k Wikipedia/default-batch build still OOMs under a 1.25 GiB container
cap after output allocation reuse and earlier publication-buffer release.
Lowering batch size previously enlarged the index and degraded million-row
query throughput. Keep batch/merge selection and LSG3 bytes unchanged while
reducing execution retention. A refused merge is not a successful capacity fix.

Existing `MergeLimits` cover input/output format admission, not live allocation.
A true execution budget must account for input staging, validation, document
metadata, per-term encoding, retained output areas, and publication transfer.
Allocator retention, PostgreSQL memory and shared buffers also affect process
RSS; a Rust buffer budget cannot promise a particular container limit.

## First experiment: retained encoded output

`OutputSink` separates direct merging from retention of accumulated postings and
payloads. The existing memory destination preserves allocation reuse. A separate
PostgreSQL destination uses resource-owner-managed `BufFile` temporary files
when its two output areas would exceed their shared capacity budget. Final
publication reads one page at a time, preserving the reverse-linked run and page
map rather than assembling another full segment vector. No new format is needed.

`stannum.experimental_merge_output_kb` is zero/off by default. Nonzero values
apply only to admitted foreground merges, including CREATE INDEX and buffer
folding. It is deliberately not named a total merge budget. The experiment does
not change VACUUM, oversized aggregate fallback, scheduling or lock ownership.

Temporary files must respect PostgreSQL's temporary-space limit, be cleaned up
on cancellation/error and remain private until output publication. Full input
validation and duplicate-live-TID rejection remain mandatory. Partial output
must never enter the directory. Preserve existing orphan-page recovery.

## Remaining stages before a total budget

1. Bound input retention. The existing `Reader<Source>` arena retains all fetched
   extents for its lifetime; using it over pages would eventually reload the
   whole input. Introduce maintenance cursors that own and release dictionary,
   postings, payload and length blocks as they advance. Keep query readers'
   borrowed-slice lifetime guarantees intact.
2. Make validation streaming too. Preserve bounds, ordering, document ownership,
   frequency and score-bound checks, including dead postings. Do not silently
   replace whole-input validation with validating only surviving postings.
3. Account for document metadata and oversized individual terms. Output spilling
   alone leaves the live-document map, dictionary, lengths, document postings and
   per-term codec builders in RAM. Each needs a measured bound or spill path;
   a single common term must not bypass the executor's eventual budget.
4. Apply the same executor to VACUUM snapshots with generation revalidation,
   cancellation, failure recovery and deferred reclamation. Use PostgreSQL's
   maintenance memory setting only after its effective scope is honest.

## Acceptance gates

- Reference-byte equality and same-engine membership/ranking after construction,
  folding, deletion, maintenance and REINDEX; legacy inputs and malformed inputs.
- Retained 1.25 GiB reproducer completes, with actual caps and OOM failures saved.
- Compare build time, temp I/O, peak sampled anonymous memory, index size,
  directory shape and all 906 query forms. Include a comfortable-memory control;
  do not call one timing window a speedup.
- Error during temp write/read, temp-file limit, cancellation after spilling,
  rollback/retry, crash/orphan recovery and replica behavior before default use.
- Bound input, metadata and individual-term allocations before claiming a total
  execution budget. Do not enable by default merely because output spilling helps.
