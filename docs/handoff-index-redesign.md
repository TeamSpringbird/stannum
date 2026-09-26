# Handoff: redesign the index layout

Stannum answers the published Stack Exchange ranked workloads at 17.8 queries a
second where PlanetScale's TIN publishes 199, on the same instance type and
corpus. This document records what we measured, why the gap is the index layout
rather than the query code, what to build instead, and how to measure it without
an AWS campaign. Backwards compatibility is explicitly not a concern: there is no
production usage, old indexes need not load, and `REINDEX` is an acceptable
migration.

Branch `perf/ranged-reads`, pushed to remote `stannum`. The redesign below was
built on 2026-09-23 as a series of `WIP:` commits after `c707013`, meant to be
squashed into one; each step's measurements are recorded under "Order of
work". At every commit the full pgrx suite and the install, lifecycle, upgrade
and reference oracle gates pass; from the last commit the ranked-scan fuzz
smoke is a gate too. Segment format signatures went `STN1`
(item 1), `STN2` (items 2 and 3), `STN3` (item 5); only `STN3` is read, and
the `LSG` readers are gone. The meta page changed last (a stamp per dead
list), after the measurements below, so the local mocks predate it and any
further measurement starts with a rebuild. The 150 million row snapshot in
S3 is `LSG5` and must be rebuilt.

## The finding

At 150 million rows the index is 246 GB against 24 GB of shared buffers, so
almost every query reads from disk: 26 MB and about 1,500 pages per query,
which saturates the NVMe at eight clients. TIN's index for the same 85 GB corpus
is 50.7 GB, and it reads 1.7 MB per query.

PlanetScale's own guidance, from
<https://planetscale.com/blog/anatomy-of-a-postgres-search-engine> and
<https://planetscale.com/blog/introducing-tin>:

- An English text index with positions and frequencies is normally 30-50% of the
  corpus; TIN measured 33-61% across datasets.
- Positional data is stored apart from the postings so it is loaded only when
  needed.
- Visibility comes from a per-segment liveness bitmap of one bit per `ctid` plus
  the page-level visibility map; for all-visible pages TIN touches no heap page.
- TIN stores one representation of the document set: two-level `ctid` bitmaps,
  approaching one bit per posting for frequent terms.

Ours is 109% of the corpus live, and 310% as a relation. Every measurement below
is reproducible with the tooling in the last section.

## What we measured

Live segment bytes, 15,000,000-row mock, ten segments, 7,057,883,909 bytes total
(`script/dump-segments.py` then the `breakdown` example):

| section | bytes | share |
| --- | --- | --- |
| payload (positions and frequency buckets) | 3,134,821,907 | 44.4% |
| ordinals (document sets as ordinal bitmaps) | 1,840,605,506 | 26.1% |
| postings (the same document sets as TID lists) | 1,718,764,992 | 24.4% |
| dictionary | 273,810,325 | 3.9% |
| lengths | 59,999,440 | 0.9% |
| docs (ordinal to TID table) | 23,278,940 | 0.3% |
| page tables | 6,602,448 | 0.1% |

Two conclusions fall straight out. **The document set is materialised twice**, as
TID postings and again as ordinal streams, which is 50.5% of the index; the
ordinal streams were added for `LSG4` on top of the postings rather than
replacing them. And **positions are interleaved with the frequency bucket** in one
payload stream, so scoring a candidate reads pages that are mostly positions it
does not need.

Terms with `df >= 128` are 1.1% of terms and 86% of the bytes.

The relation is 20 GB holding those 7.06 GB, so 65% is retired runs and free
pages. `af24e4c` bounds reclamation per insert, which fixes the latency spike it
caused, but not the space.

Disk reads per ranked query in steady state on the mock (2 GB shared buffers
against a 21.7 GB index, 360 distinct published queries, 120 per style): 318
pages, 2.43 MB. By relation, index 92% and heap 8%. The heap is 49 visibility
checks per query, 25 of them random reads. Within the index, by area, now that
every scan reports its counters (see below): payload 42%, ordinals 28%, TID
postings 27%, and 2% planning. Scorer setup (capturing the view plus a
dictionary lookup per term per segment) is 14%, inside those areas.

| style | index pages | ordinals | payload | postings | heap | visibility checks |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| conjunction | 307 | 154 | 76 | 56 | 16 | 20 |
| disjunction | 205 | 90 | 115 | 0 | 58 | 125 |
| phrase | 368 | 1 | 182 | 184 | 1 | 0 |

Phrase queries are the heaviest readers and read only the two areas the
redesign removes or moves: the TID postings and the interleaved positions.
Disjunctions read no postings at all; their payload reads are the frequency
buckets the scorer needs, dragged in with positions. Measured with
`benchmarks/local/attrib.py`.

### Negative results worth not repeating

- Reading the document length lazily, only after a bound that costs no read, is
  correct and takes the disjunction median from 193 ms to 114 ms. Throughput did
  not move at all: 55.3 to 55.2 queries a second. Latency is not the constraint;
  bytes are.
- Shrinking the length window sixteenfold cut bytes copied by 42% and disk reads
  by 0.3%, because disk is page-granular.
- Raising the per-backend read cache to 256 MB eliminates disk reads in a
  small probe, but that is an artifact of a sixteen-query working set fitting in
  it, and eight backends at 256 MB exhausted the container, reproducing the
  out-of-memory kills that ended two AWS campaigns.
- Chunk-level skipping in the disjunction walk already works: 446 of 2,760
  candidate chunks are loaded. Seeding the walk with the true tenth-best score,
  which is the best any champion-list scheme could achieve, cut candidates scored
  by 43% and pages by 16%, and by nothing at all on the large disjunctions.

## The target

Roughly a four to five times smaller index. At 50 GB against 24 GB of shared
buffers the workload is mostly cached and CPU-bound, which is the regime where we
already measure well: the same build on the 15 million row prefix, with the index
resident, does 282 queries a second and beat TIN on the Wikipedia count workload
at 12,362 against 10,260.

## The layout to build

1. **One document-set encoding.** Keep the ordinal bitmaps, delete the TID
   postings entirely, and serve everything that reads postings today from the
   ordinal streams plus the page table. This is the single largest item and it is
   removal rather than invention. Check every consumer: the count fold, the
   candidate stream for unordered scans, phrase skeletons in `tinql`, merges,
   `verify`, and the dead-list intersections.
2. **Frequency inline with the document set.** Put the term-frequency bucket
   beside the membership bits in the ordinal chunk so scoring never touches the
   payload area. Dense chunks grow; measure the trade against the payload reads
   it removes.
3. **Positions in their own area, read only by phrase verification.** They are
   44% of the index today and are dragged through the scoring path for every
   candidate.
4. **A per-segment liveness bitmap, one bit per document**, consulted with the
   page-level visibility map, replacing the per-candidate heap fetch. Ours is 49
   random heap reads per query to return ten rows.
5. **Compact document lengths.** A one-byte length class for bounding, with the
   exact length read only for rows actually admitted, keeps scores bit-identical.
6. **Fewer, larger segments.** Scorer setup is 13% of reads and scales with the
   segment count: ten locally, eighteen at 150 million rows. The 3 GiB
   `SEGMENT_BYTES_CAP` and the `u32` run length are the current limits.

Items 1 and 3 are where the size comes from. Items 2, 4 and 5 are where the
per-query read count comes from.

## Order of work

1. **Close the attribution gap first.** Done 2026-09-23. PostgreSQL reported
   105,532 index page reads where the extension's counters accounted for
   58,335. Nothing unknown was reading the index: the EXPLAIN callback printed
   the area and phase counters only for scans that pruned by ordinal, so every
   phrase query (scored from the candidate stream) and every conjunction that
   fell back reported nothing. With the counters printed for every scan and
   reset at the start of unordered and count scans too, the extension accounts
   for 103,038 of the 105,522 pages and planning for the other 2,484. The
   table above is the corrected breakdown; the previous one understated payload
   and postings by leaving phrase queries out entirely.
2. **Delete the TID postings (item 1).** Done 2026-09-23, as segment format
   `STN1` (see `segment/src/segment.rs` and `segment/src/docs.rs`): the
   ordinal stream is the only document set, the document table is the page
   table plus a two-byte heap offset per document, dead lists are ordinal
   streams, and the write buffer carries the same streams. The `LSG` readers
   are gone. Measured on the mock, same build settings, ten segments:

   | | LSG5 | STN1 |
   | --- | ---: | ---: |
   | index relation, fresh build | 21.7 GB | 16.3 GB |
   | index build | 898 s | 1,105 s |
   | index pages per query, steady state | 293 | 210 |
   | phrase / conjunction / disjunction pages | 368 / 307 / 205 | 187 / 239 / 201 |
   | CPU per query, warm | 18.2 ms | 18.9 ms |
   | throttled mixed, 8 clients | 55.3 QPS, p50 89 ms | 130.7 QPS, p50 26 ms |
   | disk read per query, throttled | 5.4 MB | 1.9 MB |
   | private memory, 8 backends | 2.1 GB | 2.05 GB |

   Phrase queries lost their postings pages and read 11 pages of ordinals in
   their place; conjunctions lost the 56 postings pages their fallback read;
   disjunctions were already ordinal-only. The throttled run was measured
   with other work on the machine and is a lower bound. Three things had to
   be fixed on the way, each found by measuring: the page table was
   re-validated on every cursor (now once per reader); rows scored by
   location ranked the ordinal by a population count over the whole chunk
   (now a forward cursor per term with incremental counts); and the offsets
   table was fetched one heap block at a time, which tripled buffer hits,
   pushed private memory to 3 GB and had the container's OOM killer end a
   run (now 8 KiB windows). Verification lost its independent witness for
   term names, page-table block numbers, offsets and positions; the
   byte-flip test in `segment/src/verify.rs` records exactly what is and is
   not caught.
3. **Split positions out and put frequency inline (items 2 and 3).** Done
   2026-09-23 as `STN2`: each member of an ordinal chunk (or list) carries
   its term-frequency bucket as a nibble after the members, and the payload
   holds positions only. Scoring never opens a payload stream.
4. **Liveness and compact lengths (items 4 and 5).** Done 2026-09-23. The
   liveness bitmap already existed: a dead list is an ordinal stream, and
   the walk clears it from every chunk. What remained was the heap fetch per
   admitted candidate, now answered by the page-level visibility map when the
   page is all-visible; if a dead list was published after the view was
   captured the walk is repeated against the heap (the count path's rule).
   `STN3` adds a one-byte length class per document (`segment/src/length_class.rs`),
   a lower bound the walk tightens its bounds at before reading the exact
   length. Measured together on the mock, ten segments, throttled:

   | | LSG5 | STN1 (item 1) | STN3 (items 1-5) |
   | --- | ---: | ---: | ---: |
   | index relation, fresh build | 21.7 GB | 16.3 GB | 15.3 GB (4.98 GB packed, see item 6) |
   | index build | 898 s | 1,105 s | 760 s |
   | index pages per query | 293 | 210 | 182 |
   | heap pages per query | 25 | 25 | 4.7 |
   | conjunction / disjunction / phrase index pages | 307 / 205 / 368 | 239 / 201 / 187 | 200 / 104 / 242 |
   | CPU per query, warm | 18.2 ms | 18.9 ms | 16.7 ms |
   | warm unthrottled mixed, 8 clients | 281 QPS | 165 QPS | 385 QPS |
   | throttled mixed, 8 clients | 55.3 QPS, p50 89 ms | 130.7 QPS, p50 26 ms | 378 QPS, p50 7 ms |
   | disk read per query, throttled | 5.4 MB | 1.9 MB | 0.6 MB |
   | CPU busy of 8 cores, throttled | 1.4 | 3.4 | 6.7 |
   | private memory, 8 backends | 2.1 GB | 2.05 GB | 2.16 GB |

   Disjunctions read no payload at all now (104 pages, all ordinals); the
   throttled workload is CPU-bound, which is the regime the target named.
   Phrase queries still read 223 pages of positions per query; they are the
   remaining disk cost and are untouched by items 2 to 5 by design.
5. **Segment sizing (item 6).** Partly done 2026-09-23: an index build ends
   by compacting its directory, merging the smallest segments that fit one
   run under the segment byte cap until no two do, so a build leaves the
   fewest segments the cap allows instead of every tier's leftovers. Lifting
   the 3 GiB cap and the `u32` run length is deferred: a direct merge holds
   every input blob and its output in memory, so segments larger than the
   cap need a streaming merge before they can be built inside a 32 GB
   container next to 24 GB of shared buffers.

   Measured on the mock: 2 segments instead of 10, scorer setup 13.1 to
   4.5 pages per query, index pages 182 to 166, CPU per query 16.7 to
   14.7 ms, throttled mixed 378 to 383 QPS (CPU-bound either way). The
   relation grew from 15.3 GB to 20.2 GB, because the runs the compaction
   merged stayed on the pending list, to be reclaimed a bounded slice per
   later insert. Freeing them at once did not help either: freed pages are
   reusable but never returned, and the tier merges of any build retire
   about as many pages as they keep. A build now ends by packing its live
   runs into its lowest pages and truncating the rest. **The built relation
   is 4.98 GB**, against 21.7 GB for the same rows in `LSG5`: 4.4 times
   smaller, and 66% of the 7.6 GB table. Two segments, scorer setup 4.5
   pages per query, 166 index pages and 4.7 heap pages per query, 15.1 ms
   CPU per query warm, 358 QPS throttled at the mock's 400 MB/s read cap and
   408 QPS with the byte cap lifted (CPU-bound at 7.8 of 8 cores, the IOPS
   cap kept). The packed relation reads half the IOs per query of the
   unpacked one (18.7 against 37.6) but twice the bytes (1.1 against 0.5
   MB/query): the kernel's readahead fires far more often on a dense file
   whose neighbours are cached, so the average read grew from 14 to 64 KiB.
   The mock's byte cap was set from what the AWS NVMe delivered, not from
   what it can deliver, so on the instance this trades scarce IOPS for
   plentiful bandwidth; `benchmarks/local/mock-run.sh` keeps both caps, and
   a run pinned by the byte cap shows it as a read rate near 400 MB/s. The
   relation now fits the mock's 2 GB of shared buffers plus page cache far
   better than the 21.7 GB it replaced, and would fit the AWS instance's 24 GB
   of shared buffers at 150 million rows if size scales with rows (about
   50 GB, TIN's 50.7 GB).

   Compacting the test index into one segment exposed a pruning gap: the
   walk decided whether to prune once per chunk, before that chunk's
   candidates were admitted, so the chunk that fills the top k scored every
   candidate in it. The threshold is now consulted per sub-block and per
   candidate.

### Predicted degradations, measured

Two paths do per-document work where grouped postings did per-page work:
the page-mask count strategy (`stannum.count_fold = off`) and unordered
scans, both served by the page cursor over an ordinal stream. On the STN3
mock, warm: a count of `the OR is OR to` (12.4 million matches) takes 0.8 ms
by the default fold and 299 ms by page masks; `python AND error AND file`
(8,196 matches) 0.7 ms and 26 ms. An unordered `the AND is LIMIT 100000`
returns in 151 ms and the full 8,196-row conjunction in 269 ms, most of it
heap fetches. The page-mask path is slower than it was but is not the
default for counts, and unordered scans are bounded by the heap, not the
index. The mutation workload, measured as the published disjunction workload
with updates on the compacted STN3 mock: 83,039 updates in 300 s at the
driver's rate, none failed, update p50 2.7 ms, p95 4.9 ms, worst 1.0 s,
with disjunctions at 383 queries a second beside them and post-update
checks agreeing. The write-buffer re-encode on out-of-order inserts does
not show.

### Found by the fuzzer on the way, all fixed

The ranked-scan fuzzer (`postgres/tests/ranked_fuzz.py`) failed on the new
format in three ways, each with a regression test now, and `--smoke` is a
gate (`benchmarks/local/gates.sh`):

- The disjunction walk masked a segment's dead ordinals before it rebuilt
  its candidates from the essential terms, and never masked a list chunk:
  a deleted document was scored and its location, by then reused by a row
  that never matched, returned. This was latent in the prototype's ordinal
  walk and became live when that walk became the only ranked path.
- A dead list is rewritten whole by every VACUUM that finds more dead rows,
  and a replacement can land in the freed pages of the list it replaces,
  at the same byte count; readers keyed their cached dead set by the run
  alone and served the old list. Each dead list now carries a stamp from
  the index's generation counter (`SegmentEntry::dead_stamp`; the meta page
  holds 48 pending runs instead of 64 to make room).
- The per-row scorer's term cursors, forward-only since this redesign,
  were reopened for a request behind them but not once exhausted, so a
  join, which scores rows in its own order, scored every row after the
  term's last member without that term.

The failure the handoff previously recorded as pre-existing (`fox^0`, join,
`LIMIT 20 OFFSET 10`) was the third of these at a different seed and does
not reproduce any more.

Re-measure after each step rather than at the end: three hypotheses were
falsified today by measuring, and each one looked obvious beforehand.

## Measured at 150 million rows, 2026-09-24

The published Stack Exchange workloads on the i7i.8xlarge protocol (8
clients, 8 CPUs, 32 GB container, 24 GB shared buffers, 600 s), built from
`b961ded` and measured from the saved database with the two fixes below.

| | LSG5, 2026-09-23 | STN3, 2026-09-24 | TIN, published |
|---|---|---|---|
| index relation | 246.5 GB | 47 GB | 50.7 GB |
| segments after build | 56 | 18 | |
| build wall time | 3 h 42 min | 5 h 06 min | |
| mixed QPS | 17.8 | 34.0 | 199 |
| mixed p50 / p95 / p99 ms | 217 / 1,396 / 3,904 | 76 / 804 / 3,060 | |
| conjunction-phrase QPS | 17.8 | 31.0 | |
| conjunction-phrase p50 ms | 201 | 65 | |
| disk read per query | 26 MB, ~1,500 pages | 3.1 MB, 62 IOs | 1.7 MB |
| CPU during the mixed run | disk-bound | 7.7 of 8 cores | |
| correctness | | 0 mismatches, 10 count + 2 ranked checks per run | |

Per family in the mixed run: conjunction p50 27 ms, disjunction p50 122 ms
(p99 914 ms), phrase p50 158 ms (p99 4.8 s). The disjunction-updates
workload ran 145,000 updates at p50 1.1 ms before one update reached the
driver's 20 s deadline, which fails its zero-error threshold; the old
format failed the same workload with 37 such errors.

The layout did what it was built for: the index is 5.2 times smaller and
the run no longer waits on the NVMe. Throughput only doubled because the
run is now CPU-bound at about 226 ms of CPU per query, against TIN's
implied 40 ms. Where that CPU goes, from EXPLAIN counters on the host
beside the 15 million row mock:

- Every query re-fetches all 18 page tables (67 MB) and dictionary
  indexes: the per-backend reader cache budget is exceeded at this segment
  count, so readers are dropped and rebuilt per statement. The mock, with
  two segments, pays nothing here.
- A stopword-heavy disjunction scores 101,000 candidates (4,200 on the
  mock), and each candidate copies an 8 KiB window of the length and class
  tables through the cache: 264 MB and 132 MB per query of memcpy.
- Phrases are scored exhaustively, with no positional pruning: 380 MB of
  positions and 5 s for "how to get the value"; phrase p99 is 4.8 s.
- Eighteen segments multiply per-term setup and chunk loads; the 3 GiB
  cap forced 18 here against 2 on the mock.

Two harness-side problems cost the first attempt: the per-row scorer
re-parsed a term's stream on every lookup past its end (`20d0db9`), and the
ranked reference ran as a sequential scan because the statement never
disabled sequential scans (`7978381`). Both are fixed; the campaign ran with
`--ranked-validation-queries 2` so three workloads fit before the host's
expiry, which skips the exhaustive disjunction reference at this scale. The
build's extra 85 minutes are the single-threaded compaction and pack at the
end, which read the relation at about 130 MB/s.

Rerun with readers kept resident and the view lock released before
readers load (`1406e5a`): mixed 33.8 QPS, p50 68 ms (from 76), p99 3.2 s;
the same disjunction warm in one session fetches no page tables any more
but still copies 264 MB of lengths and 132 MB of classes for its 101,000
candidates, 431 ms warm. The per-query cost is candidate work, not
reader setup.

Per-candidate cost, 2026-09-24 afternoon, on the 15 million row mock
with candidate counts unchanged throughout: the stopword disjunction
warm went from 12.0 ms to 6.5 ms and a five-term conjunction from 7.6 to
4.9 ms, by reading the debug seed setting once per walk instead of per
threshold check, caching the per-bucket bound table per chunk, reusing
the scoring scratch vectors, counting a candidate's rank from the
previous candidate's word instead of the chunk's start, and dropping a
sweep of every bucket that could never tighten the sub-block bound
(`b747b84`, `eaa9507`). Tightening the sub-block bound itself, per bucket
at that bucket's shortest document, removed under 2% of candidates: with
stopwords some member of every sub-block carries a high bucket, so what
separates candidates is their own bucket, which is the score. Profiling
in an OrbStack container: `perf` cannot attach (perf_event_open is
refused even privileged), but gdb stack sampling of a plpgsql loop over
the query is enough to rank hotspots; see the progress notes. A local
150 million row database (`/tmp/stannum-ordinal-poc/local150m`) was
building as this was written, to measure these at eighteen segments.

Measured on the local 150 million row database (18 segments, warm,
same database, 32 GB container with 24 GB of shared buffers) on
2026-09-24 at 18:00 ET, the build's image `f099667` against `eaa9507`:
the stopword disjunction 210 to 117 ms (102,000 to 96,000 candidates), a
five-term conjunction 94 to 70 ms (91,000 to 79,000), a three-term
disjunction 94 to 48 ms (2,400 to 2,300). The three-term case is the
next lever: 2,300 candidates but 6,693 chunk loads, every chunk of every
term in every segment, because a chunk's bound over 65,536 documents
beats a top-ten threshold for any moderately common term. Chunk-level
cost, per segment, is where the time goes once candidates are cheap.

The mixed workload on the same local database with the harness's byte
cap lifted (the IOPS cap kept), CPU-bound at 7.4 of 8 cores like the
host: `f099667` 42.9 QPS, p50 56 ms; `eaa9507` 47.3 QPS, p50 50 ms.
Per family, `f099667` to `eaa9507`: conjunction p50 21 to 19 ms,
disjunction p50 95 to 70 ms with p99 674 to 415 ms, phrase p50 125 to
124 ms with p99 3.8 to 4.0 s. The per-candidate work bought 10% on the
mixed number because a third of that workload is phrases, which it does
not touch: phrases are now the largest single cost of the published
mixed workload, and the disjunction tail the second.

The built database is cached as
`s3://springbird-dev-stannum-corpus-cache-860510875764/postgres-snapshots/stackexchange-150m-stn3-b961ded.tar`
(118 GB, manifest beside it), so a full-scale run now restores in minutes.

### Phrase pruning, 2026-09-25

Splitting the mixed run's time by query style (the k6 samples carry the
query id) put phrases at 78% of it: a third of the queries, mean 360 ms
against 74 ms for disjunctions and 29 ms for conjunctions, with the
slowest fifteen queries all phrases of common words ("you want to" 8.2 s,
"see the" 6.2 s). The slowest 5% of queries were 55% of the time. A
phrase was scored from the candidate stream: every document holding the
words in order had its positions read, then was scored. The same database
with phrases left out of the workload ran the conjunction-disjunction
style at 153 QPS, p50 31 ms.

`a821f91` walks a phrase as the conjunction of its words (their documents
are a superset, and a document scores the same under either) and reads a
candidate's positions only once it scores into the top k. Warm explain
at 150 million rows: "how to get the value" 131 ms with 1,127 position
checks over 244,722 scored candidates; "you want to" 13.7 ms; "see the"
9.7 ms. Mixed, byte cap lifted, CPU-bound at 7.2 cores:

| | `90c6215` | `a821f91` |
|---|---|---|
| mixed QPS | 51.8 | 141.8 |
| p50 / p95 / p99 ms | 43 / 539 / 2,446 | 29 / 187 / 336 |
| phrase p50 / mean ms | 118 / 360 | 25 / 61 |
| disjunction p50 / mean ms | 54 / 74 | 57 / 77 |
| conjunction p50 / mean ms | 19 / 29 | 20 / 31 |
| disk read per query | 6.1 MB | 2.1 MB |

Count and ranked checks were clean. Time is now 46% disjunctions, 36%
phrases, 18% conjunctions; the slowest queries are still long phrases of
common words (worst 3.9 s), whose conjunction admits many candidates
before the top ten fill.

### Phrase tail and sub-block bounds, 2026-09-25

Two branches were built and measured in parallel. `perf/subblock-bounds`
(estimator only, behind `stannum.debug_bound_estimate`) asked whether a
finer stored bound per sub-block would cut the candidates a stopword
disjunction scores: on the 15 million row mock an exact per-sub-block
maximum removes 1% ("the OR a OR you"), 13% (7 stopwords) and 23 to 25%
(8 and 15 words) of them, a quantized byte slightly less, at +2 to +18%
index size. A 1,024-ordinal sub-block of three 40 to 70% terms really
does hold a near-maximal document, so the format change was not built.

`perf/phrase-verify` (merged here as `3c4a3a7`, `e589756`, `d9068b1`)
reads a phrase candidate's position lists rarest word first and drops
the candidate at the first adjacent pair that cannot match, in both the
ordinal walk and tinql's streamed `SpanFilter`; a filled top k also
checks the rest of a sub-block best score first. Decoding was 98% of a
check. Warm explain at 150 million rows, `a821f91` to `d9068b1`: "a
number i would like to have" 1,237 to 356 ms, "it is work for you do"
1,985 to 633, "or of no use" 1,031 to 464, "with the one which you"
(every word elided, streamed) 3,327 to 1,963; short phrases unchanged.
Mixed, byte cap lifted: 141.8 to 153.8 QPS, p50 29 to 30 ms, p95 187 to
173, p99 336 to 277; phrase mean 61 to 48 ms; count and ranked checks
clean. Time is 49% disjunctions, 31% phrases, 20% conjunctions.

### Full scale on the published host, 2026-09-25 (r7)

`64468d5` (the phrase pruning, before the phrase-tail work) on the
i7i.8xlarge protocol, restored from the local database uploaded as
`postgres-snapshots/stackexchange-150m-stn3-f099667` (the earlier
snapshot predates the meta page layout of `36355bc` and no longer opens):

| | STN3 `b961ded`, 2026-09-24 | `64468d5`, 2026-09-25 | TIN, published |
|---|---|---|---|
| mixed QPS | 34.0 | 115.2 | 199 |
| mixed p50 / p95 / p99 ms | 76 / 804 / 3,060 | 35 / 234 / 418 | |
| conjunction-phrase QPS | 31.0 | 143.4 | |
| conjunction-phrase p50 / p99 ms | 65 / | 27 / 453 | |
| disjunction-updates QPS, query p50 ms | failed | 67.3, 74 | |
| updates in 600 s, p50 / p99 / worst ms, errors | 145,000, 1.1 / / 20,000, 1 | 243,116, 1.2 / 6.1 / 1,457, 0 | |
| count + ranked checks, before and after updates | clean | clean | |

The write workload passed for the first time at full scale, on the
database packed by `f099667` with an empty free-space map: no update
reached the driver's deadline. The same commit measured 141.8 QPS mixed
on the local M4, so the host runs about 0.8 of the local figure. The `cross_engine_membership_differences`
field names four trace disjunctions whose counts differ from the
published engine's; the local runs report the identical four, so it is
the trace, not the index.

### Per-candidate lookups and the walk's inner loop, 2026-09-25

A single-user perf profile of two common-word disjunctions at 150
million rows put 44% of the time inside the walk itself, 15% in
`length_class` and 11% in `Lengths::get` (a buffer read per candidate
each), and 14% in chunk loads. `3280717` keeps the class window last
read, as the length table already did: mixed 153.8 to 166.9 QPS, p99
277 to 264 ms, disjunction p50 57 to 51 ms.

The walk's inner loop was the first bound of every member of the
essential terms' union, tested against every present term; the
disassembly of the profile's hottest addresses resolved there. Gathering
the terms' words per 64-bit word and stopping early (shelved as
`/tmp/stannum-ordinal-poc/v22-per-word-first-bound.patch`) changed
nothing measurable, because the cost is the iteration count. ANDing each
word with every required term's word first (a term the other bounds
cannot reach the threshold without) cut the 7 stopword disjunction 89.7
to 74.2 ms and "python OR java OR sql" 33.0 to 20.7 ms warm; the mixed
workload stayed within noise at 165.6 QPS.

### Two rounds of parallel branches, 2026-09-25

Each branch was built off the branch head, validated on the 15 million
row mock by its own agent, then measured here on a quiet machine at 150
million rows (mixed, byte cap lifted, 300 s), one build after another.

| branch | mixed QPS | against | merged |
|---|---|---|---|
| `perf/length-lookups` (bucket bound before the length read) | 169.1 | 160.0 | yes |
| `perf/chunk-loads` (bitmap chunks read in place, lazy loads) | 168.0 | 160.0 | yes |
| `perf/elided-phrase` (all-elided phrases walked by ordinal) | 163.0 | 160.0 | yes, for the 2 s tail |
| all three merged (`7eb033f`) | 174.2, 176.7 | | |
| `perf/threshold-warmup` (best-bounded chunks first) | 172.2 | 176.7 | no |
| read cache 256 MB instead of 64 (setting only) | 165.0 | 176.7 | no |
| `perf/word-bounds` (a word's lanes sieved before the per-member bound) | 188.1 | 176.7 | yes |

`perf/word-bounds` also cut p99 from 280 to 194 ms: the long trace
disjunctions went 332 to 126, 311 to 146 and 431 to 149 ms warm. A profile
had put 70% of the walk's own time in bounding each member of the
essential union against every term; the sieve (per-sub-block essential
terms, then a bit-sliced conservative sum of the terms' sub-block bounds)
clears 94 to 97% of those members a word at a time.

`perf/threshold-warmup` is kept on its branch: a perfect seed would cut
the 5 word stopword conjunction from 45 to 3.4 ms and 7,893 to 908 chunk
loads, but the directory-bound warm-up recovers only 7,893 to 6,464, and
it regresses phrases whose best-bounded chunks hold no match. The merge
of the three first-round branches turned up a pre-existing bug the
property test caught: `DocCursor::seek` resurrected an exhausted
universe under `NOT` (`7eb033f`).

Found then, fixed on `perf/mixed-disjunction`: a disjunction with a phrase
child (`a OR "i m"`, which is also how `i''m` parses) was not a prunable
shape and scored every match, 17 s on the mock. The published trace never
mixes the two. Such shapes (and `(a AND b) OR c`, `a AND (b OR c)`,
`a AND NOT b`, `AT LEAST n OF [...]`) now walk the disjunction of their
scoring terms, testing each candidate against the shape: 1 to 4 ms.

Time now: disjunctions 47%, phrases 32%, conjunctions 21%.

### Round three, 2026-09-25

Against 4eb4f51 at 181.9 QPS, each alone, then merged as `fbf6e52`:

| branch | mixed QPS |
|---|---|
| `perf/candidate-reads` (length and class pages held pinned for a walk) | 209.6 |
| `perf/phrase-decode` (a positions seek passes a slot's entries in one pass; one-byte varints a word at a time) | 187.0 |
| `perf/mixed-disjunction` (mixed shapes walked over their scoring terms) | 187.4 |
| all three merged | 205.9, p50 25, p99 178 ms |

The merge is within noise of `perf/candidate-reads` alone; warm
explains put the mixed-shape code at about 3% on pure shapes, worth a
fast path. `perf/mixed-disjunction` makes `a OR "i m"` 16.2 s to 1.6 ms
on the mock and `(how AND to) OR python` 5.6 s to 0.6 ms; prefixes,
regexes and fuzzy terms in a disjunction still score every match, and
`al* OR python` (over the expansion cap) now at least answers a cancel.
Predicted on the published host at the 0.81 ratio of r7: about 167 QPS.

### Round four, 2026-09-25

Chosen from a warm profile over a stratified 60-query sample of the trace
(chunk loads 30%, the walk's loops 20%, phrase checks 17%, candidate
scoring 12%). Each against b1899a0 (209.0 QPS; rerun 210.7), then merged
as `74c678f`:

| branch | mixed QPS |
|---|---|
| `perf/in-place-reads` (ordinal chunks and position spans read from pinned shared-buffer pages during a walk, not copied into the per-backend cache) | 301.0, rerun 301.3 |
| `perf/walk-overheads` (sorts, lazy row readers, parsed chunk bounds kept across statements, cached threshold) | 222.6 |
| `perf/conj-warmup` (a conjunction's 256 best-bounded chunks first, guarded by an estimate of its matches) | 206.9 |
| all three merged | 306.1, p50 16, p95 86, p99 128 ms |

In-place reads gained far more under eight clients than single queries
showed (the copies were per backend, into a 64 MB cache each). A walk
now holds at most a few dozen pages pinned; a test cancels walks
mid-chunk and checks no pin survives. Short disjunctions are 10 to 25%
slower than before it ("python OR java OR sql" 13.5 to 15.6 ms warm):
past eight pins PostgreSQL's pin bookkeeping moves to a hash table.
The conjunction warm-up is flat on the trace but takes the stopword
conjunction from 30.5 to 4.5 ms.

Predicted on the published host at r7's 0.81 ratio: about 248 QPS, above
TIN's 199. The ratio came from a build bottlenecked elsewhere, so an AWS
run is the real check.

### Round five, 2026-09-25

Against 382c685 (307.0 QPS), each alone, then merged as `e867445`:

| branch | mixed QPS |
|---|---|
| `perf/cheaper-pins` (pin through the buffer a block was last seen in, `ReadRecentBuffer`) | 322.4 |
| `perf/conj-walk` (phrase positions checked before scoring until the top k fills; a conjunction scorer bounding per class per sub-block; phrase slots read rarest first against the nearest read leaf) | 340.7 |
| both merged | 359.6, p50 12, p95 77, p99 117 ms |

Count and ranked checks clean. At r7's 0.81 ratio: about 290 QPS on the
published host, which an AWS run must confirm.

### Full scale on the published host, 2026-09-26 (r8)

`313a696` (rounds one to five) on the i7i.8xlarge protocol, restored from
`postgres-snapshots/stackexchange-150m-stn3-f099667`:

| | r7, `64468d5` | r8, `313a696` | TIN, published |
|---|---|---|---|
| mixed QPS | 115.2 | 269.9 | 199 |
| mixed p50 / p95 / p99 ms | 35 / 234 / 418 | 15 / 114 / 172 | |
| conjunction-phrase QPS | 143.4 | 437.9 | |
| conjunction-phrase p50 / p99 ms | 27 / 453 | 12 / 91 | |
| disjunction-updates QPS, query p50 ms | 67.3, 74 | 127.6, 40 | |
| updates in 600 s, p50 / p99 / worst ms, errors | 243,116, 1.2 / 6.1 / 1,457, 0 | 254,224, 1.1 / 5.5 / 1,379, 0 | |
| count + ranked checks, before and after updates | clean | clean | |

The host ran mixed at 0.75 of the local figure (r7: 0.81). Predictions
made from the local per-style costs before the last two workloads
reported: conjunction-phrase about 400 (370 to 430), measured 437.9;
disjunction-updates about 130 (115 to 145), measured 127.6. No pin or
buffer warnings in any server log.

## How to measure

Everything below runs on a laptop and needs no AWS. See
`benchmarks/local/README.md`.

The mock reproduces the out-of-memory regime: the 15 million row prefix, a 21.7
GB index against 2 GB of shared buffers in a 5 GB container, eight clients on
eight CPUs. Two details make a Docker container behave like the instance. Its
disk is served from the host's page cache at memory speed, so reads are capped
through `STANNUM_DOCKER_RUN_ARGS`, which `benchmarks/tin.py` passes to
`docker run`. And copying and validating the database leaves the index in the
VM's page cache, uncharged to the container, so the runner drops that cache when
the measurement starts.

- Build the database once: `benchmarks/local/mock-build.sh`, about 25 minutes
  (`STANNUM_IMAGE` picks the image, `STANNUM_MOCK` the directory). It is
  saved and reused by every later run, and is only valid for the segment
  format its image wrote.
- A workload: `benchmarks/local/mock-run.sh IMAGE LABEL STYLE UPDATES [SECONDS]`
  reports queries a second, latency, disk read per query and CPU.
- Per-query detail: `benchmarks/local/mock-probe.py IMAGE LABEL [N]` reports
  pages touched, candidates scored, disk read and time with the cache dropped
  before each query.
- Read attribution: `benchmarks/local/attrib.py IMAGE [WARM] [STEADY]` runs a
  warm-up slice and then a disjoint steady slice of the published queries and
  reconciles three views of every page read: `pg_statio` by relation, the
  EXPLAIN node counters, and the extension's area and phase counters, per
  style. It needs `psycopg`; `/tmp/stannum-venv/bin/python` has it on this
  machine.
- An iteration is a new image plus a five-minute run:
  `python3 benchmarks/tin.py build-image --image stannum-bench:arm64-<tag> --base postgres:18-trixie --output /tmp/<tag>`.

Read accounting, all behind `EXPLAIN (ANALYZE, BUFFERS)` on the custom scan:
`Bytes Fetched` and `Disk Pages By Area` per segment area, `Disk Pages By Phase`
for paths outside the segment reader, `Walk Setup Blocks`, `Walk Body Blocks`,
`Chunks Loaded`, `Visibility Checks`. `segment::cache::set_disk_probe` is the hook
that lets the segment crate attribute storage reads. `stannum.debug_seed_score`
prunes a walk against a known threshold, which is how the champion-list idea was
tested and rejected.

Index size: `script/dump-segments.py --dbname DB --index NAME --out DIR
--data-directory PATH` dumps segment blobs (the `--data-directory` override is
for a server in a container whose directory is bind-mounted), then
`cargo run -p segment --release --example breakdown -- DIR/*.segment`.

Correctness on every change: `cargo pgrx test pg18 -p stannum` and the four
gates. The pgrx suite includes a test that compares the pruned top k against
exhaustive scoring bit for bit, which is the one that catches pruning mistakes.
Use `python3 /tmp/stannum-pgrx-lock.py -- <command>` to serialise pgrx runs, and
reinstall the release build afterwards, since `cargo pgrx test` overwrites it.

A 322 GB snapshot of the built 150 million row database is at
`s3://springbird-dev-stannum-corpus-cache-860510875764/postgres-snapshots/stackexchange-150m-lsg5-7372fc6.tar`,
so a full-scale run restores in about fifteen minutes instead of rebuilding for
3.7 hours. It is only valid while the segment format matches, so this redesign
retires it; rebuild once the new format settles.

## Also outstanding

- The published write workload at 150 million rows completed for the
  first time on 2026-09-24 at 17:40 ET, on the local 150 million row
  database built by `f099667` and measured with `eaa9507`: 62,361 updates
  in 300 s at p50 3.5 ms, p95 6.7 ms, worst 1.07 s, zero errors, queries
  p50 76 ms and worst 2.0 s beside them, count and ranked checks clean
  before and after the updates. The history below explains what it took.
  Previously: on
  STN3 one update in about 220,000 reaches the driver's 20 s deadline (the
  old format: 37), and its zero-error threshold fails the run. Updates
  themselves take 1.1 ms at the median. A watcher on the host saw the
  stall: the update sits 20 to 30 s in data-file reads holding the index
  meta page exclusively, and every reader queues behind it on the buffer
  lock. Releasing the meta page before readers load (`1406e5a`) did not
  change it, so the holder is the writer's own work under the lock. Two
  walks in `storage/mod.rs` are unbounded there: `drain_pending`, which
  frees every removable retired run once the pending list is full
  (`MAX_PENDING`, now 48), and `prepend_chain`, which walks a whole
  retired run to join chains when the list is full. A merge of a large
  segment retires millions of pages. Fixed in `36355bc`: a run records
  its last page, so the join writes one page, and the locked drain frees
  at most `stannum.reclaim_pages` pages per call. Locally, 180 inserts of
  2,000 rows with the pending list pinned by a repeatable-read snapshot
  and then released ran at p50 80 to 110 ms with a worst of 1 to 2 s, all
  of it the merge in the same statement, and `verify_index` was clean
  before and after VACUUM. That fix was necessary but not the stall the
  mock reproduced: with it, the local update workload still stalled once
  for 9.5 to 11 s, and a backtrace of the lock holder put it in
  `Buffer::allocate`, walking stale free-space-map entries page by page
  under the meta lock. The pack at the end of a build reused pages from
  its own free set without marking them used in the map, so a packed
  index started with an entry per reused page (millions at 150 million
  rows). Fixed in `f099667`, with the packing test asserting an empty map
  after a build. On the 15 million row mock the update workload then ran
  92,723 updates at p50 2.1 ms with a worst of 967 ms (from 9.5 to 11 s)
  and a worst query of 306 ms (from 9.6 to 11 s), zero errors, post-update
  checks clean. The full-scale proof is a rerun of the write workload
  from a database built by `f099667` or later; the cached snapshot was
  packed by the old code and carries the stale map, so it needs a rebuild
  or a `VACUUM` of the index's map first.
- The article's four charts carry full-corpus Stannum runs for the mixed and
  conjunction-phrase workloads and prefix runs for the write workload. The count
  chart is from an older build. The importer replaces the series per chart, so
  re-importing a newer run is enough.
