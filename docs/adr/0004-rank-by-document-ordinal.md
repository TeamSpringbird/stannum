---
status: superseded
extends: 0003
superseded-by: 0005
---
# Rank by document ordinal

> Superseded by [ADR 0005](0005-address-postings-by-ctid.md): ranked walks
> move to ctid-addressed postings with per-block impact bounds. The
> extension ranks this way until that format is integrated.

Ranked queries walk the same ordinal streams that counts fold
([ADR 0003](0003-address-postings-by-document-ordinal.md)). Nothing a ranked
walk reads is keyed by tuple location.

## Decision

- Each chunked ordinal stream carries a score bound per 65,536-document
  chunk (the term-frequency buckets that occur with the shortest document per
  bucket) and, per 1,024-document sub-block, one past the largest bucket
  there. A stream of at most 64 ordinals carries one bound. A term's bounds
  are a few hundred entries per segment, decoded once per query.
- The walk is block-max WAND over the terms' occupied chunks and
  sub-blocks; within an admitted chunk it folds the terms' words into a
  candidate set and scores candidates in ordinal order.
- A member's term-frequency bucket is a nibble beside it in the ordinal
  stream, so scoring never opens the positions stream. A candidate's length
  is a lookup by ordinal, bounded first by a one-byte length class.
- A candidate's tuple location is resolved only when it enters the top k.
  Dead documents are tested by ordinal; snapshot visibility is tested on
  admission to the top k.

## Why

A walk over per-match postings keyed by tuple location spent most of its time
on that keying: seeks over grouped page streams, varint decoding, copies of
whole postings extents into a per-backend cache, and a bounds table with an
entry per 128 postings. On the 300 published Stack Exchange disjunctions over
15 million rows the ordinal walk returns the same rows and scores at 8.9 ms
median and 58 ms p99, against 17.3 ms and 154 ms.

## Consequences

- Ranked queries do not depend on a per-backend cache holding a term's
  documents; a walk reads chunks in place from pinned shared buffers.
- Chunk bounds are coarser than per-document bounds, so some candidates in
  an admitted chunk are scored and rejected; sub-block bounds recover most of
  that pruning.
- Builders and merges compute bounds from the buckets and lengths they
  already hold, and `stannum.verify_index` checks every bound against them.
