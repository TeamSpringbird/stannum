---
status: accepted
---
# Preserve posting order before changing encoding

Segment merges will move toward a term-ordered merge of existing sorted postings,
retaining physical PostgreSQL CTIDs and separate positional payloads. Prove this
algorithm with the existing segment encoding first; adopt a new SIMD-friendly
encoding only after an independently measured codec proof of concept. This avoids
coupling elimination of document reconstruction and regrouping to an on-disk
migration, and preserves the existing correctness baseline while each benefit is
measured.

The architecture separates three decisions:

- **Merge algorithm:** traverse sorted dictionaries and merge per-term postings;
  filter each input's dead tuples, carry lengths/positions, and rebuild output
  statistics and score bounds. Duplicate live CTIDs remain errors; an obsolete
  dead occurrence must not suppress a live occurrence in a different segment.
- **Encoding:** retain adaptive sparse lists and dense heap-page bitmaps as the
  baseline. Experiment with contiguous integer batches, block bitpacking or
  Stream VByte for sparse postings/positions. CTIDs are block/offset pairs, not
  arbitrary 32-bit document numbers. No codec or block width is selected yet.
- **Publication:** retain current WAL, metadata-lock and reclamation boundaries.
  Changing the merge algorithm does not establish that unlocked publication is
  safe or that optimistic fold construction improves concurrent throughput.

A direct merge may retain document-length lookup state and output buffers. Calling
it streaming describes ordered input traversal, not bounded total memory or
zero-copy disk I/O. A production implementation must expose these costs and support
interrupt checks; the experimental implementation remains test-only.

## Promotion and migration gates

1. Direct-merge POC: compare complete output bytes with the current builder for
   all supported formats and mixed-format input; cover deletes, empty output,
   overlapping CTID ranges, dead CTID reuse, duplicate live CTIDs, positions and
   score bounds. Record alternating release-mode timings and their limitations.
2. Production direct merge: harden malformed-input handling and resource limits,
   reduce peak allocation where measured useful, integrate cancellation, then
   validate lifecycle/recovery, ranked results and concurrent tail latency on
   ARM64 and x86-64 with PostgreSQL 17 and 18. Preserve existing encoding.
3. Codec POC: compare full decode/merge/query behavior, not just an isolated SIMD
   kernel. Measure bytes/posting, index size, WAL volume, peak memory, small-list
   overhead, random seeks and dense/sparse workloads on both architectures.
   Include scalar fallback and CPU-independent encoded bytes.
4. New format: only after those results justify the tradeoff, introduce a new
   versioned writer while retaining old readers. Test mixed old/new segments,
   merges, VACUUM, REINDEX, crash recovery and standby replay. Document upgrade
   sequencing and downgrade limits before enabling the new writer; do not
   silently rewrite or reinterpret existing format signatures.

## Alternatives and consequences

SIMD-accelerating the current reconstruction/sort/regroup path preserves redundant
work. Replacing every tree with a hash table changes lookup costs but does not
remove that round trip. A wholesale new encoding now would make attribution and
rollback harder. Direct merging is therefore first, with SIMD encoding a separate,
evidence-gated change. Additional CPU savings may shorten locked merge stalls,
but neither database throughput nor reduced contention follows from microbenchmarks
alone.

Relevant codec references: [Lucene packed postings](https://lucene.apache.org/core/10_5_0/core/org/apache/lucene/codecs/lucene104/Lucene104PostingsFormat.html),
[Stream VByte](https://github.com/fast-pack/streamvbyte), and
[Rust bitpacking](https://github.com/quickwit-oss/bitpacking).
