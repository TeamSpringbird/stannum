---
status: accepted
supersedes: 0003, 0004
---
# Address postings by heap ctid

A segment stores each term's documents as a set of heap locations, laid out
in a grid of 256-page groups, with term frequencies in a separate TF tail,
exact lengths in a sidecar by document rank, per-block impact bounds in a
footer and liveness as a per-segment bitmap: TIN's shape. The format,
`TNS1`, is described in [the TIN-shape guide](../architecture/tin-shape.md).
It replaces addressing by document ordinal (ADRs 0003 and 0004), which the
extension keeps reading until the new format is integrated.

## Why

- Ordinals tie every posting to one segment's document table: a merge
  renumbers every input, a match needs the table to become a row, and an
  external TID set (a btree's result, the visibility map) must be mapped
  into ordinals before it can filter. A ctid slot is a multiply away from
  the row, and a group's bitmap lines up with the visibility map's pages.
- It matches TIN's structures area for area (footer, payload, TF tail, DL
  sidecar, positions, liveness), so page touches and plans compare directly.
- It is not the per-match TID list that made LSG5 246.5 GB at 150M rows.
  Rare terms are Elias-Fano lists over the grid and dense groups bitmaps;
  measured offline on 1M Stack Exchange rows the index is 0.895 of `STN3`
  (about 42 GB at 150M against TIN's 50.7 GB and `STN3`'s 47 GB), counts
  and ranked answers are exactly PostgreSQL's, and count and ranked
  latencies are within a small factor of `STN3`'s (better for conjunctions
  and phrases at the median).

## Consequences

- The grid costs about 1.6 slots per document on this heap, against one
  ordinal per document: dense terms are a little larger than `STN3`'s
  bitmaps, and a disjunction count folds more words.
- Positions keep `STN3`'s codec, addressed by posting index.
- Merges must rebuild footers, TF tails and position skip tables, but not
  renumber documents; containers of groups only one input holds are copied.
- Scores and statistics are unchanged, so answers do not change with the
  format.
