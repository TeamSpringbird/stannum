# Batched postings: production sources and the next experiment

Researched September 21, 2026. Primary-source review only; no implementation,
build, or benchmark was run. The experiment context supplied for this review is
that a wrapper-only sparse page-cursor specialization was neutral. That result
does not measure a reader-owned batch decoder. No source below establishes TIN's
codec or predicts a Stannum speedup.

## What production implementations actually do

### Lucene: specialize the operation where the decoded representation lives

Lucene 10.3.2's `Lucene103PostingsReader` maintains either an integer buffer for
packed deltas or a bitset for unary-encoded blocks. `refillFullBlock` decodes 128
packed deltas with prefix sums, or loads the bitset representation; tail postings
use a separate variable-integer path. Its `intoBitSet` override consumes buffered
integer ranges directly and uses `FixedBitSet.orRange` for bitmap ranges. It
therefore avoids repeatedly crossing the scalar `nextDoc` interface, and avoids
expanding an existing bitmap into individual IDs. `nextPostings` also bulk-copies
IDs and features, but falls back to the superclass when frequencies are not
needed: it is not the strongest count-only analogue.
[Release source: buffers, refillFullBlock, intoBitSet, nextPostings](https://github.com/apache/lucene/blob/releases/lucene/10.3.2/lucene/core/src/java/org/apache/lucene/codecs/lucene103/Lucene103PostingsReader.java).

The generic `DocIdSetIterator.intoBitSet` contract is equivalent to scalar
iteration, preserves pre-existing destination bits, and takes an upper bound and
offset. Specialized implementations can honor that contract while changing the
mechanism underneath it.
[Lucene 10.3.2 API](https://lucene.apache.org/core/10_3_2/core/org/apache/lucene/search/DocIdSetIterator.html#intoBitSet(int,org.apache.lucene.util.FixedBitSet,int)).

**Inference for Stannum:** make the batch fill method own decode state and bulk
output. Another wrapper around `current`/`advance` does not reproduce this design.

### Tantivy: expose decoded blocks as slices

Tantivy 0.26.1 documents postings as sorted, delta-encoded, bitpacked blocks of
128 document IDs, followed by frequency blocks; the incomplete final block uses
variable integers. Document IDs belong to a compact segment-local integer
space, not PostgreSQL heap addresses.
[Versioned architecture](https://github.com/quickwit-oss/tantivy/blob/0.26.1/ARCHITECTURE.md#postings-iterate-over-documents-very-fast).

`BlockSegmentPostings` exposes `docs()`, `freqs()`, block advancement and seek.
The official iteration example requests `IndexRecordOption::Basic` and consumes
document slices directly, while warning that deleted documents may still appear.
This is evidence for an existing production bulk interface, not evidence that
PostgreSQL visibility work disappears.
[Versioned API](https://docs.rs/tantivy/0.26.1/tantivy/postings/struct.BlockSegmentPostings.html),
[Official example](https://tantivy-search.github.io/examples/iterating_docs_and_positions.html).

**Inference for Stannum:** separate storage decoding from scalar cursor
presentation, and avoid decoding positions/frequencies on a count-only path.

### Roaring: container representation and bulk iteration are distinct choices

Roaring partitions 32-bit integers by their high 16 bits. Containers hold sorted
16-bit arrays, 8192-byte bitsets, or runs. The portable format's array/bitset
boundary is 4096 entries; this follows its container sizes and does not supply a
threshold for Stannum's much smaller heap-page masks.
[Authoritative format specification](https://github.com/RoaringBitmap/RoaringFormatSpec/).

CRoaring separately exposes `roaring_uint32_iterator_read`, which fills a
caller-owned integer buffer and advances the iterator. A bulk integer API still
materializes IDs; it should not be confused with retaining a bitset through a
Boolean operation.
[CRoaring public header](https://github.com/RoaringBitmap/CRoaring/blob/master/include/roaring/roaring.h).

**Inference for Stannum:** borrow bounded bulk consumption and representation
preservation. Adopting Roaring's persisted format or its thresholds is a separate
proposal requiring its own evidence.

## Sorted integer blocks versus heap-page masks

An integer block groups a fixed number of postings, potentially spanning many
pages. A page mask groups matching offsets belonging to one physical heap page,
with cardinality varying independently of the number of pages in a batch.
Converting integer blocks into page masks still requires page-boundary handling
and setting bits. SIMD integer decompression does not eliminate those steps.

Stannum already has the relevant destination: `Offsets([u64; 5])`, representing
291 valid offsets using zero-based bit positions. `Rows<C>::advance` groups scalar
TIDs into one mask by repeatedly calling `current` and `advance`. The sparse
reader decodes a block delta and offset varint for each posting and validates
ordering, overflow and offsets. Grouped postings already decode stored masks
directly. These are separate starting points for batching.
[Page representation and Rows](../../segment/src/pages.rs),
[SparseCursor and grouped decoding](../../segment/src/postings.rs).

## Smallest existing-format prototype

**Recommendation, not a measured result:** add one reader-owned sparse fill
operation that decodes the existing varints into a caller-owned, bounded slice
of complete `Page` records. Start with a small fixed capacity such as 32 pages
(a test parameter, not an established optimum). Decode successive entries in one
loop, accumulate offsets while the block number remains equal, and emit a page
only when it is complete. Retain the first posting of the next page as cursor
state when output is full. Preserve the current decoder's ordering, offset,
overflow, ordinal and seek invariants, including errors discovered at refill
boundaries. No persisted bytes or writer changes are necessary.

Keep a scalar cursor adapter over the buffered pages for compatibility, but make
the experimental consumer drain batches directly so the new boundary is actually
exercised. Grouped postings can fill the same page slice using their existing
direct-mask decoding. Begin with scalar masks and array-of-page records; SIMD,
new integer compression, padding masks, and changing union selection should be
separate experiments. Existing varints still require scalar parsing unless a
separately verified decoder is added; batching alone does not make them SIMD.

Future acceptance should compare decoder-plus-page-consumption work against the
current path, then end-to-end count execution. Cover sparse/dense pages, page and
batch boundaries, seeks, truncated/invalid streams, multiple sources, dead-entry
filtering and mixed visibility. Preserve per-source dead filtering before union
and PostgreSQL visibility/recheck rules. A neutral wrapper experiment justifies
testing a lower-level seam; it does not establish that batching will win.
[Existing design and correctness constraints](../adr/0002-batched-posting-execution-before-format-migration.md).
