# Segments in TIN's shape: postings addressed by ctid

Stannum is moving from per-segment document ordinals (`STN3`, ADRs 0003 and
0004) to postings keyed by heap location, the shape PlanetScale's TIN uses
([ADR 0005](../adr/0005-address-postings-by-ctid.md)). This document is the
target format, `TNS1`, and how queries, merges and VACUUM use it. Phase A
defined the format and its codecs in `segment/src/tinshape/`, counts and
ranked top-k over it in `engine/src/tinshape.rs`, and a converter and
replay in the bench crate, all measured offline against `STN3` on the same
data. Phase C (below, [Integration](#integration-phase-c)) makes `TNS1` the
extension's only segment format.

Phase B (branch `tinshape/phase-b`) made ranked queries faster than
`STN3`'s, median and tail, in every query style
([measurements](#measurements-phase-b)) and tightened the format for small
tables; phase C (branch `tinshape/phase-c`) integrates it into the
extension ([entry points](#entry-points-for-the-extension)).

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
the segment holds documents in has a first page `f`, a span of `p` pages
to its last (256 but at a heap's or a segment's edges), and per page of the
span a **width** `w[i]`, the largest line pointer offset among the page's
documents (0 for a page without one). Page `i` owns the group's slots
`start[i] .. start[i] + w[i]`, `start[i]` the sum of the widths before it:
tuple `(block, offset)` is slot `start[block % 256 - f] + offset - 1` of
group `block / 256`, and the groups' slots are numbered in a row. Slots sort
as ctids, and every term's members in a group form a bitmap of the same
shape, so two terms combine word by word.

A heap only inserted into holds each page's rows at offsets `1..=n`, so a
page costs `n` slots and a group's bitmap one bit per document: TIN sizes
its page masks the same way, to each page's rows (round 2, H5). Page layout
8 and earlier gave a group one width, its largest offset over all its
pages: 1.53 slots per document at 150M rows (17.9 rows a page), 1.61 on
the Stack Exchange heap (20.5, 4 to 70). A page that lost rows to pruning
keeps its holes as empty slots, as before.

A slot's ctid is a popcount: `Geometry` keeps per group its page starts
(`starts(i)`, `p + 1` of them, the slot count last), a bitmap over its slots
marking each non-empty page's first slot with, per word, the marks before
it, and the span index of each non-empty page, so `page_of` counts the
marks up to the slot (no search, no branch) and the offset is the slot less
its page's start. Only a candidate's ctid (and a native cursor's position)
needs it, and a ranked walk computes it only for a candidate it scores or
whose bound ties the threshold: computed for every candidate examined, it
cost a conjunction 3 to 5% of its cycles at 150M rows. Word operations,
counts, containers and ranks never convert a slot: a document's rank is a
popcount of the document set's words, as before; `each_tid` steps pages
with the members of a bitmap.

### Document set and rank directory

The segment's documents in slot space: per group an 11-byte directory entry
(group, first page, span, documents before it, kind) and either, when every
page holds offsets `1..n` with `n` below 256, a byte of `n` per page (its
width; 0.06 MB per million documents here), or each page's width (two
bytes) and the group's slots as a bitmap of whole words. A reader decodes it once into a
bitmap with the rank of each word's first slot, so a document's **rank**
(its position in ctid order) is a popcount away. Ranks address the DL
sidecar and the liveness bitmap; nothing addresses a posting by rank.

### Term map

`STN3`'s dictionary unchanged: prefix-compressed blocks of 64 terms with a
block index, each entry holding `df`, the largest bucket and the extents of
the term's postings record and positions stream.

### Postings record

```text
record  := slot varint [length varint]           when df = 1 (the bucket is the
                                                 dictionary's largest bucket)
         | sparse:  form u8, [footer_len], payload_len, [lengths_len],
                    [footer], list, [lengths], tf
         | grouped: form u8, directory_len, payload_len, [lengths_len],
                    directory, container*, [lengths], tf
                    (directory_len: the bytes of the payload before its
                    containers, layout 10)
form    := 1 sparse | 2 grouped, | 0x80 when lengths, | 0x40 when no footer
           (sparse) or when its grids end in sub-range codes (grouped, a
           record holding a grid, layout 13),
           | 0x10 when the frontiers' lengths are rounded (grouped),
           | 0x20 when the directory ends in skip points (grouped, layout 11)
lengths := base varint, width u8, (length - base) packed at width, by posting
list    := Elias-Fano over the term's slots, universe = all slots
directory := groups, (index gap, (count - 1) << 2 | kind, [len],
                      group frontier)*, skip*: one entry per group the term
                      occupies, then before every 16th entry past the first a
                      skip point
skip    := u32 LE each: the entry's offset from the first entry, the group
           index after the entry before it, and the container bytes,
           postings and TF bits before it
group frontier := head u8 = (pairs - 1) << 4 | first bucket, first length
                  varint, then per pair ((length gap - 1) << 4 | bucket gap - 1)
                  varint: the group's impact frontier, its lengths rounded
                  down to six significant bits when the form says so
container := grid:  the group's slots as a bitmap of whole words, then
                    its sub-range codes (layout 13): per 1,024 slots, per
                    pair of the group frontier, u8 the least rounded length
                    code (at most 254) of the sub-range's postings above the
                    previous pair's bucket, 255 for none
           | ef:    Elias-Fano over the group's local slots (universe p * w)
           | paged: a page mask (or page list) and per page a bitmap, a
                    packed sorted list of offsets or a run, the smallest
           | array: the group's local slots as sorted little-endian u16s
                    (count * 2 bytes; layout 11)
kind    := 0 grid | 1 ef | 2 paged | 3 array
```

The encoder takes the smallest form, with one exception: a group holding
at least one slot in 64 of a term of at least 4,096 postings
(`Options::grid_min_postings`) is a grid whatever else is smaller, and a
term with such a group is grouped. A smaller term decodes whole in
microseconds, and a small table, all of whose terms are small, would pay
for grids it never folds. Grids are what counts fold and what a ranked
sieve reads word by word; Elias-Fano lists must be decoded a member at a
time (about 2 ns each), and the threshold trades size for that (see
[measurements](#grid-density-size-against-count-speed)). Elias-Fano costs
about `2 + log2(slots / members)` bits per posting, so a rare term costs
what its sparsity costs and no more; a group directory costs only terms
that span many groups. The paged container is TIN's two-level bitmap
proper; on this corpus it is almost never the smallest (402 of 331,137
group containers), because pages hold few rows and a term rarely has two
on one page.

A group the encoder would store as Elias-Fano, of at most 64 members
(`Options::array_max`) and at most 65,536 slots, is an **array** of its
local slots as `u16`s instead (page layout 11; see
[measurements](#short-lists-as-u16-arrays-at-150m-rows-branch-loopr8-page-layout-11)):
two bytes a member where Elias-Fano takes 1.2 to 1.8 at these lengths,
but nothing to decode. A walk widens an array into its list eight values
a step (NEON, AVX2), sets it in a row a value at a time, and probes one
much longer than its candidates in place by galloping. Roaring's array
container is the precedent; TIN's sparse lists measure 3 to 4 bytes a
posting (round 2, H5), so it likely stores short lists byte-wise too.

A term of at most 64 postings in a segment of at least 65,536 documents
(`Options::inline_lengths_max_df`, `inline_lengths_min_documents`)
carries its documents' lengths (`Postings::lengths`), so a rare term's top
k reads no random DL sidecar pages: TIN's probes found its rare-term top k
bound by them. Such a term of at most 8 postings, sparse and in one block,
has no footer at all (`Postings::compact`): a reader derives its last slot
from the list, its frontier from its buckets and lengths, and its TF width
from the dictionary's largest bucket.

The group directory holds each group's posting count and where its
container starts: a single-term count is `df`, and an OR of terms in a
group only one of them holds is that term's count, without the payload.

### Score metadata: per (term, group), or a footer of blocks

A grouped record's score metadata is per (term, group), as TIN keeps one
footer entry per term and aligned 256-page group (its probes, rounds 2 and
7): each directory entry ends with its group's **impact frontier**, the
(bucket, shortest length) pairs no other posting of the group dominates (1
to 16; 2.4 on average at 150M rows), and the group's buckets follow the
groups before it in the TF tail, at the width the group's largest bucket
needs. A walk reaches group `g`'s entry by its position in the directory
(skimmed when the record is parsed, its entries decoded a block of 128 at a
time as the walk reaches them: see [lazy group
directories](#lazy-group-directories-layout-89)) and reads nothing of the
groups before it but their entries; a group's bound is its frontier's best score, its floors per
bucket come from the same pairs, and a group the term lacks bounds at
zero. The frontier's lengths are rounded down to six significant bits
(`round_length`, as TIN quantizes its lengths): a bound at the rounded
length is a bound at the length, the frontier costs 3% less, and 0.1% more
candidates pass (`Options::group_frontiers`, `Exact` keeps them whole).
The best score under any `k1` and `b` is the score of one of the pairs, as
Lucene's competitive impacts are; a reader computes the bound for the
query's parameters (`bound_through`, so rounding never puts a bound below
a score it covers).

A grid group (a dense term's) refines its frontier per **sub-range** of
1,024 slots (page layout 13, `Options::sub_codes`; not TIN's measured
structure, kept for a measured gain, see [sub-range
codes](#sub-range-codes-for-grid-groups-layout-13)): its container's words
are followed by a byte per sub-range and frontier pair, the least rounded
length code of the sub-range's postings whose bucket lies above the
previous pair's (`put_sub_codes`). A posting of the sub-range is then
dominated by the pair's bucket at that length, so the walk bounds a dense
term's candidate by its sub-range rather than its group: a window ends
with the sub-range, a bucket's floor is the sub-range's (at least the
group's), the sieve takes a sub-range at a time, and a disjunction weighs
a sub-range's members by its terms' sub-range bounds. A word in most
documents of a group has one frontier there, whose short documents leave
most candidates in reach of the DL sidecar; its sub-ranges' floors are
those of a seventh of its postings each. The codes cost a byte per
sub-range and pair (about 2.4 pairs, 8 sub-ranges a group), read with the
grid and only by a ranked walk that bounds by them.

A sparse record (one Elias-Fano list over the segment: a rare term) keeps
the footer of blocks: one entry per block of `block_size` postings (256 by
default) in posting order, the gap from the previous block's last slot to
this one's and the block's impact frontier. Such a block spans many groups
of a rare term, so its bound is near the term's own; per-group entries for
sparse terms would cost about 4 bytes per posting-group (11% of the index
at 150M rows, where 17.5% of postings are in sparse records), and
computing them per query from the list, the TF tail and inline lengths
costs about 12 ns a member, more than a conjunction's lead can pay. TIN
keeps a per-(term, group) entry and a container for every posting of a
rare term (about 120 bytes per posting-group, round 7); a sparse record
here costs 3.5 bytes a posting.

### TF tail

A grouped record: each group's buckets packed at the group's width, in a
row (no padding between groups); its entry records where they start in
bits. A sparse record: each block's buckets packed at the block's width,
each block starting on a byte. A width is 0 bits when the largest bucket
is 0 (every posting has tf 1), 1, 2, else 4. 2.3 bits per posting here
against 4 at a fixed width (13.1 MB against 22.8 MB at 1M rows; packing
per group rather than per byte-aligned block saved 46 MB of 1.7 GB at
150M).

### Positions

`STN3`'s positions stream (`segment::tinshape::positions`): per term,
entries in posting order with a skip table every 32, its head
(`count << 2 | masks << 1 | subs`) flagging two tables. Subs, for a term in
at least one document in 16 (and 4,096 postings): a `u16` offset of every
8th entry from its 32-entry block's start, `0xffff` past 64 KiB. Masks,
where they save bytes: per block a `u32` naming the entries holding one
position, whose count is then left out (a byte saved on most postings, at
a bit each: 28.6 MB at 1M rows). A posting's index (its
rank in slot order, which is ctid order, which was `STN3`'s ordinal order)
addresses it. A phrase checks a common word's positions for candidates far
apart in its posting order: from a skip it decoded up to 31 entries to
reach one, from a sub at most 7 (`Positions::skip_entries`: a block's mask
read once, a run of masked entries skipped as one run of varints).
`Builder::add_term` takes the
`segment::payload` stream; `Positions::payload` gives it back.
`Positions::skip(index, at)` and `read_entry(index, at, out)` take the
entry's index (for its mask bit).

### DL sidecar

Exact lengths by document rank, bit-packed per block of 256 documents: an
8-byte header per block (where its bits start, a 24-bit base, the width)
and its lengths less the base at the width its longest needs. A length is
a header read and a bit extraction; 8.3 bits per document here (TIN's
probes: about 6 on shorter documents) against `STN3`'s 4-byte lengths plus
1-byte classes, and 2.6 times the documents per page of `u16` lengths.

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
  AND-popcount); otherwise the rarest term's members
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

**Ranked top k** (`engine/src/tinshape/rank.rs`). Exact BM25 from the TF
tail and the DL sidecar, summed in the scorer's term order, ties broken by
ctid, dense terms elided as today: the answers are bit-identical to
scoring every match. A segment keeps the records it parses between
queries (`Segment::resolve_memo`, 16 MiB). A grouped scoring term's
"blocks" are its directory entries (`rank::Bounds::Groups`): a group's
bound is computed from its entry's frontier when the walk first asks for
it and kept while the walk is in the group, a group the term lacks bounds
at zero, a candidate's window ends with its group, and its bucket lies at
its entry's TF bits. What follows about footers applies to sparse scoring
terms (`rank::Bounds::Blocks`). A walk decodes those terms' footers per
query, a block at a time as it reaches them
(`postings::LazyFooter`), and keeps nothing of them after it: a footer's
entries are variable-length and in a row, so reaching a block decodes every
entry before it, and nothing after. Its bytes are read in place a page at a
time, as far as the blocks decoded (`Bytes::page_end`). A disjunction
bounds every block of every scoring term (each group a term holds is
bounded by its blocks there), so its walk decodes each footer whole, with
every block's bound, before its first group (`Sc::decode_all`): decoding
as it went saved nothing there, and read footer pages among a group's,
where they made the group's release drop pages the next group read again
(full_score disjunctions at 150M rows: 18% fewer pins, 11% fewer
instructions in the walk). It bounds each block as it parses the block's
pairs and keeps their entries' bytes rather than the pairs
(`LazyFooter::decode_bounds`): at 150M rows a full_score disjunction
decodes about 1.1M blocks and reads the frontier of at most a sixth of
them again (a candidate's floors, all sixteen at once), so keeping eight
bytes a pair for every block cost more than reading one block's bytes
again (2.5% fewer cycles in the full_score replay).

- A query some terms of which every match holds (a conjunction, a phrase)
  walks the groups its rarest such term holds, taken from its directory. A
  group whose scoring terms' footer blocks cannot reach the threshold is
  skipped unread (the sum stops once it reaches the threshold, rarest term
  first); otherwise the required terms' members are intersected: word by
  word where every one is a grid, else the rarest term's members filtered
  by each other required term in order of `df`, each looked up only while
  candidates are left (at 150M rows nothing is left after one or two terms
  in nine groups of ten), by a grid's bits, by probing an Elias-Fano
  container of more than four times their number in place
  (`Ef::retain_members`), or by merging a shorter list. Each candidate is
  then bounded by the footer blocks it falls in (a window per run of slots
  no block boundary crosses), then by its buckets: each term's bucket from
  the TF tail, scored at the shortest length its block holds for that
  bucket or more (the frontier's pairs at or above it), so the DL sidecar
  is read only for a candidate this leaves in reach, and the score is exact
  from the buckets and the length. (A required term carrying its lengths
  inline is read first instead, with a bound by its length against the
  window's largest buckets, `TermScorer::length_bound_parts`.) At 150
  million rows the bucket bound rules out two in three of the candidates
  the length bound let through, and 11% (conjunction), 20% (disjunction)
  and 3 to 5% (phrase) of the pages a query pins; CPU is unchanged where a
  page touch is a memory read.
- Such a walk scoring a common term (in at least one document in ten of
  the segment, as `full_score` scores them) beside a rarer one treats it
  apart (`Sc::make_common`). Its footer holds a dozen blocks or more per
  group, each with a short document holding it often: bounding a group by
  them decoded and bounded every one, for a group check that skipped one
  group in eight at 150M rows. So the group check bounds a common term by
  its record (its largest bucket at the least length), the rarer terms'
  blocks summed first; a candidate's window still bounds it by its block,
  and its bucket is read last, only for a candidate the rarer terms'
  buckets leave in reach at its block's bound. Its footer is skimmed
  (`LazyFooter::skim`): a block is decoded for its last slot, TF offset,
  width and largest bucket, its entry's bytes kept for the frontier a
  window or a bucket asks for later. The answers are those of the blocks'
  bounds (a looser bound only lets more through to an exact check). At
  150M rows `full_score` conjunctions retire 14% fewer instructions in the
  server (11% less backend CPU), phrases 4.6% fewer; what is left of a
  common term's cost is mostly its footer's decode up to the candidates'
  blocks (a fifth of a `full_score` phrase's CPU in the replay), which a
  footer addressed per group, as TIN's is, would remove.
- A phrase reads positions only for a candidate that would enter the top
  k. Until the top k fill every match does, so its positions are checked
  first, in ctid order (each word's read forward from the entry read last),
  and its length is read only once they hold (as TIN reads lengths for
  phrase matches only: at 150M rows this halves a phrase's DL pages and
  saves 13% of its pins, 12% with `full_score`). After, a group's
  candidates are scored best bound first, and one that scores into the top
  k is checked at once, so a confirmed one raises the threshold over the
  rest (admission compares score and ctid, so the order is free). The
  rarest slot is read first and each pair of leaves tested as it is read
  (`PhrasePlan`); a word repeated in the phrase is read once, and a
  candidate whose bucket for it falls short of the number of times the
  phrase uses it is dropped before its length is read (a phrase's leaves
  sit at distinct positions).
- Any other query is block-max MaxScore over its scoring terms, a group at
  a time. Each term present in the group becomes a row of words (a grid
  copied, a list decoded into bits; a posting's index is a popcount over
  its row). The group is first planned at its own bounds: its required
  and essential terms' rows are read and combined into a mask (a document
  holding none of the essential terms cannot reach the threshold anywhere
  in the group), and a group whose mask is empty reads no other row. Each
  member of the mask is then weighed alone: the bounds of the terms it
  holds, summed, must reach the threshold. A word's members are first
  weighed together by the terms holding any of them, at the group's
  bounds, and a word left in reach again at its 1,024-slot sub-range's
  (taken once per sub-range, from the terms' blocks), then member by
  member. At 150 million rows a group's mask holds about 85 members in a
  third of its words, and this costs less than planning each sub-range
  as `STN3` planned a sub-block and sieving all its words bit-parallel
  (`LaneSums`), and lets fewer through.
  A candidate is then bounded by its terms' blocks (a member's block is its
  posting index over the block size), then by its buckets, a term at a
  time while it is left in reach, the term bounding it highest first (as
  TIN reads a term's TF only for a candidate that still needs it). Each
  bucket read bounds the document's length from below (the shortest its
  block holds at that bucket or above), and the longest of those floors
  bounds every term it holds, read or not (TIN's shared length): an unread
  term by its block's largest bucket there. A rare word's bucket in a long
  document so rules out the common words' blocks, bounded by their short
  documents, before their TF is read: at 150M rows, scoring every term
  (`full_score`), a candidate past its blocks reads 60% fewer buckets and
  a disjunction 38% fewer TF pages. A flat disjunction's
  candidates left in reach are scored best bound first at the group's end,
  as a phrase's are, so the best of a group raise the threshold over the
  rest before their lengths are read (on TIN's corpus, 15 ORed terms read
  the DL sidecar for 40% fewer candidates); a node's own check reads
  cursors that only move forward, so a non-flat one scores in ctid order.
- Candidates come in ctid order, so one that only ties the threshold ranks
  after the k-th row and is skipped like a lower one; every bound is
  compared with a relative margin (`1e-5`) far above `f32` rounding. A
  group's candidates scored best bound first are cut by their own ctid
  against the k-th row's, so the order within a group is free.

### Word kernels and instruction sets

The block loops (`engine/src/tinshape/kernels.rs`) have a body per
instruction set: a grid loaded, ANDed, ORed or cleared into words, words
combined and tested for any member, popcount, the fused AND-popcount, the
masked count-and-clear of index-only counts and a grid's per-word prefix
counts. The choice is made per call, never per word, and each loop runs
whole inside one function compiled for its instruction set:

- aarch64: NEON, part of the baseline; no runtime choice.
- x86-64: chosen once at runtime, as PostgreSQL's `pg_popcount` chooses
  and as PGDG's baseline packages need: AVX-512 (F, BW, VPOPCNTDQ) and
  POPCNT, else AVX2 and POPCNT, else the portable bodies. AVX-512 works a
  64-byte cache line per vector, with masked loads for tails; AVX2 two
  32-byte vectors per line, counting by nibble lookup (`vpshufb`,
  `vpsadbw`). `STANNUM_KERNELS=scalar|avx2|avx512` forces a level the CPU
  has (tests, A/B runs); others are ignored.

Elias-Fano lists are decoded a block at a time
(`engine/src/tinshape/kernels/ef.rs`, the same per-call choice): the high
parts a highs byte at a time (a table row per byte value, eight lanes
stored at once, a word's byte ranks by one multiply), then the low parts
eight at a time (eight values are `l` whole bytes: a shuffle, a shift per
lane and a mask, per width up to 25 bits), into a list or straight into a
cleared row; lists of fewer than 16 members by clamped eight-byte loads
before the level is chosen. Where the AVX-512 level also has VBMI, VBMI2
and BMI2 (Ice Lake, Sapphire Rapids, Zen 4 on; asked once), it has its own
bodies (Langdale's and Lemire's bitset decoding): a highs word's members
by one `vpcompressb` of the positions 0..63 under it, less each lane's
rank, widened sixteen at a time; sixteen low parts by one `vpermb` of 64
bytes read by a load masked at the list's end, so only widths above 25
go one at a time. Without VBMI that level runs the AVX2 bodies. The same
`vpcompressb` turns a bitmap's words into positions (`kernels::set_bits`,
a conjunction's grid lead and all-grid AND); other levels keep the
count-trailing-zeros loop. A sparse list drained a group at a time by its
cursor stays scalar on every level: drains are short (some 20 members),
and decoding them in bulk was no faster on aarch64 (spans found from the
highs first) nor on x86 (a highs word decoded whole by the VBMI body,
then the cursor resumed). Neither was an AVX-512 path for lists of fewer
than 16: 2-3x faster per member in `tnsef`, no faster per query.

Not vectorized on either architecture: the OR walk's per-word and
per-member weighing (a few terms' bounds a word; branch-free whole-group
weighing measured slower), and TF-tail and DL reads (one packed value per
candidate). Their
scattered `count_ones` and `trailing_zeros` become POPCNT and TZCNT only
in an `x86-64-v3` or `-v4` build (a baseline build runs bit tricks); that
difference, not the kernels, is what those builds measure.

x86-64 builds (set the per-target variable, not `RUSTFLAGS`, which would
replace `.cargo/config.toml`'s link flags; `x86-64-v4` lacks VPOPCNTDQ, so
its AVX-512 kernels are still chosen at runtime):

```sh
cargo pgrx install --package stannum --release ...           # runtime choice
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C target-cpu=x86-64-v3' \
  cargo pgrx install --package stannum --release ...           # or x86-64-v4
docker build --build-arg STANNUM_TARGET_CPU=x86-64-v4 -f benchmarks/Dockerfile .
```

Each level is tested against the portable bodies on random inputs
(unaligned starts, odd lengths, every density). On an x86-64 host,
`STANNUM_EXPECT_KERNELS` makes a level the CPU lacks a failure rather than
a skip:

```sh
STANNUM_EXPECT_KERNELS=avx2,avx512 cargo test -p engine --lib tinshape::kernels -- --nocapture
STANNUM_EXPECT_KERNELS=avx2,avx512 cargo test --release -p engine --lib tinshape::kernels
for l in scalar avx2 avx512; do                  # whole suites at each level
  STANNUM_KERNELS=$l STANNUM_EXPECT_KERNELS=$l cargo test -p engine -p segment; done
for cpu in x86-64-v3 x86-64-v4; do
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C target-cpu=$cpu" \
    cargo test -p engine -p segment --target-dir target/$cpu; done
```

On Apple silicon, an `x86_64-apple-darwin` build runs the scalar and AVX2
levels under Rosetta with `ROSETTA_ADVERTISE_AVX=1`; nothing local runs
AVX-512 (QEMU's TCG has none).

## Writing and maintaining it

**Building and the write segment.** A build or a promotion produces `TNS1`
from documents (`SegmentBuilder::finish_tns`): sort the segment's ctids,
derive the grid, encode each term's slots. The write buffer of forward
records is TIN's mutable write segment: sealed in place when full, then
promoted into a `TNS1` segment (see [segmented
storage](segmented-storage.md#writing-an-index)).

**Merges.** Nothing is renumbered. A merge's grid is the grid of its live
documents: each page's width the largest offset of the documents kept
there, each group's span the union of its pages. Containers
hold slots local to their group, so a group only one input holds, on the
same pages at the same widths, keeps its containers as they are, counts
included; a group two inputs share (rows updated into pages another segment
covers), or whose pages a dead document narrows, is re-encoded over the
merged widths, which moves the later pages' slots. A term's
footer blocks and TF tail are rebuilt, since its posting indexes change
when two inputs' postings interleave, and its positions streams are
interleaved in slot order with their skip tables rebuilt; entries are
copied, not re-encoded.

**Copying containers** (`Builder::add_term_reusing`,
`postings::encode_reusing`). A merge passes, per group of the output's
geometry (by index), an input's container of the term as
`postings::Reused { kind, bytes, input_df }`; the builder copies it instead
of encoding the group. The output is byte for byte a fresh build's
(`segment::tinshape::segment` tests merge two inputs sharing a group both
ways and compare) when:

1. the output group lies on the input group's pages at the same widths
   (`Geometry::same_slots`);
2. its members are exactly the input's there: no dead document of the
   input in the group, and no other input holding the group;
3. both segments are built with the same `Options`;
4. the output and input terms fall on the same side of
   `Options::grid_min_postings` where the group is dense enough to be
   forced to a grid.

The caller vouches for 1 to 3 (`kind` and `bytes` come from the input's
`Postings::container` and its `GroupEntry::kind`); the builder checks 4
and encodes the group afresh when it fails. The footer, the TF tail and
the positions are always rebuilt: they depend on posting indexes, which
interleave.

**VACUUM.** A VACUUM publishes the segment's liveness bitmap (a bit per
document rank, at most an eighth of a byte per document) and readers
decode it into slot space once. A segment past the dead threshold is
rewritten by a merge with itself, dropping dead documents; groups that end
up empty disappear, the rest may narrow.

**Statistics.** `df`, document counts and total length are unchanged from
`STN3`, so scores are identical; dead documents leave the statistics only
when their segment is rewritten.

## Entry points for the extension

Stable for phase C; every signature below is as of `tinshape/phase-b`:

- Reading: `Segment::parse(&[u8])`; `Segment::term_memo(&str)` and
  `Segment::resolve_memo`, `Segment::footer_memo` (memoized parses, keep the
  `Segment` alive across queries to benefit; ranked walks decode footers
  per query instead, `Postings::lazy_footer`); `Segment::positions`
  (a `positions::Positions` stream); `Segment::length_at(rank) -> (header,
  bits)` blob offsets; `Segment::lengths.get(rank)`.
- Queries (`engine::tinshape`): `lower(&Query, &mut names) -> Option<Node>`;
  `count(&Segment, &Node, &names, &mut impl Touch) -> u64`;
  `top_k(&Segment, &Node, &names, &[(String, TermScorer)], k, &mut impl
  Touch) -> RankedAnswer` (rows `(score, Tid)` best first, plus counters
  `scored`, `candidates`, `position_checks`, `windows`,
  `windows_pruned`); `read_positions(stream, index, &mut Vec<u32>, touch)`;
  `Touch` / `Part` / `NoTouch` for page accounting by TIN's areas.
- Building: `Builder::new(tids, lengths, Options)`, `Builder::add_term(term,
  ranks, buckets, payload_stream)`, `Builder::add_term_reusing(..., reuse)`,
  `Builder::finish(dead) -> (blob, BuildStats)`; `Options::default()`
  (block 256, grid at 1 in 64 for terms of 4,096 postings or more).

Changed in round 2 of phase B: `Positions::skip` replaces the free
`skip_entry`, `Positions::read_entry` and `skip` take the entry's index,
`Positions::payload` returns a `Result`; `Postings` has `lengths`
(`InlineLengths`) and `compact`; `Options` has `inline_lengths_max_df` and
`inline_lengths_min_documents`; `BuildStats` has `inline_lengths`.
`engine::tinshape::{lower, count, top_k, read_positions}` and the builder
calls are unchanged.

Added in phase C: `engine::tinshape::top_k_into(segment, node, names,
scorers, &mut TopRows, &mut dyn Visibility, touch)` (the walk into rows
shared with other sources, no zero fill), `for_each_match` and `tids_in`
(live matches a group at a time), `score_at` and `score_at_in` (one row's
score, with term cursors kept between rows), `TermSet::{rewind, sought}`;
`engine::walk::{NativeSegment, reads_positions, span_requires_all}` and
`Source::native`; `Segment::share_footers` and `FooterCache`;
`Segment::{docs, liveness}` are `Rc`; `Liveness::is_dead`; `lower` refuses
spans that do not need every word.

Changed for conjunction and phrase speed (branch
`tinshape/perf-conj-phrase`): `postings::Form::Grouped` holds the group
directory as `Rc<[GroupEntry]>`, shared by a parsed record's clones (it was
a `Vec`), and `engine::tinshape::TermSet` reads a group's entry from it when
asked rather than copying the directory per query; `TermSet::find` returns
the entry by value. Added: `ef::Ef::retain_members(&mut Vec<u32>)` (keep
the values a list holds, its highs passed a word at a time and only the
lows of the values' buckets read) and `ef::EfCursor::list`;
`positions::Positions::skip_entries(from, at, to)`;
`boldi_vigna::PhrasePlan::leaves`.

Added for Elias-Fano decoding (branch `tinshape/ef-simd`):
`ef::Ef::{lows, highs, low_width}`; `engine::tinshape::kernels::{ef_decode,
ef_into_row}` (and `_at` per level), `container_locals` and
`container_row` (a group container's members appended to a list, or set
in a cleared row: the walks' container decodes now go through them).
Callers of `postings::for_each_local` for a walk's lists would rather use
`container_locals`.

Added for conjunction overhead (branch `tinshape/conj-overhead`):
`bits::intersect_marked(&mut list, &other, &mut marks)`, the members of
`list` in `other` through a bitmap the caller keeps clear (`other` marked,
`list` kept by its marks, the marks cleared); the led walk's group
intersection uses it where it used `bits::intersect_sorted`, which the
counts keep.

Changed in page layout 9 (per-page widths, branch `fable/pgrid`): `Group`
has no `width`; a group's page widths are `Geometry::starts(index)` (each
page's first local slot, then the slot count), `Geometry::width(index,
page)`, `local(index, page, offset)` and `page_of(index, local)`;
`Geometry::push_group` builds a grid from widths; `same_slots` compares two
groups' slots. `postings::for_each_local` and `or_into` and
`kernels::container_locals` and `container_row` take the group's page
starts instead of `&Group` (a paged container needs them; the slot count is
their last). `tnsreencode` re-encodes a version-8 (or 6) dump to 9.

Changed in phase B (callers of phase A's API): `Group` has `first` and
`pages` (`slots()` is `pages * width`, `grid_bytes()` new);
`postings::for_each_local` and `or_into` take `&Group` instead of a width;
`Segment::length_at` returns two offsets; `Lengths` has `headers_at`,
`data_at` and `at(rank)` instead of `stored_at`; positions streams are
`positions::Positions` (read them with it or `engine::tinshape::read_positions`,
not `segment::payload`); `Options` has `grid_min_postings`;
`RankedAnswer` has `candidates` and `position_checks`. Blobs of phase A do
not parse (no compatibility is kept).

## Measurements (phase B)

1M Stack Exchange rows, one segment, the 3,762-query trace; Apple M4 Max,
one thread, warm, the median of 5 passes per query; latency as the median
of 3 interleaved `STN3` / `TNS1` runs with the same build. Every run
matches PostgreSQL exactly (3,762 ranked top-10 lists, ids and score
bits, and 3,762 counts); the converter verifies every posting, bucket,
position and length against `STN3`. Experiment log and outputs:
`stannum-lab/tinshape/phase-b-log.md`, `stannum-lab/tinshape/phase-b/`.

| ranked, µs | STN3 p50 / p99 | TNS1 phase A | TNS1 phase B |
| --- | ---: | ---: | ---: |
| conjunction | 34.8 / 375 | 36.0 / 790 | **22.1 / 229** |
| disjunction | 368 / 1,355 | 547 / 3,228 | **254 / 874** |
| phrase | 32.9 / 1,255 | 38.7 / 4,166 | **23.0 / 785** |

(Round 2's interleaved runs; round 1 measured 21.9 / 216, 244 / 874 and
23.0 / 745 against 37.0 / 392, 379 / 1,381 and 34.5 / 1,244.)

Single-term queries of rare terms (983 terms, df 2 to 4,096; answers equal
to `STN3`'s): p50 0.6 / 1.6 / 7.3 / 25 µs for df 2–8 / 9–64 / 65–512 /
513–4K against `STN3`'s 5.3 / 16.5 / 27 / 57; DL pages 0 / 0 / 61 / 94
(inline lengths; 4.4 / 18 without).

| ranked, pages per query (footer / payload / TF / DL / positions) | phase A | phase B |
| --- | --- | --- |
| conjunction | 11.6 / 22.3 / 1.6 / 25.9 / 0 | 11.2 / 23.0 / 1.6 / 17.3 / 0 |
| disjunction | 11.6 / 36.1 / 7.5 / 161.3 / 0 | 11.3 / 36.3 / 7.4 / 102.2 / 0 |
| phrase | 11.7 / 22.8 / 1.6 / 27.5 / 53.8 | 11.3 / 23.6 / 0.4 / 18.5 / 18.1 |

Candidates examined (scored exactly) over the trace: conjunction 624K
(530K), disjunction 3.69M (2.96M; phase A proposed 29.8M), phrase 1.05M
(619K, 409K position checks). `STN3` examines 965K / 5.5M / 1.14M.

Size against `STN3` (the first N rows of the 1M dump rebuilt by `STN3`'s
builder, `tinshape --subset N [--rows-per-page R]`): 6,000 rows 0.633
(0.621 packed 250 rows to a page), 100k rows 0.716, 1M rows 0.802
(219.3 MB, 4.81 bytes per term-document pair against TIN's 4.7: positions
106.3, postings 104.8 with 2.5 of inline lengths, term map 7.0, DL 1.04;
phase A 0.895).

Counts (offline, µs p50 / p99, `STN3` then `TNS1`): AND 52 / 159 and
34 / 108; OR 32 / 97 and 57 / 132; phrase 121 / 26,873 and 52 / 9,449.
OR counts remain slower: a union decodes each Elias-Fano group container a
member at a time into the group's words.

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

The extension writes and reads only `TNS1`; the page layout is version 6
(phase B's positions masks, inline lengths and footer-less records) and an
older index must be rebuilt. What the segment crate gained for it, beside
the phase A and B codecs, under `segment/src/tinshape/`:

- `index::Reader<S: Source>`: a segment read a range at a time from the
  extension's run pages, implementing `segment::index::Index` for the term
  map (statistics, expansions) and, for what the native paths do not
  handle, ordinal streams. A term's postings are translated into an
  ordinal stream only when a cursor first reads them
  (`AreaFetch::ordinals_extent`), so a lookup for its statistics costs
  nothing more. Its `Header::parse` reads the blob header from a prefix; a
  change to the header must change it too.
- `merge::merge`: the inputs' live documents in ctid order. A group of the
  merged grid only one input holds, with that input's width, first page and
  span and no dead document of it there, keeps each term's container as
  the input wrote it (`Builder::add_term_reusing`); every other group is
  re-encoded. Footers, TF tails and positions skip tables are rebuilt. A
  property test checks a merge is byte for byte a build of the live
  documents, with inputs interleaved by ctid and in runs of ctids (where
  most groups are copied).
- `verify::verify_segment`: the checker of `stannum.verify_index()`.
- `SegmentBuilder::finish_tns` (in `segment.rs`) and `Payload::count`.
- `Segment::assemble` (a segment over a `blob::LazyBlob` loading what queries read,
  from a document set and liveness decoded once and shared, `Rc`),
  `Segment::remember` (term-map entries found elsewhere) and
  `Segment::share_footers` (decoded footers kept across the segments a
  backend assembles); `Liveness::is_dead`.

**How the extension reads a segment** (`storage::with_native`). A query
reads a segment within a span of the segment's `blob::LazyBlob`
(`blob::Bytes`), in place from the index's pages in shared buffers: a range
on one page is a slice of the page, which the run source pins (through the
buffer it was last pinned in, as STN3's held pages) the first time the span
asks for it and keeps pinned until the span ends; a range across a page
boundary is copied into a stitch buffer, which the blob keeps (up to 1 MiB,
unzeroed) for its next span. The blob remembers the pages a span pinned by
page, each entry naming its span, so a range on a page the span holds is a
lookup and a slice, and closing a span forgets them without touching them.
A term's record header and group directory are read when it is resolved, a
group's container
when a walk reaches the group, a posting's TF bits, the entries of a
positions stream a phrase checks and the DL sidecar words of the documents
scored; nothing is kept but what is parsed from them. A ranked walk does
not hold what it read until the span ends: after each group it walks it
releases the pages pinned since it began opening its terms
(`LazyBlob::release_since`), but for the record ranges its terms keep
slices of (`Postings::borrowed`: a sparse list, inline lengths, held
from then on) and the 24 pages read last, from which the next group most
often reads on, once 12 more than those are pinned; a stitched read
releases at once a page it pinned for itself and copied whole (the middle
of a footer). Over the trace at 150M rows (replay) a query held 560 /
1,140 / 820 pages at once (conjunction / disjunction / phrase, mean of
the most per query; 740 / 3,230 / 1,000 by `full_score`, whose
disjunctions reached the 8,192 a span may pin), which overflowed
PostgreSQL's per-backend array of eight private pin counts into a hash
table of as many entries, so every pin and release searched it (3% of a
conjunction's backend CPU, 1.8% of a disjunction's), and its resource
owner's array of 32 likewise; now some 37 / 45 / 58 (41 / 49 / 62), at
1% / 2% / 4% more pins (1% / 9% / 7%), pages released and read again.
Holding fewer, 4 say, pinned 3 to 7 times as many pages. A walk whose
candidates are checked against a node (cursors that keep a group's grid
between seeks) holds its pages as before. The span ends when
the read returns or unwinds (an error, a cancel): it forgets the parsed
records that borrow its pages, then releases them; a backend exiting
mid-read leaves the pins to PostgreSQL, as held pages are. At 150M rows the
earlier design, which copied what queries read into 8 KiB chunks each
backend kept, overflowed every backend's share of the budget and copied
again per query what the last had, some 15,000 pages a query (2.4x fewer
queries per second than STN3); `stannum.native_in_place = off` restores it
for comparison, and `score()` of single rows, which keeps its terms between
rows, reads that way.

What a backend keeps per segment is small parsed metadata: the document set
(the paged reader's, decoded once from a transient read), the dead list
VACUUM published decoded once into the segment's liveness in slot space,
memoized term-map lookups, decoded footers, and parsed grouped records
(their group directories; ranges of the blob, no bytes). The segment
assembled over the blob is kept while the liveness stays. These, and any
chunks copied outside spans, count against `stannum.reader_cache_mb`, the
chunks at most a quarter of it (`SET client_min_messages = debug1` reports
the parts as a view is captured). Past their budgets the records and
footers used longest ago go first: a segment's memos down to half of their
16 MiB, and over the native quarter the largest segments' memos are halved
before any segment's state is dropped whole. At 150M rows a common word's
directory and footer, which most queries parse, were dropped with the rest
every few queries when the memos emptied whole. `EXPLAIN ANALYZE` reports
per kind of structure (record, footer, container, sparse list, TF tail, inline lengths,
DL sidecar, positions) the bytes copied and their pages, the pages pinned
and the bytes and reads stitched, and buffer accesses by phase.

**Which paths are native.** A query is lowered (`engine::tinshape::lower`)
after its wildcards, regexes, ranges and fuzzy terms are expanded against
the segment's term map (`tinql::runtime::plan::expand_terms`); a span is
lowered only when every word of it must occur (a phrase, `NEAR`, ordered
and unordered spans; not `NOT ENCLOSES` and the like).

- Counts: `count_terms_visible`, members on pages the visibility map does
  not mark all-visible checked against the heap a page at a time.
- Ranked scans (`ORDER BY score()` or `full_score()` with `LIMIT`, every
  style, with tiebreak keys and filtered walks):
  `engine::tinshape::top_k_into` per segment, into the scan's `TopRows`.
  The engine walks the segments natively, largest first, then the write
  buffer and sealed segments over their ordinal streams, all pruning
  against one bar. A walk keeps a row only through the scan's visibility
  check, which reads the visibility map (and the heap where a page is not
  all-visible) and, in a filtered walk, the scan's other quals; it is
  asked once per row that would be kept, so heap visibility costs a lookup
  per kept row, not per candidate. Dead documents are the segment's
  liveness, cleared in the walk before anything is read for them. Scores
  come from the scorers the binding built, so `^` boosts and span leaf
  boosts weigh as in the ordinal walk. EXPLAIN's `Pruning` reads `ctid`,
  `ordinal` or `ctid+ordinal`.
- Bitmap index scans, the custom scan's candidate stream (plain scans,
  unordered scans and the exhaustive scoring of shapes the walks do not
  prune) and the candidates of `max_score`: `engine::tinshape::for_each_match`,
  a group of live matches at a time as words over the group's slots. The
  stream keeps a segment's matches as those words (at most 1.6 bits a
  document) and walks them in ctid order.
- `score()` and `full_score()` of a row the scan did not rank:
  `engine::tinshape::score_at`, the row's slot found by a multiply, each
  scoring term's posting by a seek, in the segment whose document set holds
  the row (`storage::native_holds`), not by assembling every segment for the
  row's terms in turn.

**What remains ordinal.** The write buffer and sealed write segments are
in-memory ordinal indexes (`MutableIndex`), so the ordinal walk, planner,
fold and codecs (`segment::ordinals`, `payload`, `docs`, `forward`) stay
for them. A segment is read through the translating reader only for a
query that still does not lower there (`NOT ENCLOSES` and other spans that
do not need every word, position filters, `AT LEAST` of more than one, a
match of every document, an expansion past `max_expansion_terms`,
expansions inside a span), for a query run inside another's walk over the same segment (a
filter's subquery), and by the planner's selectivity estimate, which
reads the term map and, in a segment with dead documents, a term of at
most 1,024 postings. `STN3`'s blob reader and builder remain for the bench crate's
converter and replays; its merge and checker are gone.

VACUUM publishes a segment's liveness as a dead list of ranks beside the
blob, which keeps an all-live liveness area of its own. Merges, promotions
and dead-fraction rewrites all go through `merge::merge` or
`finish_tns`.

Ties: the native walks prune a window or candidate only when its bound is
below the bar by more than rounding can explain, so a candidate that can
only tie the bar is scored (and ranks after it, by ctid); a segment whose
best documents all tie scores them all. The ordinal walk skipped such
sub-blocks by their first ordinal's location.

## Storage-layer costs at 150M rows (branch `tinshape/perf-storage`)

Measured on the 150M dump (14 segments, 37.5 GB) with `tnsreplay` and
`tnsunits`, and in PostgreSQL on a copy of the saved 150M database (24 GB
of shared buffers, `perf` sampling in the server's PID namespace). Log and
outputs: `stannum-lab/tinshape/perf-storage/`.

- Pins: a query pins 2,600 (conjunction), 8,100 (disjunction) and 3,700
  (phrase) pages, a page once per segment it touches; `ReadRecentBuffer`
  finds 33% to 55% of them in the buffer the backend last pinned them in
  (cold backend, 300 queries in). The others go through the buffer mapping
  table, whose hash lookups were 3% of a looping conjunction's samples;
  thousands of pins held at once overflow PostgreSQL's private refcount
  array into its hash table, so every pin and release looks up a hash.
- Reads spanning two pages (a stitch, a copy and a second pin), as a query
  makes them (replay, 300 queries per style): containers 5.3% to 5.7% of
  payload reads (1,450 / 4,450 / 1,650 a query, 1.3 / 4.2 / 1.5 MB), footer
  reads 41% (whole footers, decoded per term), sparse lists 16 a query
  (0.7 MB, re-read every query: a sparse record borrows its span's bytes),
  TF 0.00%, DL 0.06%, positions 0.12%. Over every unit of the segments
  (`tnsunits`): containers 1.9% straddle, positions blocks 0.5%, TF blocks
  0.9%, DL blocks (256 documents, 277 bytes) 3.4%, footer block entries
  0.14%.
- DL reads go in rank order: of 1.5M / 10.4M / 6.6M DL reads (three
  styles), 16 / 15 / 5 went to a lower rank than the read before in the same
  segment, and consecutive reads share a data page 14 / 80 / 20 times on
  average, so one pin serves them. Liveness is decoded once per backend into
  slot-space words and reads no page.
- Stitched containers, by length (replay, 150 queries per style, every
  pass): 62% to 64% of the stitched container reads are of at most 1 KiB
  (35% to 38% of their bytes), the rest 1 to 4 KiB; none is longer. Sparse
  lists and footers stitched are mostly longer than a page (whole lists and
  footers read to be decoded), and a record's directory is read in a 16 KiB
  window once 4 KiB does not hold it.
- 64-byte lines (`tnsunits`): a run page's data starts at byte 28 of the
  page, so no unit is line-aligned by construction. Lines touched per unit
  against the fewest its length needs: containers 3.45 / 3.06 (37.7% touch
  one more), TF blocks 2.11 / 1.32 (77.7%), DL blocks 5.31 / 4.72 (56.9%),
  positions blocks 1.67 / 1.34 (33.5%), footer block entries 1.17 / 1.00
  (17.3%), directories 1.24 / 1.17 (7.1%). The group directory and the
  footer are variable-length records (varints) of structures in a row
  (AoS), decoded whole before use; the decoded footer is a structure of
  arrays (`last`, `starts`, `frontier`, `tf_at`, `widths`), but the
  frontier is (bucket, length) pairs and a block's bound is computed one
  block at a time.

Proposals (not built; layout only, the same structures):

- Pad the writer so that no unit of at most 1 KiB spans a page: about
  0.9% of the blob at 150M (containers 208 MB, positions blocks 108 MB,
  TF blocks, sparse lists and positions streams 14 MB each, DL blocks
  3 MB). It removes about 63% of the stitched container reads but only 37%
  of their bytes; padding containers of up to 4 KiB too removes all of
  them for 462 MB (1.2%; with the rest about 1.6%). The copies cost little
  (1.3 to 4.2 MB a query, well under a millisecond); the second pin is the
  larger part. Without padding, a walk could read a container across two
  pages in place, a run of words per page, which needs the walks to take a
  container as up to two slices. (The disjunction walk now copies a grid
  across pages straight into its row, unstitched: see
  [grids read in place](#grids-read-in-place-branch-tinshapegrids-inplace).) Units could also start on a 64-byte line
  of the page (page offsets that are multiples of 64: data offsets of
  36 + 64k on a page): 507 MB for containers, 730 MB for TF blocks,
  2.9 GB for positions blocks (`tnsunits`, "line pad"), which only pays
  for units of a line or more.
- Decode footers by the blocks a walk reaches instead of whole: a common
  word's footer at 150M is some 20,000 blocks per segment, decoded per
  query whenever its memo entry was dropped (5.7% of a looping
  conjunction's samples in the server, `Postings::footer`). Done on branch
  `tinshape/lazy-meta` (see [lazy footers](#lazy-footers-at-150m-rows-branch-tinshapelazy-meta)).
- `TermSet::open` cloned the term's group directory and built a cursor
  entry per group per segment per query: 9.2% of a conjunction replay's
  samples (2.4% with the built groups kept beside the parsed record, an
  experiment since undone). The conjunction and phrase branch's lazy
  directory reads (`b7776bd`) cover it.

## Per-query overhead in the server at 150M rows (branch `tinshape/perf-overhead`)

Measured in PostgreSQL on a copy of the saved 150M database (24 GB of
shared buffers, 8 CPUs), eight clients taking the benchmark driver's
shuffled (query, style) entries from one counter, backend CPU per query
read from `/proc/self/schedstat` after each query, `perf` across the eight
backends. Log and outputs: `stannum-lab/tinshape/perf-overhead/`.

- Buffer lookups. A backend found 38% / 59% / 78% (conjunction /
  disjunction / phrase) of the pages it pinned through the buffer it had
  last pinned them in: its table of 2^18 entries, direct-mapped, thrashed at
  150M rows, and most pages of a query it had never pinned (other backends
  had). The rest went through `ReadBuffer` (buffer mapping hash and
  partition lock): 4.4% of the mixed workload's samples, 7.7% of a
  conjunction's. Pin and release together, with the private refcount hash
  thousands of held pins overflow into, were about 10%. `pin_block` now
  finds the buffer through a table every backend shares (`shared_hints`, a
  named dynamic shared memory segment of 8 bytes per shared buffer, 32 MB at
  24 GB; no `shared_preload_libraries` needed), and raises the buffer's
  usage count as `ReadBuffer` would (`ReadRecentBuffer` does not, so an
  index pinned through hints would look unused to the clock sweep): 96% /
  98% / 99% of pins found their buffer, backend CPU per query -10% / -5% /
  -5%, 6% more queries per second. A hint is only a hint: the tag check in
  `ReadRecentBuffer` refuses a buffer since given to another page, and an
  error creating the segment (no room in `/dev/shm`) falls back to the
  backend's own table. `stannum.share_buffer_hints = off` uses only the
  backend's table, for comparison. Holding pins on hot pages across queries
  was not tried: the segment's pages are distinct per segment, so within a
  query nothing repeats, and across queries a pin outliving its statement
  needs a resource owner of its own and blocks buffer eviction.
- Term setup. In the driver's order a backend asks for some 7 MB of parsed
  directories and footers a query (54,000 records and footers, 1.3 GB, over
  the trace), and its memos (at most a quarter of `reader_cache_mb`, 96 MB)
  kept 3 of a query's 56 to 61 footers and 29 of its 110 records: parsing
  them again was 12% (directories) and 9% (footers) of a conjunction's
  samples, 10% and 7% of a phrase's. A memo kept by use counts instead of
  recency, with half of the reader cache, kept 8 footers and 49 records,
  for 3% less conjunction CPU and 75 MB more per backend: not kept, as TIN
  keeps nothing across executions either (round 3 of the TIN probes) and
  the gain rests on the benchmark repeating each query some 27 times. What
  was kept instead makes the parse cheaper: varints of one or two bytes
  decode without a loop, a directory longer than the first 4 KiB window is
  read in one window sized by its group count rather than re-parsed in
  windows four times longer, the directory is built in its shared slice
  (it was allocated and copied twice), and a footer's frontier is allocated
  once at its upper bound: as many records and footers parsed per query,
  backend CPU per query -9% / -2% / -7%.
- A view counted every segment's liveness bytes per query by walking its
  groups (0.3% to 0.5%); it is counted once when decoded.
- Together, against a0f29d6 (two rounds, interleaved): backend CPU per
  query 12.98 -> 10.80 ms (conjunction, -17%), 41.2 -> 37.8 ms
  (disjunction, -8%), 17.0 -> 14.3 ms (phrase, -16%); 309 -> 345 queries
  per second at eight clients; the same ranked answers to all 3,762 queries
  of the trace. What remains outside the walks: decoding footers (7% of a
  conjunction's samples, 6% of a phrase's), directories (4% and 3%), pin
  and release (6% and 3%, mostly the private refcount hash), and planning
  (2% to 3%).

## Lazy footers at 150M rows (branch `tinshape/lazy-meta`)

A ranked walk decodes its scoring terms' footers per query, a block at a
time as it reaches them (`postings::LazyFooter`), instead of whole through
`Segment::footer_memo`, which kept 3 of a query's 56 to 61 footers in the
server; TIN keeps nothing across executions either. Reaching block `b`
decodes every entry before it (they are variable-length and in a row) and
nothing after it. A decoded block keeps what `Footer` keeps (last slot,
frontier pairs, TF offset and width, structures of arrays); keeping only
where its pairs start and decoding them again for each bound cost more, as
a walk bounds most blocks it reaches (all of them in a disjunction). Blocks
whose entries lie well within the window read are decoded in one loop
without a bounds check per byte. The bytes are read in place a page at a
time as far as the blocks decoded (`Bytes::page_end`); an entry across a
page boundary is stitched alone (16 bytes, more as needed). A walk records
as read only the footer bytes it read. `score()` of single rows still uses
the memo. Log and outputs: `stannum-lab/lazy-meta/`.

Replay (`tnsreplay`, first 900 queries of the trace, the answer's pass, per
query; "whole" is what decoding the walk's footers whole decodes):

| | blocks decoded / whole | blocks bounded | footer KB parsed / whole |
| --- | ---: | ---: | ---: |
| conjunction | 26,056 / 59,673 (44%) | 13,563 | 300 / 678 |
| disjunction | 64,776 / 64,777 | 64,777 | 741 / 741 |
| phrase | 24,898 / 60,816 (41%) | 7,014 | 287 / 693 |
| conjunction, `full_score` | 303,775 / 690,688 (44%) | 145,512 | 3,903 / 8,819 |
| disjunction, `full_score` | 726,833 / 726,834 | 726,834 | 9,292 / 9,292 |
| phrase, `full_score` | 293,799 / 695,940 (42%) | 73,853 | 3,784 / 8,889 |

A conjunction or phrase walk stops reaching a term's blocks at its lead's
last group, and a group's bound sum stops at the threshold, so later terms
are asked less; of a `full_score` conjunction's 84 footers, 36 are never
read at all. A disjunction's MaxScore bounds every block of every term.

Group directories (layout 6, entries of about 3 bytes) were decoded whole
when parsed: a walk stepped through nearly every entry of its terms'
directories, and a container's offset counts from the directory's end, so no
container is found before the directory is parsed through. Since layouts
8/9 they are decoded as far as a walk reaches (see [lazy group
directories](#lazy-group-directories-layout-89)). What is skipped either way
is a record the walk never needs: a walk checks its required terms first
(`Segment::holds_memo`, a term-map lookup) and parses nothing in a segment
lacking one, 8% of a conjunction's directory entries in the replay.

### Lazy group directories (layout 8/9)

Since every directory entry carries its group's frontier (layout 8), an
entry takes about 10 bytes (3 before), and decoding a common term's
directory whole cost a cheap conjunction 1-3 ms at 150M rows: about 118,000
entries decoded per query in the server, of which the walk reached about
39,000 (198,000 and 101,000 in the replay). `postings::Directory` decoded a
block of 128 entries at a time (runs of 16 from skip points since layout 11,
[below](#skip-points-in-the-directory-layout-11)), the first time one of them is asked for,
into slots reserved when the record is parsed (a walk asks for an entry
several times per group, so one decoded is a comparison and a load away);
each entry is checked against the record as it is decoded (its group, its
count, its container within the payload, its buckets within the TF tail),
and the record's totals with the last.

Before layout 10 the containers' start followed only from the directory's
end, so the parse stepped over every entry without decoding it (a skim). An
entry is a gap, a
count and kind, a paged container's length, then the frontier's head (its
pairs), first length and a delta per further pair: its length in varints
follows from the first bytes of its second to fourth varints. A scan finds
where each varint of a chunk starts and its first byte, sixteen bytes at a
time (NEON; a byte at a time elsewhere), and each varint's step (the
varints an entry starting there takes, sixteen at a time); stepping from
entry to entry is then a load and an add. Decoding reads the same scan:
the gap, count and paged length (one or two bytes, else a varint at a time)
without a branch on their lengths, the container's length for every kind
and the one its kind names chosen without a branch (the kinds mix within a
directory), and the frontier's largest bucket from its deltas' first bytes,
eight at a time. Per entry (`tnsdirskim`, 1,515 grouped records of segment
5240): skimmed 2.7 ns, 60 instructions, 12 cycles; skimmed and decoded 8.8
ns, 280 instructions, 38 cycles; decoded a varint at a time as before 11.6
ns, 261 instructions, 50 cycles (a branch per varint length, kind and pair
count).

The memos count a kept record at what its directory holds (its bytes, and
its entries decoded so far) and count it again when it is used again and
before records are dropped.

At 150M rows (branch `loop/lazy-dir`, against `e9f4506`, which decoded
every entry; all answers the same, the ranked oracle 0 differing):

- Replay with `--forget` (every pass parses again), 400 queries per style,
  2 rounds: entries decoded per query 198,000 -> 104,000 (conjunction);
  cycles -10.2% (conjunction), -7.2% (phrase), -1.2% (disjunction), -6.6%,
  -2.3% and +3.1% under `full_score` (whose disjunctions reach every entry);
  median query -26% (conjunction), -15% (phrase); instructions +2 to +5%
  (the skim is paid for every entry).
- `vm_ab`, eight clients, 2 rounds: backend CPU per query -4.9%
  (conjunction), -3.7% (phrase), -1.8% (mixed), +0.2% (disjunction); under
  `full_score` -2.1%, -1.8%, +0.4% and +0.6%. An identical-binary arm agreed
  within 0.4%.
- One backend against the layout 6 build (`e0616a0` on its database), PGO
  images: a conjunction's mean CPU +8.4% -> -0.8%, its median query x1.36 ->
  x1.20 (+0.87 -> +0.34 ms); phrase +1.4% -> -2.5%, x1.34 -> x1.19.
  Conjunctions that took the old build under 3 ms: 1.28 / 2.37 / 1.62 ms
  (old, `e9f4506`, lazy), directory entries decoded 118,900 -> 32,600 of
  39,400 reached, records reused 29.7 -> 34.9 a query (the old build 39.2:
  a kept record now counts only what it decoded).

The skim was the floor while containers were found from the directory's
end; layout 10 removed it (below). On x86-64 the scan has an SSSE3 body.

### Directory length in the header (layout 10)

A grouped record's header gives its directory's length (the bytes of its
group count and entries, where its containers start) in the varint a
sparse record spends on its footer's length. The parse reads the directory
in one window, copies it and reads none of its entries; the last block
decoded checks that the entries end where the header says. Metadata only,
so judged by measurement: 1.36 bytes per grouped record (336,689 bytes over
246,724 records, +0.001% of the 150M-row dump). `tnsreencode --from 9
--keep-records` adds the lengths to a layout 9 dump and leaves every other
byte; each segment so made equals its own merge (a fresh encoding).

Per entry (`tnsdirskim`, segment 5240): the parse 0.57 ns, 3.7
instructions, 2.3 cycles (the skim was 2.7 ns, 60, 12); parse and decode
7.0 ns, 224, 28 (8.8, 280, 38). A disjunction bounds every group of each
term it scores, so `Sc::decode_all` decodes a scored grouped term's
directory in one pass (`Directory::decode_all`) rather than a block at a
time between the walk's reads, and `TermSet::find` scans the decoded
entries as a slice.

Replay with `--forget`, 400 queries per style, 2 rounds, against
`perf-integrate9` (lazy directories, layout 9; all answers the same):
instructions -13.1% / -10.9% / -5.3% and cycles -11.8% / -13.5% / -4.5%
(conjunction / phrase / disjunction), median query -17% / -21% / -6%; under
`full_score` cycles -7.2% / -4.7% / -1.4%. A `full_score` disjunction, which
reaches every entry, against `perf-integrate8` (eager directories): cycles
-1.3%, instructions +1.5% (the branch-free decode), where lazy directories
alone were +3.1% cycles.

On x86-64 (Sapphire Rapids, `m7i.4xlarge`; replay with `--forget`, 300
queries per style and 150 under `full_score`, 2 rounds, `perf stat` user
counters; all outputs the same), the SSSE3 scan's first measurement on
hardware: lazy directories (`perf-integrate9`) against eager ones
(`perf-integrate8`) cut branch misses 30-40% and cycles 2.5-2.8%
(conjunction, disjunction, phrase; median conjunction -21%) for 1.4-1.7%
more instructions, and cost a `full_score` disjunction +1.2% cycles. With
the header's length (layout 10) against lazy directories: cycles -10.9% /
-3.9% / -7.0%, instructions -6.4% / -3.8% / -5.6%, median query -16% / -6%
/ -7%; under `full_score` cycles -6.7% / -1.2% / -5.9%. Against eager
directories: cycles -13.1% / -6.6% / -9.6%, and a `full_score` disjunction
level (0.0%, instructions +1.9%).

CPU in the replay (2 rounds, 400 queries per style, instructions retired)
against the base without its footer memo (the replay repeats each query five
times, which the memo amortizes as the server's driver order does not):
`full_score` conjunction 956 / 935 G -> 738 / 749 G (-21%), phrase 1,045 /
1,042 G -> 835 / 837 G (-20%); `score` conjunction 400 / 388 G -> 370 /
368 G (-6%), phrase 451 / 456 G -> 435 / 434 G (-4%), disjunction 1,068 /
1,057 G -> 1,078 / 1,075 G (+1%: every block is reached, and a range of
blocks was bounded in two passes until `410cea3`, +4% before it). Against
the base with its memo, the replay's repeats favour the base: `score` +1%
to +6%, `full_score` +14% to +18%.

In the server, the OrbStack VM's instructions per query, counted from the
host (`proc_pid_rusage` of its helper; backend CPU from `schedstat` moves
some 10% between identical images), 8 clients in the driver's order, the
base twice as a control (`317a3b9`, two rounds; millions):

| | base | base again | lazy |
| --- | ---: | ---: | ---: |
| mixed | 243.4 | 243.8 | 245.5 (+0.8%) |
| conjunction | 103.0 | 102.6 | 95.4 (-7.2%) |
| disjunction | 486.0 | 485.0 | 503.0 (+3.6%) |
| phrase | 146.7 | 146.5 | 137.8 (-6.0%) |

The same at `410cea3` (one round; `score`, then `full_score`):

| | base | base again | lazy |
| --- | ---: | ---: | ---: |
| mixed | 244.4 | 242.3 | 239.2 (-1.7%; cycles -3.9%) |
| conjunction | 102.3 | 102.3 | 94.8 (-7.3%) |
| disjunction | 482.6 | 486.1 | 492.8 (+1.4%; cycles -0.9%) |
| phrase | 146.9 | 146.8 | 137.0 (-6.7%) |
| mixed, `full_score` | 747.9 | 748.4 | 701.0 (-6.3%) |
| conjunction, `full_score` | 354.3 | 352.6 | 270.0 (-23.6%) |
| disjunction, `full_score` | 1,493.8 | 1,504.2 | 1,547.4 (+3.2%) |
| phrase, `full_score` | 396.9 | 396.9 | 305.1 (-23.1%) |

A disjunction reaches and bounds every block, so it decodes as much as
before and keeps the cost of decoding as it goes.

Backend CPU per query (`schedstat`, 150 s warm and 90 s measured, two
rounds; `0d7c3b0`):

| | base A1 / A2 | lazy B1 / B2 |
| --- | --- | --- |
| conjunction, ms | 14.21 / 11.51 | 10.64 / 10.80 |
| disjunction, ms | 47.33 / 40.83 | 39.93 / 40.48 |
| phrase, ms | 20.32 / 16.23 | 15.02 / 15.35 |
| queries per second | 259 / 306 | 323 / 319 |
| conjunction, `full_score`, ms | 28.36 / 28.79 | 23.80 / 22.50 |
| disjunction, `full_score`, ms | 147.92 / 137.63 | 142.79 / 133.41 |
| phrase, `full_score`, ms | 32.40 / 33.35 | 28.64 / 26.40 |
| queries per second, `full_score` | 105 / 109 | 112 / 122 |

A1 ran beside another lane's replay (the host's load at 11 to 17). The
reader cache no longer holds footers, so it keeps more records: 41 of a
query's 70 parsed against 29 of 80. Answers: all 3,733 of the trace equal
to `pg-ranked.tsv`; `--full` answers identical to the base's on the whole
trace.

### Skip points in the directory (layout 11)

An entry's totals (its container's offset, its first posting, its buckets'
offset) count every entry before it, so a walk that asks a term for group
`g` decoded every entry before `g`'s, whether it needed them or not. A
conjunction asks its other terms only for the groups its rarest term
holds: at 150M rows (replay with `--forget`, 400 queries) it decoded
104,000 entries a query and asked for 31,000 (38%). TIN's footer entry
locates its group's container without the entries before it (probes round
7, E1).

So a grouped record's directory ends in a skip point before every 16th
entry past the first (form flag `0x20`): five little-endian `u32`s, where
decoding stands there (the entry's offset from the first entry, the group
index after the entry before it, and the container bytes, postings and TF
bits before it), 20 bytes per 16 entries. `postings::Directory` decodes a
run of 16 entries from the skip point before it the first time one of them
is asked for, and checks where the run ends against the next skip point
(the last run against the record's totals). Seeking a group
(`Directory::seek`, the walk's `TermSet::find`) scans the hint's run, then
gallops over the skip points' group indexes, decoding none of the runs it
passes. A record without the flag (layout 10) still reads, its runs decoded
in order; `tnsreencode --from 10` merges each segment alone into layout 11.
A term's choice between sparse and grouped leaves the skip points out, as it
does the frontiers.

What the skip interval buys, from the walk's own requests (a lab patch
replaying them against each interval; `--forget`, 400 conjunctions):
entries decoded per query against decoding through to each one asked,
every 4 entries 42%, 8 46%, 16 50%, 32 55%, 64 61% (asked: 38%).

A run's own costs (two skip points read, a varint scan set up, the checks)
are about what decoding its 16 entries costs, so a walk that reaches most
of a term's entries (a phrase of common words) paid more per entry than a
block of 128 did: the first cut cost phrases 1.8% more cycles for 2.9%
fewer instructions. Three changes took that back. A run's bytes are scanned
exactly (the next skip point says where it ends), not by an estimate per
entry and again for the entry the estimate cut; a seek scans a decoded
run's entries as a slice; and a walk that reaches the run after one it
decoded decodes up to eight runs in one pass, checked at their end. The
parse also copies a directory once, a page at a time
(`Bytes::copy_into`), where a directory spanning pages was stitched into
the span's buffer and then copied (2 MB a conjunction at 150M rows). A
disjunction decodes its scored terms' directories whole and reads their
entries many times a group: there a seek scans decoded runs on without the
skip points, and the entries of the runs decoded from the first on are a
comparison away, as the decoded prefix was.

At 150M rows (`dump-new` re-encoded `--from 10`: +0.30% of the index,
postings +0.70%; replay with `--forget`, 400 queries per style, 2 rounds,
M4, against `perf-integrate10`): directory entries decoded per conjunction
103,800 -> 64,900, phrase 106,100 -> 66,900; instructions -7.8% / -5.6%,
cycles -4.0% / -3.9%, median query -12% / -12%, mean -2.5% / -3.3%, p99
-1.6% / +2.6% (conjunction / phrase). A disjunction (300 queries) decodes
as many entries and costs a little more: instructions +2.1%, cycles +2.3%,
mean -0.6% (the skip points copied with the directory, the run checks).
Whole trace: every answer equal to `pg-ranked.tsv`, `--full` answers
identical to `f0962a8`'s.

On x86-64 (Sapphire Rapids, `m7i.4xlarge`; replay with `--forget`, 300
queries per style, 2 rounds, `perf stat` user counters, `1c4694b`, before
the disjunction's two changes; all outputs the same): conjunction cycles
-7.5% (-7.8 / -7.1), instructions -6.2%, median query -16%, mean -9.4%;
phrase cycles -3.6% (-1.6 / -5.5), instructions -4.2%, median -10%, mean
-4.2%. With short lists as arrays on top (branch `loop/conj-x86-r8`, both
re-encoded from the same layout 10 dump): conjunction cycles -10.9% (-11.6
/ -10.2) against the base, median -25%, mean -14%; phrase -3.8%.

The x86 profile this answered (`550cf63`, the same replay, `perf record`
with each sample's inline chain): a conjunction's 31 M cycles a query went
15.6% to filtering candidates (7.6% on one load: a grid's bit for each
candidate, a cache and TLB miss as a rule), 14.5% to Elias-Fano decoding,
14.0% to the directory (decoding and seeking), 11.1% to libc (copies of
directories and stitched containers, allocation), 7.0% to pins, 6.3% to
group bounds and about 13% to candidates; a phrase's 51 M, 11.4% each to the
directory and to filtering, 10.8% to decoding and 10.5% to pins.

### A disjunction's group bounds on x86-64 (branch `loop/c5-disj`)

The same profile of a score disjunction on layout 12 (`e7a6aee`, 300
queries, `--forget`; the replay's own mapping of answers to ids, 13% of the
recording, left out) by the stages of `Walk::or_group`: group bounds 15.7%
(a bound per present term and group), weighing words 13.9%, the terms' rows
13.6%, candidates 13.1%, weighing members 10.2%, the next group's directory
seeks 9.5%, libc 6.7%, the mask 4.9%, pins 2.9%. Under `full_score`:
candidates 25.8%, members 18.1%, group bounds 13.4%, words 10.4%.

`Sc::bound_of` was 8% of the cycles at about 156 cycles a call (64,000 a
query), but its BM25 arithmetic only 1.2%: most of it sat on the fold's
`f32::max`, which on x86-64 orders NaNs with `cmpunordss` and a blend after
`maxss`, all on the chain the fold carries from pair to pair (a bound table,
R7, had removed the division per pair for no fewer cycles). The bounds are
never NaN, so a frontier's best pair is now kept by a compare and a select
(`postings::higher`), which ignores a NaN as `f32::max` does. And a present
grouped term's group bound and container come from the directory entry the
walk's seek found (`Sc::entry_bound`, `TermSet::group_at`): `range_bound`
galloped to the same entry again (2.6% of the cycles) and `or_load`'s
`TermSet::find` sought it a third time.

At 150M rows, against `e7a6aee`, with the same answers: on Sapphire Rapids
(`perf stat` user counters, 300 queries, 2 rounds) score disjunction cycles
-3.2% to -3.8% and instructions -5.6% to -8.1% (measured over the directory
change below, with and without it); on the M4 (timed passes, 300 queries, 2
rounds) score disjunction instructions -7.1%, cycles -3.9%, median query
-6.9%, mean -3.8%; `full_score` instructions -5.0%, cycles -2.5%;
conjunctions and phrases unchanged (instructions -0.2% / -0.1%).

Not kept: reading a grouped record's directory in place rather than copying
it at the parse (2.2 MB a conjunction, 2.5 MB a disjunction). With skip
points the copies were 1-2% of a conjunction's x86 cycles, libc 5.5% of
them in all; read in place, every run's chunk, skip point and frontier went
through the blob's page lookup, and the directory's pages, pinned while the
terms opened, were released between groups and pinned again: directory pins
x10 per conjunction and x416 per `full_score` disjunction, `full_score`
cycles +4.5% on Sapphire Rapids. Holding the pages a directory read last (as
a footer holds its window) cut the re-pins to x20 but left `full_score`
+2.6%, and with a dozen windows held a walk released pages at every group
(conjunctions +18%) until a release stopped counting held pages; the M4
still ran conjunctions 2.5% more instructions and 4.3% more cycles than with
the copy. The attempt is branch `loop/c5-inplace`.

## A sieve's first pass a word at a time (branch `loop/v5-r1b`)

Under `full_score` a dense pair ("is to", "to a") leaves most of a group's
slots as candidates; `Walk::sieve_words` drops, before any is listed, those
the sieve's first term (a grid there) cuts by its bucket alone, every other
term at its bound over the group. It read each candidate member's bucket
from the TF tail (its rank in the bitmap, an unaligned load, a shift) and
looked its verdict up: some 30% of a dense pair's time.

Now the buckets of the members it needs (from the first word holding a
candidate member to the last such member) are gathered once and turned
into a pass bit per member by width (`kernels::pass_bits`): at 1 bit a
mask per value, at 2 a three-mask select and a compaction (PEXT where
fast), at 4 (two in five groups) a 16-entry table, `tbl` on aarch64 and
`pshufb` with `pmovmskb` on x86-64. A word's members' bits are then laid
onto its candidates at once (`kernels::deposit_pass`): PDEP on x86-64
with BMI2, except on AMD and Hygon CPUs before Zen 3 (family 0x19), whose
microcode takes a cycle or more per set bit of the mask; elsewhere a byte
at a time from where the bytes before it end (a prefix sum of their
counts) through a 16 x 16 nibble table, without a branch (`STANNUM_PDEP=0`
forces it). On the M4 that expansion took 3-5% fewer cycles over the dense
pairs than reading each candidate's bit by its rank.

The tail trace (walk-v9's six dense queries per style, `full_score`,
`--full` outputs identical), against the same code without it: on
Sapphire Rapids (`m7i.4xlarge`, `perf stat` user counters) cycles -5.7% /
-5.4% / -6.0% (conjunction / disjunction / phrase), mean query -23% / -19%
/ -17%, 99th percentile -34% / -29% / -23%, branch misses -16% to -20%; the
portable expansion there gets about half (mean -13% / -12% / -9%). On the
M4 cycles -12.6% / -11.3% / -9.4%, mean -12% / -11% / -9%. Over the whole
trace (`--forget`; 150 queries per style on x86-64, 400 on the M4, 2
rounds) `full_score` cycles move -0.2% / -0.8% / -0.7% on x86-64 (99th
percentile of conjunctions -12%, of phrases -26%) and -1.0% / -1.0% /
-2.9% on the M4; instructions -1.4% / -0.4% / -1.5% on x86-64 and up to
+0.7% on the M4, where the expansion costs a fixed hundred instructions a
word however few candidates it holds (sparser groups reading each
candidate's bucket alone cut that by half for the same cycles: not kept).
`score` answers too are unchanged (`--expect`: 1,228 / 1,253 / 1,252 equal,
0 different), and `--full` matches `f0962a8` byte for byte.

## Elias-Fano decoding a block at a time (branch `tinshape/ef-simd`)

A walk decoded its Elias-Fano containers a member at a time
(`Ef::for_each`: next set bit, low part from a bit stream, some 24
instructions and a branch on the data). The block kernels
(`kernels/ef.rs`, see "Word kernels and instruction sets") cut a
container's decode, measured by `bench tnsef` on two segments' containers
(instructions per member, aarch64; `for_each` -> NEON, into a list | into
a cleared row):

| members | list | row |
| --- | ---: | ---: |
| 1-7 | 101 -> 52 | 100 -> 58 |
| 8-15 | 50 -> 31 | 53 -> 40 |
| 32-63 | 35 -> 12 | 39 -> 22 |
| 64-127 | 32 -> 8.3 | 36 -> 17 |
| 128-511 | 31 -> 6.8 | 35 -> 15 |
| all, density 64 | 42 -> 18 | 46 -> 28 |
| all, density 32 | 37 -> 13 | 41 -> 23 |

Timed, 64-127 members: 3.9x (list), 1.9x (row); 128-511: 4.7x, 1.5x.
On a Sapphire Rapids devbox AVX2 is 1.7x the portable body overall, up
to 5x on dense lists.

The AVX-512 VBMI bodies, on one layout-9 segment's containers (`tnsef`,
Sapphire Rapids, ns per member into a list | into a row, AVX2 -> AVX-512):

| members | list | row |
| --- | ---: | ---: |
| 16-31 | 2.07 -> 0.86 | 2.78 -> 2.03 |
| 32-63 | 1.13 -> 0.49 | 1.66 -> 1.24 |
| 64-127 | 0.61 -> 0.33 | 1.12 -> 0.89 |
| 128-511 | 0.47 -> 0.25 | 0.95 -> 0.78 |
| sparse lists | 1.11 -> 0.94 | 2.01 -> 1.94 |
| all (a third under 16, the same body) | 2.72 -> 2.20 | 3.27 -> 2.95 |

On the 150M replay (layout 9, perf user counts less the load, one
interleaved round, answers exact: the whole trace equal to
`pg-ranked.tsv`, `--full` byte-identical to `f0962a8`'s), with the
set-bits kernel, against `fcbb53a`: cycles -2.6% conjunction, -2.3%
disjunction, -3.5% phrase under score, -3.3%, -1.8%, -3.7% under
full_score; instructions -0.7% to -2.0%; branch misses -9% to -16% (not
in score disjunctions).

Replay of the 150M trace (first 400 queries per style, 3 passes, timed
instructions | cycles against `1e01d81`; aarch64, `--pin-ns 1000`, quiet
machine): disjunction -5.7% | -3.0%, conjunction -4.9% | -2.1%; without
pin costs, phrase -5.2% instructions. Answers: the whole trace equal to
`pg-ranked.tsv`, `--full` byte-identical to `1e01d81`'s. x86-64 (Sapphire
Rapids devbox, AVX2 bodies, perf user counts, 2 rounds, answers exact):
conjunction -9.0% instructions | -5.3% cycles, disjunction -8.4% | -6.0%,
phrase -6.9% | -4.2%. In PostgreSQL (vm_ab, aarch64): instructions per
query -5% to -7% per style; its cycles were not resolved at this host's
noise.

Not kept: decoding a sparse term's per-group drains in bulk (a span found
from the highs, then the block kernels). Those drains are many and long
(258M members in 400 conjunctions, 640M in 400 disjunctions, two thirds
in drains of 16 or more), but a span's setup cost about what its decode
saved: disjunction instructions +0.8%, cycles +1.6% against the block
kernels alone. Retried on x86-64 with the AVX-512 VBMI bodies and no
setup pass (the highs word holding the cursor decoded whole, sixteen
values compared with the group's end at once, the cursor resumed at the
first not below): instructions -0.3% to -2.6%, cycles -0.7% to +8.4%
against the kernels alone, so not kept either.

Grid density 32 (`tnsregrid --grid-density 32`) still loses with the
block kernels: fewer groups force grids, so more terms are sparse lists
(2.3 to 2.9 times the drained members) and Elias-Fano containers grow.
With pin costs, density 32 against 64, both with the kernels:
disjunction instructions +13%, cycles +0.8%; conjunction +11%, +6%
(without the kernels, +4% and +6.5% cycles). On x86-64 with the kernels:
disjunction +17% instructions, +4.4% cycles; conjunction +21%, +9.8%.
Keep 64. Log and tools:
`stannum-lab/ef-simd/`.

## Grids read in place (branch `tinshape/grids-inplace`)

A disjunction copied every present term's grid into its row of
`Walk::rows` (`kernels::load`, a libc `memcpy` call on x86-64, where
`memcpy` was 11% to 14% of cycles), and a grid across two pages was
stitched first, so copied twice, into a fresh stitch block. Counted at
150M rows (replay, 200 queries per style, per query):

| | grids into rows | mask starts | stitched containers | all copies |
| --- | --- | --- | --- | --- |
| disjunction, before | 40.7K (31.2 MB) | 9.7K (8.5 MB) | 4.1K (3.9 MB) | 44.4 MB, 59K `memcpy` |
| disjunction, after | 7.8K parts across pages (3.9 MB) | 7.9K (7.1 MB) | 0.2K (21 KB) | 11.8 MB, 17K |
| disjunction `--full`, before | 127.6K (106 MB) | 10.1K (8.8 MB) | 13.5K (14.4 MB) | 130 MB, 168K |
| disjunction `--full`, after | 26.5K parts across pages (14.3 MB) | 6.0K (5.5 MB) | 0.2K (23 KB) | 20.7 MB, 36K |
| conjunction / phrase | 0.6K / 0.9K `dense_group` starts | | 1.1K / 1.3K (1.0 / 1.2 MB) | unchanged |

(A copy across pages is a `memcpy` per page's part; "all" counts other
stitches too, 0.8 MB a query.)

Now a row is a `RowAt` (a pointer and a length to the group's words,
little-endian): a grid on one page is read where it lies, and a grid
across pages, or members decoded from another container, lie in the
term's part of `rows`. Every reader (the mask, the sieve's word per term,
`row_index`'s popcounts, `or_stage`'s bit tests) loads a word the same way
from either, with no choice per read. The mask starts from the essential
terms' union ORed into it, then ANDs the required terms, so a required
row is copied only when no essential term filters. An earlier attempt
(`tinshape/perf-disj`, S2) read only the non-essential grids in place,
beside the others' rows, and measured 10% slower, most of it in the
sieve's row reads.

A conjunction's grids were already read in place (`Kind::Grid`); copying
one across pages into the term's own words instead of stitching it
copies as many bytes, and measured 1.3% more instructions on aarch64 (a
page check per grid load; a first version, sizing that buffer at every
load, 5% more cycles on x86-64), so it stays stitched.

Measured against `1e01d81` (`--expect` 0 different on the whole trace,
every style; `--full` answers byte-identical), two rounds A B / B A:

| | aarch64 (M4, timed passes): instructions, cycles | x86-64 (r7i.2xlarge, `perf stat`): instructions, cycles, mean |
| --- | --- | --- |
| disjunction | -6.3%, -6.6% (196 queries) | -6.1%, -4.9%, -4.5% (292 queries) |
| disjunction `--full` | -5.1%, -8.5% (100 queries) | -4.9%, -5.0%, -4.9% (150) |
| conjunction | 0.0%, within ±2% noise (300) | 0.0%, +0.7%, +0.9% (299) |
| phrase | 0.0%, +0.6% (300) | 0.0%, -0.2%, -0.5% (298) |

On x86-64 the disjunction's L1 data misses fell by 16% (23% with
`--full`), but the cycles saved are about the instructions saved, not
more, as if the copies' cost was mostly the first touch of the grids'
lines, which the in-place reads now pay for the words they read.

For the lanes changing the same walk (`tinshape/full-or`,
`tinshape/ef-simd`): `Walk::rows` holds little-endian words, read only
through `Row::at`; a path that fills a row sets `Row::at` (`RowAt::of`
for `rows`, `grid_row` for a grid) and writes words with `to_le`
(`RowBits` does). The kernels are the existing byte-slice ones
(`load`, `and_bytes`, `or_bytes`, `popcount_bytes`); nothing in `ef.rs`
changed.

## `full_score` disjunctions (branch `tinshape/full-or`)

A `full_score` disjunction scores the dense words `score` elides (in at
least one document in ten). At 150M rows (replay, 400 disjunctions) it
read 25,282 pages a query against `score`'s 7,895, and its profile on
aarch64 was the word loop 29%, grid copies 20% (now read in place, above),
the candidates' path 23% (11.7% reading buckets), footer decoding 12% and
range bounds 8.5%. Counted over two passes:

- Dense words already behave as MaxScore's non-essential terms: of 39.0M
  groups, 13.4M are cut before a dense row is read (7.2M by the summed
  bounds, 6.2M by an empty essential mask); 0.34M passing groups hold
  dense words alone. A dense word is essential in 4.9M of the 25.5M
  passing groups, where the threshold is still below the dense words'
  summed bounds.
- 1.32G live words: 930M cut at the group's bounds, 1.79G members of the
  rest weighed (10.4 terms each) into 177M candidates (`score`: 27M), 69M
  of them holding dense words alone. 40M fall to their blocks' bounds; the
  rest read 511M buckets (313M of dense words), 2.8M are scored.

Kept:

- A candidate's buckets are read a term at a time while it is left in
  reach, the term bounding it highest first, every held term bounded at
  the longest length floor its read buckets imply (an unread term by its
  block's largest bucket there; `LazyFooter::decode_bounds` keeps each
  block's largest bucket, a byte a block). Buckets read past the blocks'
  bounds fell from 375M to 148M (simulated: dense words first at their
  blocks' bounds would have saved 39M; plain incremental reads 81M). TF
  pages a query 6,820 to 4,257 (`score`: 601 to 565).
- No word check at the sub-range's bounds before the members are weighed
  there (it cut 13M of 383M words).

On top of `tinshape/grids-inplace` (`0bd6e15`), 400 disjunctions x 3
passes, aarch64 (M4, shared host, two rounds): `--full` instructions
-2.4% / -2.2%, cycles -5.5% / -4.9%; with `--pin-ns 1000` -3.7% and
-7.3%; `score` -1.0% / -0.9% and -1.1% / -1.6%. x86-64 (r7i.2xlarge,
`perf stat`, user, less a load-only run, one round): `--full`
instructions -3.7%, cycles -5.1%, mean latency -4.6%; `score` -1.6%,
-2.2%, -2.3%. Answers: `--expect` 0 different on the whole trace,
`--full` byte-identical.

Measured and not kept (exact, all): sub-ranges of 512 or 256 slots
(closer to a dense word's ~365-slot blocks: +1.6% / +3.8% instructions);
dense words' group bounds by their record, so their footers could be
skimmed (passing groups +8.5%, members weighed +50%; the group check
needs the blocks); blocks found by slot with posting indexes only for
buckets read (+5% cycles); the length read first for a candidate whose
best term is dense (more DL pages, +1.6% instructions); a MaxScore plan
per sub-range (members weighed -7% only); a word's members weighed term
by term, or only by the terms holding any of them (+7% / +2%); the held
terms found without a branch (+0.5% instructions, -1.3% cycles alone,
nothing on top of the above).

What is left is mostly not dense-specific: the word loop (each member of
a live word weighed by every present term), `range_bound` per term per
sub-range, and decoding every dense footer block's bound up front, which
the group check needs. For the lanes on the same walk: `or_stage` reads
`Walk::held_bound` as each held term's current bound (block, then bucket
at the shared floor) and computes a member's posting index as before
(`row_index`); `or_bucket_of` no longer writes a bound.

## A conjunction's instructions at 150M rows (branch `tinshape/conj-overhead`)

Where a ranked conjunction's CPU goes, at `1e01d81`. Log, tools and
outputs: `stannum-lab/conj-overhead/`.

**By phase**, from the process's instruction counter read at each phase's
ends in the replay (lab-only; the whole conjunction trace, 1,253 queries,
timed passes, aarch64, millions per query):

| part | instructions | share |
| --- | ---: | ---: |
| scorer build (TinQL, term lookups, BM25 constants) | 2.72 | 3.5% |
| `Scorer::top_k` outside the segments | 0.46 | 0.6% |
| per segment (14): terms opened (records, directories) | 0.44 | 0.6% |
| per segment: walk setup (footers, scorer tables) | 0.23 | 0.3% |
| per segment: the walk | 73.29 | 94.6% |
| per segment: counts and drops after it | 0.37 | 0.5% |

The replay keeps no term memo, so its scorer build walks a dictionary block
per term and segment; the server memoizes those lookups. In the server
(perf over 8 backends running conjunctions) planning is 1.7% of the
backends' cycles, the executor 1.5%, the rest of PostgreSQL 0.7%, pins and
the buffer manager 9.4%. Setup, scorer construction and term resolution
are a few percent; the walk is the rest.

**Within the walk**, each instruction counted to the innermost function of
its source line, inlined code included (callgrind on aarch64 Linux over 4
of the 14 segments, 98 queries):

| part | share |
| --- | ---: |
| members decoded (the lead's, other terms' lists) | 34.4% |
| intersection and filters (grids, merges, Elias-Fano probes) | 16.5% |
| footers decoded as the walk reaches their blocks | 11.0% |
| reads in place (page lookups, pins) | 7.6% |
| directory lookups (a term's group, the next group) | 5.3% |
| candidates: window, buckets, floors | 4.6% |
| records and directories parsed | 3.0% |
| scorer build | 1.7% |
| group bounds | 1.2% |
| the walk's loop and group releases | 1.2% |
| candidates: length, score, top k | 1.1% |
| allocation | 0.7% |
| library code inlined in the walk's functions | 11.8% |

Per query over the whole trace: 18,864 groups visited (2,703 skipped by
their bound), 9,311 candidates examined, 1,237 scored, about 3,620 pages
pinned; 4.1 K instructions a visited group, 21 K a page, 8.3 K an examined
candidate. A visited group decodes its lead's members (about 22) and the
next required term's (about 27, or probes them in place), and nine groups
in ten end empty after one or two terms. What is not decoding is spread
thin: no part outside decoding and intersecting is a tenth of the walk.

Tried and not kept (instructions of the timed passes, 300 conjunctions):
the required terms ordered per group by cost (grids, which test a bit per
candidate, before lists, which decode, would have left nothing to decode
in 6.6 of 9.0 thousand list steps a query) +14.6%, as looking every term
up in every group costs more than the decodes saved; every scoring footer
skimmed +0.4%; buckets read best bound first, stopping once the candidate
falls short with the others at their window's bounds +1.0% (at the shared
length floor, +2.2%; TF pages unchanged: candidates fall short only with
every bucket read); containers decoded in place rather than pushed +0.1%;
the sparse lead drained in chunks +1.9%; the Elias-Fano probe ratio at 8
+0.1%. The required term holding the fewest groups is the df-rarest one in
all but 2 of 13 walks a query (22.3 K groups against 21.9 K). The final
threshold known from the start (the replay's `--seed`) bounds what any
early threshold could save: 6.1% of the instructions (examined 10,209 ->
6,459, scored 1,192 -> 569, pages 3,620 -> 3,253).

**Kept: the group's intersection through a bitmap.** A list's members are
intersected with the candidates by marking the list in a bitmap over the
group's slots, keeping the candidates whose bits are set, and clearing the
marks (`bits::intersect_marked`), where `intersect_sorted` counted, per
candidate, the list's values below it a stride at a time: a chain of
dependent steps per candidate (count, then the stride's position, then
the next count), 13% of a conjunction's cycles in the replay's sampled
profile. Marking does more work but nothing waits on the step before.
In the replay (aarch64, timed passes, 300 queries, base and change
interleaved): conjunctions retire 2.1% more instructions (69.2 to 70.7 G)
in 4% to 12% fewer cycles (seven pairs, median 7%), phrases 1.3% more in
8% fewer. The same answers: the whole trace against `pg-ranked.tsv`, and
`--full` byte-identical to `1e01d81`'s. On x86-64 `intersect_sorted`'s
chain per candidate is a load, two compares, a pack, a move of the mask,
a count and an add; marking is a few independent operations per value.
Looking a member's posting index up by binary search rather than a scan
changed nothing (conjunctions -0.2% instructions, phrases +0.1%).

In the server (`vm_ab`, plain images, one round; host load near 28 on 16
cores) the VM retired 1.7% more instructions per conjunction (96.8 to
98.5 M), as in the replay, but its cycles did not resolve the change: the
identical-binary control differed from the base by 5.7% in conjunction
cycles and by 13% (conjunction) and 42% (disjunction, whose code did not
change) in backend CPU. The server comparison is to be repeated on a quiet
host.

## Per-page widths at 150M rows (branch `fable/pgrid`, page layout 9)

The 150M dump re-encoded from layout 8 (`tnsreencode --from 8`, 10 to 12
minutes at four jobs): 1.529 slots a document become 1.000 (32,696 groups,
109.6 words a group become 72.2). With `Options::grid_density` at 64 a group
is now forced into a grid at one document in 64 where it was one slot in 64
(one document in 98 at 1.53 slots a document), so 60% more groups are grids
(11.4M to 18.3M) and the postings area shrinks only 5.7% (16.59 to 15.65 GB;
the dump 37.47 to 36.53 GB, -2.5%). At density 42, about the old forced set,
grids rise 10% and their bytes fall 35% (8.20 to 5.36 GB): postings -18.8%
(-3.12 GB, the containers lane's model), the dump -8.3% (34.35 GB). Both
answer the whole trace as PostgreSQL does, and `--full` outputs are byte for
byte f0962a8's; a fresh PostgreSQL build at 9 equals the re-encoded 8 build
byte for byte.

Replays against layout 8 (`fable/footers`), score on 400 queries and
`full_score` on 200, `--repeat 3`, two rounds on the M4 (instructions agree to
0.1% between rounds; cycles moved by up to 3% on a loaded machine), one round
on a dedicated Sapphire Rapids (perf, the process less a load-only run, 300
and 150 queries):

| v9 against v8 | M4 instr / cycles (64) | M4 instr / cycles (42) | x86 instr / cycles / latency (64) | pins (64 / 42) | pages (64 / 42) |
| --- | --- | --- | --- | --- | --- |
| score conjunction | -10.8 / +1.0% | -2.9 / +7.0% | -9.0 / +4.6 / -11.1% | -16 / -23% | -14 / -20% |
| score disjunction | -10.9 / -3.1% | -4.8 / +2.8% | -9.1 / -8.4 / -9.3% | -16 / -25% | -15 / -23% |
| score phrase | -8.8 / +3.3% | -2.8 / +5.2% | -6.9 / -5.2 / -7.0% | -13 / -19% | -12 / -18% |
| full conjunction | -4.7 / +3.8% | -2.6 / +0.7% | -5.7 / -5.5 / -7.1% | -13 / -18% | -11 / -16% |
| full disjunction | -5.9 / -5.8% | -3.6 / -3.5% | -4.9 / -7.2 / -7.7% | -21 / -24% | -21 / -23% |
| full phrase | -4.2 / -3.8% | -2.4 / -3.4% | -5.4 / -6.4 / -8.4% | -10 / -15% | -10 / -14% |

Payload pages fall 18 to 20% at density 64 (29% for a `full_score`
disjunction) and 28 to 33% at 42, as modelled (-33% for a disjunction);
footer pages rise 2 to 3% (more directory entries of forced groups), TF
pages fall up to 3% and DL pages 3 to 9%. Pages per query, score, density 64
(42): conjunction 3,191 (2,965), disjunction 6,318 (5,721), phrase 4,264
(4,005), against TIN's 2,688, 3,968 and 3,584. Branch misses fall 13% on x86
in every style. A row is a third shorter, so every word loop, union, AND and
popcount does a third fewer word operations; the Elias-Fano lists a forced
grid replaces at density 42 cost more to expand than their pages save in the
replay, where a pin costs nothing; at about 1,300 cycles a pin in the
server, density 42's further pins close a fifth (disjunction) to two thirds
(phrase) of that gap. Density 64 is the default.

## The server path and concurrency at 150M rows (branch `fable/server`)

Measured in PostgreSQL on a copy of the saved 150M database (24 GB of
shared buffers, 8 vCPUs of the Mac's VM, the driver's shuffled order,
backend CPU from `/proc/self/schedstat`, `perf` across the backends, VM
instructions per query from `benchmarks/local/vm_ab.py`), beside the i7i
counters of 2026-10-12 and the r7i replay of the same commit. Log and
outputs: `stannum-lab/fable/server/`.

- Outside the walk. The server's own code is a tenth of a query's cycles
  or less: pins and the buffer manager 6.8% of the mixed workload on the
  Mac (7.5% of an x86 conjunction, 3.3% of a disjunction, plus 2-3% of
  first touches of the hint, descriptor and page-header lines), the planner
  0.9%, the executor 0.8%, the kernel 0.4%. Planning a query takes 0.04 to
  0.08 ms over 15 buffers once a connection is warm (its first query costs
  33 ms and 8,700 buffers of catalog and reader caches, once); TIN plans
  in about 0.55 ms over 39 buffers and resolves its terms there. Against
  the replay at the same commit the server retires 24% more instructions
  per conjunction, 7% per disjunction and 28% per phrase, and costs 1.5 to
  1.8 times the cycles: the walk itself runs slower inside the server
  (shared buffers a page at a time on 4 KB kernel pages, a loaded module's
  code) and, on x86, on a hyperthread.
- Concurrency. From one client to eight, CPU per query is flat (conjunction
  8.4 to 7.9 ms, disjunction 33.8 to 32.4, phrase 12.7 to 10.9 on a loaded
  host) and queries per second linear; the profiles at one and at eight
  clients are the same to a tenth of a percent per area. Nothing in the
  buffer mapping partitions, the shared hint table (read on every pin,
  written on the 4% that miss), the private reference-count hash or the
  allocator contends; the one shared line visible is the buffer header's
  spinlock in `ReadRecentBuffer` (1.2 to 1.9% on x86). On the i7i (four
  cores, eight hyperthreads, 8 clients) each hyperthread retires 1.45
  (conjunction) to 1.68 (disjunction) instructions per cycle, where the
  same code alone on an r7i core retires 2.6 and on the Mac 3.7 to 4.1: the
  walk is 44 to 51% retiring with few cache misses, so a sibling thread
  finds little idle issue bandwidth and SMT adds 10 to 20% of core
  throughput rather than the 30% or more a memory-bound workload gets.
  Fewer instructions in the walk are the lever there, not the server.
- Pins. A `ReadRecentBuffer` pin and its release cost about 1,400 cycles
  of bookkeeping on x86 (`GetPrivateRefCount` and `NewPrivateRefCountEntry`
  in the hash a walk's 40 to 60 held pins overflow into, `LockBufHdr`, the
  resource owner's remember and forget, the release's hash lookup and
  delete, and a second atomic to raise the usage count) and about 600 of
  cache misses (the hint bucket, the descriptor, the page header), 3,600 to
  7,700 times a query. `own_pin` pins a hinted buffer as `PinBuffer` does,
  one compare-and-swap of the header's state (reference and usage counts
  together) and then the tag check, and `own_unpin` releases it as
  `UnpinBuffer` does: one compare-and-swap while the header is not locked,
  waiting with PostgreSQL's spin delay while it is, and under the header
  lock the wake-up of a waiter for the sole pin. Not a plain atomic
  decrement: a holder of the header lock (the bgwriter's and a checkpoint's
  scans, the clock sweep, `pg_buffercache`) writes the state it read back
  at the unlock, so a decrement landing under the lock is undone and the
  pin outlives everyone, which keeps the buffer from eviction and makes a
  `DROP` of the relation spin in `InvalidateBuffer`
  (`postgres/tests/own_pins.py` holds the lock across a release). The
  hint's buffer number is checked against `NBuffers` before the descriptor
  is read. The span's own pins are entered in
  a ledger remembered with the statement's resource owner as one resource
  (`ResourceOwnerDesc`, released with the buffer pins), so a `FATAL` exit
  mid-walk, which unwinds nothing, releases them as PostgreSQL releases its
  own; an error that unwinds the walk releases them through the span's
  close as before. The pin counters are backend statics: a `thread_local!`
  in a loaded module costs a dynamic lookup per access, several per pin.
  Together (`vm_ab`, two rounds, an identical-binary control within 0.7%
  in instructions): instructions per query -6.1% (conjunction), -3.9%
  (disjunction), -6.2% (phrase), -4.0% (mixed); cycles -5 to -7%; backend
  CPU -4 to -7%; the same ranked answers to the whole trace.
- Not changed: batching pins of consecutive blocks (a group's pages are not
  consecutive); holding at most eight pins (fewer-pins: pins x2.8 to 7);
  pins kept across queries (eviction, a resource owner of their own, and
  TIN keeps nothing). The page-header read per pin could go (the data
  length follows from the run's geometry) for one line per pin, about 1 to
  2% of an x86 conjunction, at the price of the per-pin corruption check.
  Shared buffers sit on 4 KB kernel pages in these containers (no hugetlb
  pages): the i7i counts one dTLB miss per page pinned and 2.6 to 3.9% of
  cycles in page walks, for TIN as for us; a benchmark-setup question, not
  a change here.

## Short lists as `u16` arrays at 150M rows (branch `loop/r8`, page layout 11)

The gate, over the layout-10 dump (`tnsef census`): 70.6M Elias-Fano group
lists hold 1.01G members in 1.24 GB; the largest group has 14,110 slots, so
every group's local slots fit a `u16`. By list length, the share of their
members and what arrays would add to the 36.5 GB index:

| lists of at most | members | arrays less lists |
| --- | --- | --- |
| 8 | 12.2% | +0.10% |
| 16 | 27.1% | +0.38% |
| 32 | 60.5% | +1.15% |
| 64 | 83.6% | +1.73% |
| any | 100% | +2.16% |

Per query (a census build of the replay, 400 queries a style), a score
conjunction decodes 8,457 lists whole (5,630 of them of at most 32) and
seeks 2,431 in place; a phrase about 1.13 times as many; a disjunction sets
29,079 in rows. On the M4 (`tnsef arrays`, segment 1's lists, ns a
container, list against array), decoding then marking against candidates
of the same length takes 20 against 13 at 1 to 7 members, 34 against 23 at
8 to 15, 65 against 34 at 16 to 31, 101 against 65 at 32 to 63; a seek
with an eighth as many candidates 22 against 9, 52 against 17, 97 against
38, 161 against 74; a row 18 against 11, 26 against 15, 47 against 23, 54
against 37. An all-pairs NEON compare of blocks of eight (Schlegel et al.)
marks no faster than widening the array and marking it as a list is
marked; a branch-free merge takes twice as long. Projected: 5% of a
conjunction's cycles at 64, 2% of a disjunction's.

Arrays of at most 64 members (`Options::array_max`), the dump re-encoded
alone (`tnsreencode --from 10`, 10 minutes at three jobs; every segment
verifies and equals its own merge): 65.5M arrays (822M members, 1.64 GB),
1.63M lists left; postings +4.0%, the index +1.73% (36.51 to 37.14 GB).
Every term keeps its form: an array weighs as the list it replaces when a
term chooses between grouped and sparse (weighed by their bytes, arrays
moved 80,000 terms to sparse). Replays against layout 10 (062967b),
`--forget`, interleaved: on the M4 400 queries a style, four rounds
(score) or two (`full_score`); on a Sapphire Rapids devbox
(`devbox-arrays-ab.sh`, m7i.4xlarge, AVX-512 VBMI; perf, the process less
a load-only run) 300 and 150 queries, two rounds:

| layout 11 against 10 | M4 instructions / cycles | x86 instructions / cycles / mean latency |
| --- | --- | --- |
| score conjunction | -3.1 / -2.7% | -2.0 / -3.5 / -3.2% |
| score phrase | -2.6 / -7.9% | -1.6 / +0.8 / +1.6% |
| score disjunction | -2.0 / -5.2% | -1.5 / -1.3 / -0.1% |
| full conjunction | -1.9 / -2.2% | -1.3 / -2.0 / -1.5% |
| full phrase | -1.5 / -3.1% | -1.0 / +1.6 / +3.1% |
| full disjunction | -0.9 / -2.7% | -0.6 / -1.2 / -1.0% |

Conjunction latency on the M4 moved with the order of the pair (the second
run of each pair was the faster, by up to 12%), -2% to +2% over the
rounds. x86 branch misses fall 1 to 5%. On x86 (`tnsef arrays`, AVX-512
lists against AVX2 arrays) the gaps are as wide as on the M4 for marking
(31 against 21 ns at 1 to 7 members, 52 against 33 at 8 to 15) and wider
for seeking (39 against 13, 81 against 21), but widening an array costs
17 to 18 ns below 16 members there against 5 on the M4: the AVX2 body is
a call through the level's dispatch even for three values. Every answer
of the whole trace is PostgreSQL's (1,253, 1,228 and 1,252 queries equal),
and `--full` outputs are f0962a8's byte for byte; on x86 both layouts give
the same outputs in every style and mode.

## Open

- OR counts are about 1.8 times `STN3`'s offline. A union reads every
  term's grid in every group, and a grid is 1.61 slots per document (a
  dense term's grids are 25K words at 1M rows against `STN3`'s 16K); the
  rest is decoding Elias-Fano containers into words. Fusing the ORs with
  the count, NEON grid kernels, merging short lists instead of a bitmap,
  saturating early and a table-free Elias-Fano decode all measured no
  faster (log, round 2). Grids over document ranks rather than slots
  would remove the inflation; that is a format change.
- Positions are still 49% of the blob (2.3 bytes per posting; TIN's
  synthetic probes measured 0.14 per posting at tf 1, on documents of one
  to three words). At 150M rows (18.4 GB of positions, 2.7 bytes per
  posting, 71% of postings at tf 1) bit-packing each block's counts, first
  positions and gaps at the block's widths would save about a tenth
  (1.8 to 2.2 GB, 5 to 6% of the index) and 8 to 15% of a phrase's
  positions pages: a first position or a gap takes 9 bits on average in
  these documents, against a one- or two-byte varint now. Not worth a
  format change alone.
- Per-block counts in the footer (TIN answers single-term counts from its
  footer): a single-term count is the dictionary's `df` here already; the
  group directory's per-group counts settle groups one term holds.
- Positions are 55% of the blob.
- Terms of 2 to 7 postings spend more on their footer and header than on
  their slots; a footer is unnecessary below a few postings.
- External TID filters (a btree's result as a TID bitmap) folded into the
  native walks; a plain scan's stream holding a segment's matches lazily,
  a group at a time, rather than all of them at its start.
- `score()` of rows the scan did not rank opens each term per row; an
  exhaustive scoring of many rows at 150M would want a per-term cursor.
