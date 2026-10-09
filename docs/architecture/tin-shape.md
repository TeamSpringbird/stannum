# Segments in TIN's shape: postings addressed by ctid

Stannum is moving from per-segment document ordinals (`STN3`, ADRs 0003 and
0004) to postings keyed by heap location, the shape PlanetScale's TIN uses
([ADR 0005](../adr/0005-address-postings-by-ctid.md)). This document is the
target format, `TNS1`, and how queries, merges and VACUUM use it. Phase A,
done, defines the format and its codecs in `segment/src/tinshape/`, counts
and ranked top-k over it in `engine/src/tinshape.rs`, and a converter and
replay in the bench crate, all measured offline against `STN3` on the same
data. Phase C (below, [Integration](#integration-phase-c)) makes `TNS1` the
extension's only segment format.

Stannum's earlier ctid formats (LSG1 to LSG5) stored one six-byte TID per
match and reached 246.5 GB at 150 million Stack Exchange rows. The size of
this format was therefore the first gate: at 1 million rows of the same
corpus it is 0.895 of `STN3` (below).

## What is known about TIN

Sources: PlanetScale's blog posts and docs, EXPLAIN output and sizes
recorded against hosted TIN (1.0.2 to 1.0.4), and Lead, the open part of
TIN's code (scoring only). The project's notes are in
`stannum-lab/tin-research/`; per-df probes of TIN 1.0.4's sizes are in
`stannum-lab/tin-probes/` (interim at the time of writing).

| Claim | Status |
| --- | --- |
| Postings are ctids; no per-segment document numbering, no ordinal-to-ctid map | Stated |
| Two levels of bitmaps: 256 bits for the heap pages of a 256-page group, then up to 291 bits per page (`MaxHeapTuplesPerPage`); a term occurring once is stored inline | Stated |
| About 1 bit per posting for frequent terms, 7 for medium, 25 for rare; 2.9 bits per posting measured for a term in 80% of 1M synthetic documents | Stated / observed |
| EXPLAIN page-touch areas: Metadata, Term Map, Postings Footer, Postings Payload, Postings TF Tail, DL Sidecar, Positions, Liveness Bitmap | Observed |
| A single-term count over 100K matches reads no payload (9 pages: metadata, term map, one footer page, liveness), so per-term counts live in the footer | Observed; the counts' location is inferred |
| A ranked `history` touched 78 footer pages against 10 payload and 14 TF pages: fine-grained per-block score bounds read before the payload | Observed; the grain is inferred |
| TF buckets are 4 bits with the production boundaries; lengths are exact; BM25 is bit-identical to Lead's (and Stannum's) | Stated (Lead) / observed |
| A per-segment liveness bitmap, one bit per ctid laid out like the postings; VACUUM clears bits; only page groups with a cleared bit are ANDed | Stated |
| A disjunction count with disjoint page masks is the sum of stored counts | Stated |
| Background workers seal, promote and merge segments; merges move bitmap containers without renumbering | Stated |
| Index size: 50.7 GB at 150M Stack Exchange rows (`STN3`: 47 GB); about 8.3 bytes per (term, document) pair and 3.4 per token, positions included, on Wikipedia 100K | Stated / observed |
| Serialized byte layout, container thresholds, the position codec, the block grain, the WAND variant | Not published: everything below is our design |

The interim TIN probes (`tin-probes/analysis_e1.md`) measure TIN's marginal
index bytes per posting, positions and frequencies included, for synthetic
terms of one occurrence: about 0.14 B at 100% df, 1.3 B at 10%, 2.8 B at
1%, 4.5 B at 0.1% and 7.4 B at 0.001%, and sizes that grow with how widely a
term's rows spread over heap pages.

## The format

```text
blob := "TNS1", documents, total_length, block_size, flags (bit 0: adaptive TF),
        dictionary_len, postings_len, positions_len, docset_len, lengths_len,
        liveness_len (varints),
        term map | postings | positions | document set | DL sidecar | liveness
```

### The ctid grid

Heap blocks are taken in groups of 256, as TIN's page masks are. A group
the segment holds documents in has a width `w`, the largest line pointer
offset among them, and `256 * w` **slots**: tuple `(block, offset)` is slot
`(block % 256) * w + offset - 1` of group `block / 256`, and the groups'
slots are numbered in a row. A slot names a ctid with one table of groups
and a multiply, slots sort as ctids, and every term's members in a group
form a bitmap of the same shape, so two terms combine word by word.

A slot per possible offset up to 291 would cost a bit per line pointer of
every page; the group width cuts that to what the group uses. On the
Stack Exchange heap (20.5 rows per page on average, 4 to 70) a group has
1.61 slots per document; groups of 16, 64 or 1,024 pages would have 1.29,
1.43 or 1.91. The exact per-page widths would give one slot per document,
which is the ordinal numbering again, with a page table to reach the ctid.

### Document set and rank directory

The segment's documents in slot space: per group an 11-byte directory entry
(group, width, documents before it, kind) and either the group's slots as a
bitmap or, when every page holds offsets `1..n`, a byte of `n` per page
(0.06 MB per million documents here). A reader decodes it once into a
bitmap with the rank of each word's first slot, so a document's **rank**
(its position in ctid order) is a popcount away. Ranks address the DL
sidecar and the liveness bitmap; nothing addresses a posting by rank.

### Term map

`STN3`'s dictionary unchanged: prefix-compressed blocks of 64 terms with a
block index, each entry holding `df`, the largest bucket and the extents of
the term's postings record and positions stream.

### Postings record

```text
record  := slot varint                          when df = 1 (the bucket is the
                                                 dictionary's largest bucket)
         | form u8, footer_len, payload_len, footer, payload, tf
payload := sparse:  Elias-Fano over the term's slots, universe = all slots
         | grouped: groups, (index gap, (count - 1) << 2 | kind, [len])*,
                    one container per group the term occupies
container := grid:  the group's slots as a bitmap, 32 * w bytes
           | ef:    Elias-Fano over the group's local slots
           | paged: a page mask (or page list) and per page a bitmap, a
                    packed sorted list of offsets or a run, the smallest
```

The encoder takes the smallest form, with one exception: a group holding
at least one slot in 64 is a grid whatever else is smaller, and a term
with such a group is grouped. Grids are what counts fold and what a ranked
sieve reads word by word; Elias-Fano lists must be decoded a member at a
time (about 2 ns each), and the threshold trades size for that (see
[measurements](#grid-density-size-against-count-speed)). Elias-Fano costs
about `2 + log2(slots / members)` bits per posting, so a rare term costs
what its sparsity costs and no more; a group directory costs only terms
that span many groups. The paged container is TIN's two-level bitmap
proper; on this corpus it is almost never the smallest (402 of 331,137
group containers), because pages hold few rows and a term rarely has two
on one page.

The group directory holds each group's posting count and where its
container starts: a single-term count is `df`, and an OR of terms in a
group only one of them holds is that term's count, without the payload.

### Footer

One entry per block of `block_size` postings (256 by default) in posting
order: the gap from the previous block's last slot to this one's, and the
block's **impact frontier**, the (bucket, shortest length) pairs no other
posting of the block dominates (1 to 16, usually 1 to 3). The block's best
score under any `k1` and `b` is the score of one of them, as Lucene's
competitive impacts are; a reader computes the bound for the query's
parameters (`bound_through`, so rounding never puts a bound below a score
it covers). The frontier's first pair is the block's shortest document,
which bounds a candidate by its bucket before its length is read.

### TF tail

Each block's buckets packed at the block's width: 0 bits when its largest
bucket is 0 (every posting has tf 1), 1, 2, else 4. Posting `i`'s bucket is
in block `i / block_size` at bit `(i % block_size) * width`; block offsets
follow from the footer. 2.3 bits per posting here against 4 at a fixed
width (13.1 MB against 22.8 MB).

### Positions

`STN3`'s positions stream, byte for byte: per term, entries in posting
order with a skip table every 32. A posting's index (its rank in slot
order, which is ctid order, which was `STN3`'s ordinal order) addresses it.

### DL sidecar

Exact lengths by document rank, `u16` each, `0xffff` escaping to a sorted
table of `(rank, length)`: 2 bytes per document against `STN3`'s 4-byte
lengths plus 1-byte classes.

### Liveness bitmap

A count of dead documents and, when any, a bit per document rank. A reader
decodes it once per published bitmap into slot space, a bitmap per group
holding a dead document and nothing for the others, so a fold clears a
group's dead documents word by word and a group without any costs nothing.

## Reading it

**Counts.** A term alone is `df` when nothing is dead. Otherwise the query
is folded a group at a time over the groups that can match (the
intersection of an AND's children's groups, the union of an OR's):

- AND of terms: grids are ANDed word by word (two grids: one fused
  AND-popcount, NEON on Apple silicon); otherwise the rarest term's members
  are listed and probed in the others, a bit test in a grid or a merge with
  a decoded list.
- OR of terms: a group one term holds adds its stored count; otherwise
  grids are ORed in place and lists set their bits.
- AND NOT: the negated term's grid is cleared word by word, a list bit by
  bit. A bare NOT starts from the document set.
- Phrases and spans: the AND of their words, then positions for each
  survivor through the posting indexes, checked by Boldi-Vigna as today.
- Dead documents: the group's dead slots cleared, then a popcount.

Visibility (phase B): the visibility map is a bit per heap page; a group's
256 bits expand to slots by repeating each page's bit `w` times, so pages
not all-visible come out of the fold as a mask, as in TIN.

**Filtered by external TIDs** (phase B, a btree's result as a TID bitmap):
each TID's slot is a multiply, so the filter becomes per-group slot words
ANDed into the fold, or probes into the terms' containers when it is small.

**Ranked top k.** Exact BM25 from the TF tail and the DL sidecar, summed in
the scorer's term order, ties broken by ctid, dense terms elided as today:
the answers are bit-identical to scoring every match.

- A conjunction or phrase is led by its rarest required term and probes
  the others rarest first. A window runs to the first end of a footer block
  among the scoring terms; one whose block bounds sum to at most the
  threshold is skipped unread. Positions are read only for a candidate
  that would enter the top k.
- A disjunction is block-max MaxScore sieved a group at a time: a group
  whose terms' block bounds cannot reach the threshold is skipped; else
  each term's members there become a bitmap (a grid is read as is), the
  essential terms (whose bounds, with every lower one's, reach the
  threshold) propose candidates 64 slots at a time, and a candidate is
  bounded by the bucket of each term it holds at its block's shortest
  document before its length is read. A posting's index is its group's
  first plus a popcount.
- Candidates come in ctid order, so one that only ties the threshold ranks
  after the k-th row and is skipped like a lower one.

## Writing and maintaining it

**Building and the write segment.** A build or a promotion produces `TNS1`
from documents (`SegmentBuilder::finish_tns`): sort the segment's ctids,
derive the grid, encode each term's slots. The write buffer of forward
records is TIN's mutable write segment: sealed in place when full, then
promoted into a `TNS1` segment (see [segmented
storage](segmented-storage.md#writing-an-index)).

**Merges.** Nothing is renumbered. A merge's grid is the union of its
inputs' groups, each group's width the largest of the inputs'. Containers
hold slots local to their group, so a group only one input holds keeps its
width and its containers are copied as they are, counts included; a group
two inputs share (rows updated into pages another segment covers) is
re-encoded at the larger width, which moves each page's offsets. A term's
footer blocks and TF tail are rebuilt, since its posting indexes change
when two inputs' postings interleave, and its positions streams are
interleaved in slot order with their skip tables rebuilt; entries are
copied, not re-encoded.

**VACUUM.** A VACUUM publishes the segment's liveness bitmap (a bit per
document rank, at most an eighth of a byte per document) and readers
decode it into slot space once. A segment past the dead threshold is
rewritten by a merge with itself, dropping dead documents; groups that end
up empty disappear, the rest may narrow.

**Statistics.** `df`, document counts and total length are unchanged from
`STN3`, so scores are identical; dead documents leave the statistics only
when their segment is rewritten.

## Measurements (phase A)

1M Stack Exchange rows, one segment, the 3,762-query trace (1,254 each of
conjunction, disjunction and phrase); Apple M4 Max, one thread, the median
of 5 passes per query, warm. Outputs: `stannum-lab/tinshape/se1m/`
(`sizes-default.md` and `.json`, `final.txt`, `per-query-default.tsv`).

```sh
cargo run -p bench --release --bin tinshape -- --dump DIR --out OUT \
    --trace trace.tsv --expect pg.tsv --ranked --repeat 5
```

The converter checks every posting's ctid, bucket and positions and every
length against the `STN3` blob (45,583,943 postings), and the replay
matches PostgreSQL's answers exactly: all 3,762 counts and all 3,762
ranked top-10 lists, ids and score bits.

### Size

| area | STN3 (MB) | TNS1 (MB) |
| --- | ---: | ---: |
| term map | 7.00 | 6.99 |
| postings (STN3: ordinal streams with bounds and bucket nibbles) | 127.52 | 104.05 |
| of which ctid sets / footers and directories / TF tail | | 87.10 / 3.85 / 13.11 |
| positions | 131.57 | 131.57 |
| document table / document set | 2.39 | 0.06 |
| lengths (STN3 with length classes) | 5.00 | 2.00 |
| total | 273.47 | 244.68 (0.895) |

5.37 bytes per (term, document) pair and 3.45 per token with positions
(`STN3` 6.00 and 3.86; TIN about 8.3 and 3.4 on Wikipedia). At 150M rows:
0.895 of `STN3`'s measured 47 GB is **42 GB**, against TIN's 50.7 GB.
Bits per posting of the ctid sets alone, by document frequency: 24 at df 1,
23 at 2-7, 18 at 8-63, 15 at 64-511, 13 at 512-4K, 21 at 4K-32K and 17 at
32K-256K (where grids are forced; 8.7 and 5.8 without), 3.8 above 256K;
2.2 bits for the densest term (73% of documents), against TIN's 2.9 at 80%.

### Grid density: size against count speed

| groups kept as grids | total (MB) | of STN3 | count p50, AND / OR (µs) |
| --- | ---: | ---: | ---: |
| none (smallest form always) | 203.6 | 0.745 | 129 / 1,432 |
| at least 1 slot in 32 | 220.2 | 0.805 | 59 / 99 |
| at least 1 slot in 48 | 232.3 | 0.850 | 48 / 76 |
| at least 1 slot in 64 (default) | 244.7 | 0.895 | 41 / 65 |
| STN3 | 273.5 | 1 | 70 / 44 |

### Latency against STN3 (µs, p50 / p99)

| style | count STN3 | count TNS1 | ranked STN3 | ranked TNS1 |
| --- | ---: | ---: | ---: | ---: |
| conjunction | 70 / 215 | 41 / 131 | 45 / 438 | 37 / 818 |
| disjunction | 44 / 130 | 65 / 150 | 428 / 1,516 | 558 / 3,296 |
| phrase | 152 / 30,124 | 62 / 19,798 | 41 / 1,375 | 38 / 4,153 |

Pages per query by TIN's areas (8 KiB pages over the blob; every read
counted, the dictionary index and document set held in memory as
metadata):

| | Footer | Payload | TF tail | DL sidecar | Positions | total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| count, conjunction | 11.6 | 24.6 | | | | 39.2 |
| count, disjunction | 11.6 | 118.0 | | | | 132.6 |
| count, phrase | 11.7 | 24.8 | | | 67.4 | 106.9 |
| ranked, conjunction | 11.6 | 22.3 | 1.6 | 25.9 | | 63.4 |
| ranked, disjunction | 11.6 | 36.1 | 7.5 | 161.3 | | 218.5 |
| ranked, phrase | 11.7 | 22.8 | 1.6 | 27.5 | 53.8 | 119.5 |

`STN3`'s replay counts pages its per-backend caches do not already hold;
from cold caches it touches 192, 209 and 327 distinct pages per count and
135, 309 and 167 per ranked query, of which 76 to 90 are its dictionary
index and page table.

### Footer block size

| block | footer (MB) | total (MB) | ranked p50, AND / OR / phrase | ranked p99, AND / OR / phrase |
| ---: | ---: | ---: | ---: | ---: |
| 32 | 14.27 | 251.8 | 83 / 576 / 84 | 745 / 3,402 / 3,966 |
| 64 | 8.69 | 247.5 | 62 / 581 / 63 | 730 / 3,398 / 4,051 |
| 128 | 5.54 | 245.4 | 48 / 564 / 48 | 782 / 3,292 / 4,276 |
| 256 | 3.85 | 244.7 | 38 / 565 / 40 | 878 / 3,448 / 4,066 |
| 512 | 2.96 | 244.7 | 34 / 571 / 35 | 900 / 3,363 / 4,079 |

Small blocks prune more windows but cost a window per block; the
disjunction sieve works per group and hardly depends on the block. 256 is
the default: conjunction and phrase medians at or below `STN3`'s, a footer
of 3.9 MB.

## Integration (phase C)

The extension writes and reads only `TNS1`; the page layout went to version
3 and an older index must be rebuilt. What the segment crate gained for it,
beside the phase A codecs, all in new files under `segment/src/tinshape/` so
that work on the encoder and the walk (phase B) does not collide with it:

- `index::Reader<S: Source>`: a segment read a range at a time from the
  extension's run pages, implementing `segment::index::Index`. A term is
  handed out as an ordinal stream (ranks through the document set, buckets
  from the TF tail, lengths from the DL sidecar), translated once per reader
  and kept; positions need no translation. Every query shape the planner,
  the Boolean cursors, the ordinal walk and the ordinal count fold handle
  works over it unchanged. Its `Header::parse` reads the blob header from a
  prefix; a change to the header must change it too.
- `merge::merge`: the inputs' live documents in ctid order, every term's
  postings re-encoded over the merged grid and its positions entries
  re-packed. A property test checks a merge is byte for byte a build of the
  live documents. Copying a group's containers when one input holds it is
  not done yet.
- `verify::verify_segment`: the checker of `stannum.verify_index()` for a
  `TNS1` blob, returning the ordinal format's `SegmentReport`.
- `SegmentBuilder::finish_tns` (in `segment.rs`) and `Payload::count`.
- In `segment.rs`: `Segment::remember`, which seeds the term memo with
  entries found elsewhere, and `Segment::assemble`, a segment from a
  document set and liveness decoded once and a term-map index (empty, for
  the extension's counts). In the engine: `count_terms_visible`, the count
  fold trusting only slots on all-visible heap pages and handing the others
  to the caller, and `fold::Visibility::group_bits`.

**Counts.** A count whose query lowers (`engine::tinshape::lower`) folds a
segment's ctid sets with `count_terms_visible`; members on pages the
visibility map does not mark all-visible go to the heap a page at a time,
and VACUUM's dead list is the fold's liveness. A backend keeps each
segment's blob as far as its counts have read it: the header, the document
set (decoded once) and the liveness area, and each query term's postings
record (and positions, for phrases) the first time it is named. At 1M rows
of the comparison kit, warm counts take what the ordinal fold over
translated streams takes (0.02 to 0.07 ms, phrases 2.4 ms); a backend's
first count of a segment pays about 1.5 ms decoding its document set.
Ranked queries, the plain index and bitmap scans and every other shape read
the translated ordinal streams; the engine's ctid-native ranked walk is not
wired in yet.

VACUUM publishes a segment's liveness as a dead list of ranks beside the
blob, which keeps an all-live liveness area of its own. Merges, promotions
and dead-fraction rewrites all go through `merge::merge` or
`finish_tns`.

Size on tiny tables: a heap under 256 pages is one partly filled group, and
the grid rule (a group holding one slot in 64 or more is a grid) makes every
mid-frequency term a bitmap over all 256 pages; a 6,000-row test table
indexes to 1.8 MB against `STN3`'s 0.97 MB. Counting the group's used pages
in the rule is the obvious fix, for the encoder's owners.

## Open for phase B

- The ranked tails: conjunction and phrase p99 are 2 to 3 times `STN3`'s,
  disjunction p50 1.3 and p99 2.2 times. `STN3` bounds a candidate by a
  one-byte length class and its sub-blocks are 1,024 documents whatever a
  term's frequency; here a mid-frequency term's 256-posting block spans
  tens of thousands of slots. Per-group bounds in the directory, or blocks
  cut at group boundaries, are the next things to try.
- OR counts are 1.5 times `STN3`'s: grids cost 1.61 slots per document,
  and Elias-Fano groups decode a member at a time.
- Positions are 54% of the blob, unchanged from `STN3`.
- Terms of 2 to 7 postings spend more on their footer and header than on
  their slots (48 bits per posting against 23); a footer is unnecessary
  below a few postings.
- The write segment, the visibility-map fold, external TID filters and
  merges are designed above but not built.
