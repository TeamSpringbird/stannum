---
status: accepted
extends: 0002
---
# Rank by document ordinal

The pruned ranked walk still runs over TID-keyed postings. Profiled under the
published mixed workload at eight clients on 15 million rows, more than half
of a backend's samples are the costs of that keying: cursor seeks over
grouped page streams, varint decoding of sparse blocks, copies of whole
postings extents into the backend's reader cache, and decoding a bounds
table with one entry per 128 postings, which for a frequent term is a hundred
thousand entries per query per segment. Enlarging the reader cache from 256
MB to 4 GB raises throughput 28%, which measures what the copies and their
eviction cost; reading postings a window at a time did not recover it,
because the walk visits most windows of a frequent term anyway.

Counting already runs over the ordinal streams of `LSG4`: a frequent term is
a bitmap of 65,536-document chunks, fifteen times smaller than its grouped
TID postings, and a chunk of any term is combined by word-wise instructions.
Ranking moves to the same streams:

- Each chunked ordinal stream carries a term bound per chunk, in the block
  bound encoding: the buckets that occur in the chunk and the shortest
  document per bucket. A stream of at most `LIST_MAX` ordinals carries one
  bound. A term's bounds are then a few hundred entries per segment, decoded
  once per query.
- The walk is block-max WAND at chunk granularity over the terms' occupied
  chunks, then within a chunk a fold of the terms' words into a candidate
  set, scored in ordinal order. A term's contribution to a candidate needs
  its term-frequency bucket, which is the payload entry at the candidate's
  rank in the term's stream: the cumulative cardinality of the chunks before
  plus a population count within the chunk. Ranks rise with ordinals, so the
  payload cursor advances as it does today.
- Document length is by ordinal already. A candidate's TID is needed only for
  the rows returned: the page table maps an ordinal to its heap block by
  binary search and the document table yields the offset within the block.
- Dead documents are tested by ordinal through the dead ordinal cache the
  count path keeps; snapshot visibility is tested on admission as today.

Chunk bounds are coarser than block bounds, so more candidates are scored
when the threshold falls between a chunk's best and its blocks' bests. A
bound per 4,096-ordinal sub-chunk keeps the table thirty times smaller than
today's and recovers most of the pruning, and can be added within the same
layout if the measurement asks for it.

The format is `LSG5`: `LSG4` segments remain readable and rank the old way;
`REINDEX` upgrades. The TID postings stay for streaming, positional and
Boolean scans, for conjunctions and for the write buffer.

Measured on the 300 published Stack Exchange disjunctions over 15 million
rows, with the same rows and scores as the postings walk: 8.9 ms at the
median instead of 17.3 and 58 ms at the 99th percentile instead of 154; the
mixed workload at eight clients went from 347 to 436 queries a second. The
bounds add 5% to the index and 2% to the build.

## Consequences

- Ranked queries stop depending on the size of the TID postings and on the
  reader cache holding them, which is what bounded the eight-client mixed
  workload.
- The builder and the merge compute bounds per chunk from the buckets and
  lengths they already hold for block bounds.
- The verifier checks chunk bounds against the payload and lengths as it
  checks block bounds.
