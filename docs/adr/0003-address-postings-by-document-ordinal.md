---
status: accepted
supersedes: 0002, and the "retain physical CTIDs" encoding decision of 0001
---
# Address postings by document ordinal

Later formats carried this further: `STN3` stores a term's documents only as
ordinals, and the TID postings this record keeps for ranked, positional and
streaming scans are gone. See the
[storage guide](../architecture/segmented-storage.md#why-this-layout).

Boolean counts cost 35 to 60 ns per matching document on the full Wikipedia
corpus, and 195 of the 302 published count queries match more than 100,000
documents, so the mean query took 29 ms where TIN's published p99 is 1.6 ms.
No cursor or union optimization over TID-keyed postings changes that: the work
is per match. A term's documents as ordinals into the segment's TID-ordered
document table are dense, so a frequent term is a bitset and a Boolean count is
a word-wise fold and a population count, proportional to the chunks the terms
occupy.

`LSG4` therefore stores an ordinal stream per term (a short delta list, or
65,536-document chunks that are sorted arrays or bitmaps) and a page table from
heap block to first ordinal. Counts of Boolean term queries fold those streams
a chunk at a time, clear dead documents, read the visibility map once, and send
only matches on pages that are not all-visible to the heap. TID postings remain
for ranked, positional and streaming scans, and for the write buffer.

Replaying the 302 published queries over the real corpus, all counts equal to
the recorded exact counts: 3,767 ms summed for the scalar path, 39 ms folded.
In PostgreSQL on the clean 5M snapshot with eight clients, the per-backend
prototype reached 26,407 QPS against 473 for main.

## Consequences

- Segments grow by the ordinal area, until a later format drops TID postings
  in favor of ordinals plus the document table. Earlier formats remain
  readable and are counted the old way; REINDEX upgrades them.
- Per-segment counts are summed, which relies on a location being live in one
  source only. The index checker already reports violations.
- The visibility map is read after the view is captured, and the count
  restarts if a dead list was published in between. Counting all-visible pages
  without that check, as earlier builds did for the length of the query, could
  count a tuple VACUUM had just removed.
- Pages that are not all-visible still cost a heap visit per page. An
  unvacuumed table after heavy updates is bounded by that, not by the index.
- Ranked scans still look documents up by TID. Scoring by ordinal is the
  natural next step and needs its own measurements at full scale.
