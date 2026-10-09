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
the segment holds documents in has a width `w`, the largest line pointer
offset among them, a first page `f` and a span of `p` pages to its last
(256 but at a heap's or a segment's edges), and `p * w` **slots**: tuple
`(block, offset)` is slot `(block % 256 - f) * w + offset - 1` of group
`block / 256`, and the groups' slots are numbered in a row. The span keeps
a table of a few dozen pages from paying, in every bitmap, for pages it
does not have. A slot names a ctid with one table of groups
and a multiply, slots sort as ctids, and every term's members in a group
form a bitmap of the same shape, so two terms combine word by word.

A slot per possible offset up to 291 would cost a bit per line pointer of
every page; the group width cuts that to what the group uses. On the
Stack Exchange heap (20.5 rows per page on average, 4 to 70) a group has
1.61 slots per document; groups of 16, 64 or 1,024 pages would have 1.29,
1.43 or 1.91. The exact per-page widths would give one slot per document,
which is the ordinal numbering again, with a page table to reach the ctid.

### Document set and rank directory

The segment's documents in slot space: per group a 13-byte directory entry
(group, width, first page, span, documents before it, kind) and either the
group's slots as a bitmap of whole words or, when every page holds offsets
`1..n`, a byte of `n` per page (0.06 MB per million documents here). A reader decodes it once into a
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
         | form u8, [footer_len], payload_len, [lengths_len],
           [footer], payload, [lengths], tf
form    := 1 sparse | 2 grouped, | 0x80 when lengths, | 0x40 when no footer
lengths := base varint, width u8, (length - base) packed at width, by posting
payload := sparse:  Elias-Fano over the term's slots, universe = all slots
         | grouped: groups, (index gap, (count - 1) << 2 | kind, [len])*,
                    one container per group the term occupies
container := grid:  the group's slots as a bitmap of whole words
           | ef:    Elias-Fano over the group's local slots (universe p * w)
           | paged: a page mask (or page list) and per page a bitmap, a
                    packed sorted list of offsets or a run, the smallest
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

**Ranked top k** (`engine/src/tinshape/rank.rs`). Exact BM25 from the TF
tail and the DL sidecar, summed in the scorer's term order, ties broken by
ctid, dense terms elided as today: the answers are bit-identical to
scoring every match. A segment keeps the records it parses and the
footers it decodes between queries (`Segment::resolve_memo`,
`Segment::footer_memo`, 16 MiB), as `STN3`'s walk keeps decoded bounds.

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
- A phrase reads positions only for a candidate that would enter the top
  k. Until the top k fill every match does, so its positions are checked
  first; after, a group's candidates that score into the top k wait and
  are checked best first, so a confirmed one raises the threshold over the
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
  in the group), a group whose mask is empty reads no other row, and a
  sub-range whose mask is empty is skipped. Each other 1,024-slot
  sub-range is bounded by its terms' blocks and, unless pruned, each member
  of its mask is weighed alone: the sub-range bounds of the terms it holds,
  summed, must reach the threshold (a word's members are first weighed
  together by the terms holding any of them). At 150 million rows a
  group's mask holds about 85 members in a third of its words, and this
  costs less than planning each sub-range as `STN3` planned a sub-block and
  sieving all its words bit-parallel (`LaneSums`), and lets fewer through.
  A candidate is then bounded by its terms' blocks, then by its buckets as
  above, and scored exactly once its length is read.
- Candidates come in ctid order, so one that only ties the threshold ranks
  after the k-th row and is skipped like a lower one; every bound is
  compared with a relative margin (`1e-5`) far above `f32` rounding.

## Writing and maintaining it

**Building and the write segment.** A build or a promotion produces `TNS1`
from documents (`SegmentBuilder::finish_tns`): sort the segment's ctids,
derive the grid, encode each term's slots. The write buffer of forward
records is TIN's mutable write segment: sealed in place when full, then
promoted into a `TNS1` segment (see [segmented
storage](segmented-storage.md#writing-an-index)).

**Merges.** Nothing is renumbered. A merge's grid is the union of its
inputs' groups, each group's width the largest of the inputs' and its
span the union of theirs. Containers
hold slots local to their group, so a group only one input holds keeps its
width and its containers are copied as they are, counts included; a group
two inputs share (rows updated into pages another segment covers) is
re-encoded at the larger width, which moves each page's offsets. A term's
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

1. the output group has the input group's width, first page and span;
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
  `Segment` alive across queries to benefit); `Segment::positions`
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
scored; nothing is kept but what is parsed from them. The span ends when
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
  pages in place, a run of words per page (a grid's rows are copied into
  a row anyway in the disjunction walk), which needs the walks to take a
  container as up to two slices. Units could also start on a 64-byte line
  of the page (page offsets that are multiples of 64: data offsets of
  36 + 64k on a page): 507 MB for containers, 730 MB for TF blocks,
  2.9 GB for positions blocks (`tnsunits`, "line pad"), which only pays
  for units of a line or more.
- Decode footers by the blocks a walk reaches instead of whole: a common
  word's footer at 150M is some 20,000 blocks per segment, decoded per
  query whenever its memo entry was dropped (5.7% of a looping
  conjunction's samples in the server, `Postings::footer`). The walks read
  `Footer::last` and the frontier by block, so this is an API change for
  `engine/src/tinshape/rank.rs`.
- `TermSet::open` cloned the term's group directory and built a cursor
  entry per group per segment per query: 9.2% of a conjunction replay's
  samples (2.4% with the built groups kept beside the parsed record, an
  experiment since undone). The conjunction and phrase branch's lazy
  directory reads (`b7776bd`) cover it.

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
