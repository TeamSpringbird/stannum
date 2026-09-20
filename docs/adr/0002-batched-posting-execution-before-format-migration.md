---
status: accepted
---
# Batch posting execution before migrating the stored format

## Decision

Move Boolean matching and counting toward bounded batches of physical heap-page
bitmaps, with contiguous machine-word storage and interchangeable scalar and
SIMD implementations. Keep sparse postings compact. Prove the execution change
against the current encoding before introducing a new on-disk format.

This extends [ADR 0001](0001-preserve-posting-order-before-changing-encoding.md).
The direction is accepted; batch size, memory layout, instruction set, and a new
writer remain experimental until measured. TIN's published use of vectorization
motivates this work but does not establish how much of its performance comes
from SIMD or reveal its complete representation.

## What exists today

`segment/src/postings.rs` chooses between sparse delta lists and grouped postings.
A group covers 256 heap pages, with a 32-byte page-presence bitmap. A populated
page uses either a list of offsets or a 37-byte tuple bitmap. The tuple bitmap
has 291 usable bits, indexed by the one-based heap offset; padding is invalid.

`segment/src/pages.rs` decodes offsets into five u64 words (40 bytes) and implements
OR, AND, subtraction and population count. Dense postings can already be read
without enumerating tuple IDs. Query execution is page-at-a-time, however, and
both scalar and page unions scan their inputs to select the next result.

The scalar PostgreSQL count path collects all candidate TIDs, sorts and deduplicates
them, then groups them by heap page. Page counting streams masks instead. Selection
currently depends on whether any queried posting is grouped and averages at least
four tuples per populated page. This does not estimate union density across terms.

Thus the missing capability is not simply "store bits." It is maintaining useful
ordering, batching decode and set operations, and avoiding expansion into individual
TIDs when the consumer only needs an exact count.

## Proposed module interface

The experimental `segment` posting-batch module owns decoding, ordering, mask
combination, deduplication and bounded scratch storage. PostgreSQL remains
responsible for snapshot visibility and query rechecks.

A batch cursor exposes a small interface: advance into caller-owned bounded
storage, seek to a heap block, and report exhaustion or corruption. Each batch
contains increasing unique block numbers and a corresponding exact tuple mask.
No empty pages or padding bits escape the module. Memory is O(batch size times
active inputs), not O(total matches); reject or fall back when the scratch budget
would be exceeded. Do not retain batches for every term or segment indefinitely.

Two initial adapters make this a real seam:

1. Existing sparse cursors grouped into bounded page batches.
2. Existing grouped postings decoded directly into those batches.

Both work with existing immutable segments and dead-posting filters. The scalar
implementation is the portable reference; CPU-specific kernels stay behind the
same interface. Planning and count execution must not branch on AVX/NEON details.

Compare an array of page records with a structure of arrays (block IDs plus
contiguous mask-word lanes). Try several small batch sizes; do not select a
layout purely from SIMD width. Padding each five-word mask to eight words costs
60% more mask memory, so any alignment benefit must outweigh that cost. Prefer
bounded aligned scratch storage over padding the persistent format by default.

## Optimization sequence

1. **Establish slow-shape evidence.** Replay selected fast, median and expensive
   OR queries outside benchmark traffic. Record count strategy, candidates,
   visibility checks, heap fetches, CPU profile, allocations and decoded bytes.
   The existing six plan captures do not cover the worst query.
2. **Remove unnecessary work.** Compare scalar streaming counts with current
   materialize/sort/deduplicate counts. Preserve duplicate handling across
   segments and dead/live versions. This gives a fair scalar reference.
3. **Batch the current format.** Compare scalar batch operations with existing
   page-at-a-time execution; measure decoding plus combination plus consumption,
   not merely an isolated OR loop. Evaluate smarter union selection and
   result-density planning independently so benefits can be attributed.
4. **Add vectorized kernels.** Compare scalar/autovectorized and explicit SIMD
   kernels on x86-64 and ARM64. Detect the required CPU features at runtime;
   baseline binaries must remain runnable without optional instructions.
   Measure decode, Boolean operations and counting separately. Vector OR does
   not imply equally efficient vector population count on every CPU.
5. **Consider a new encoding only if decode remains material.** Prototype
   independently decodable, bit-packed blocks of sorted sparse IDs/deltas plus
   adaptive dense bitmap blocks. Carry explicit lengths, skip information and
   byte order. The serialized bytes must not depend on the build machine's ISA.

A dense bitset across the entire CTID address space is not the default: holes
in heap blocks and sparse terms can make it prohibitively wasteful. Sparse lists,
bit-packed blocks and dense masks should coexist behind the same batch interface.

## PostgreSQL correctness constraints

- Only exact results on all-visible heap pages can be counted without heap
  access. Mixed visibility and inexact plans retain snapshot checks/rechecks.
- AND-NOT requires a valid document universe; unused offsets are not documents.
- Deduplicate overlapping physical TIDs; subtract dead entries per source before
  union so a dead version in one source cannot suppress a live one in another.
- Preserve HOT/update, deletion, CTID reuse, rescans and interruption behavior.
- Counts do not decode positions or scores. Ranking and phrases retain their
  payload streams and posting ordinals; a new encoding must preserve their
  association with each posting rather than assuming count equivalence proves it.
- Count metadata alone cannot replace MVCC-aware evaluation.

## Migration gates

No format change is needed for the initial adapters or SIMD batch kernels.
If an encoding POC earns promotion, introduce a new version with explicit reader
support first. Test old/new/mixed segments and reject unsupported versions clearly.
Enable the writer separately; migrate through verified segment merges or explicit
REINDEX, never by silently reinterpreting existing bytes. Document that binaries
without the new reader cannot reopen new-format indexes, and provide a supported
rebuild path before removing any old reader.

Require exact result equivalence, malformed-input/padding/overflow tests, and
end-to-end PostgreSQL checks under deletes, HOT updates, rollback and concurrent
snapshots. Exercise WAL/recovery, restart, VACUUM, merge and replication behavior
before making the writer the default. Preserve scalar fallback tests even on
SIMD-capable CI machines.

## Performance acceptance

Use the completed two-client baseline and fixed replay queries for single-query
latency, and the concurrency sweep for capacity. Keep those comparisons separate.
Run alternating before/after repetitions at equal client counts and resources.
Report per-query distributions plus workload p50/p95/p99, throughput, CPU,
allocation/peak memory, bytes decoded, index size and build/merge costs.

Cover rare/common terms, small/wide unions, high/low overlap, sparse/dense pages,
multiple segments and mixed visibility. Reject a design that improves one bitmap
microbenchmark but regresses sparse queries or exceeds the memory budget. Choose
numerical promotion thresholds before each experiment; do not infer a promised
speedup from published TIN numbers.
