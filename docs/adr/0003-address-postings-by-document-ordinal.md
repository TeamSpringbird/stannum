---
status: superseded
superseded-by: 0005
---
# Address a term's documents by ordinal

> Superseded by [ADR 0005](0005-address-postings-by-ctid.md): postings move
> to heap ctids. The extension reads this format until that one is
> integrated.

A segment stores each term's documents only as ordinals into the segment's
document table, which lists the segment's tuple locations in heap order. The
table maps an ordinal to its location and back through a page table from heap
block to first ordinal and a two-byte offset per document. There is no
per-match list of tuple locations. The
[storage guide](../architecture/segmented-storage.md#reading-an-index)
describes the layout.

## Why

A count or a Boolean filter that visits every match costs time per match. On
the full Wikipedia corpus, 195 of the 302 published count queries match more
than 100,000 documents, and visiting them one by one cost 35 to 60 ns each. No
cursor or union optimization over per-match postings changes that. Ordinals
are dense, so a frequent term is a bitset, and a Boolean count is a word-wise
fold and a population count, proportional to the chunks the terms occupy. In
the replay harness the 302 queries sum to 39 ms folded, against 3,767 ms
visiting each match, with every count equal to the recorded exact count.

A term's stream is a delta list of at most 64 ordinals, or 65,536-document
chunks that are sorted arrays or bitmaps. Dead lists are ordinal streams
too. Merges combine the inputs' sorted dictionaries and streams directly
rather than reconstructing documents.

## Consequences

- A count of a Boolean term query folds the streams a chunk at a time,
  clears dead documents, reads the visibility map once after capturing its
  view, and sends only matches on pages that are not all-visible to the heap.
  If a dead list was published after the view, the count starts over, because
  VACUUM may have marked a page all-visible after removing a tuple the view
  still lists.
- Per-segment counts are summed, which relies on a location being live in one
  source only. `stannum.verify_index` reports violations.
- Pages that are not all-visible still cost a heap visit per page. An
  unvacuumed table after heavy updates is bounded by that, not by the index.
- A location is needed only for rows a query returns; see
  [ADR 0004](0004-rank-by-document-ordinal.md) for ranking.
