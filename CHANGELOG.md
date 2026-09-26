# Changelog

## 0.1.0-dev

- First Stannum release baseline, independently versioned from Lead.
- PostgreSQL 17/18; TINQL matching, BM25 ranking, highlighting, segmented indexes,
  index verification and exact-count paths. Development software; see the
  architecture and benchmark guides for limitations.
- Versioned schema snapshot and automatic fresh-install/upgrade comparison.
- Explicit page/segment compatibility policy and release procedure.
- Heap permission and row-security checks for physical index diagnostics;
  catalog-dependent SQL functions use STABLE rather than IMMUTABLE.
- Malformed indexed-query and future page-version regression checks.
- VACUUM holds the index meta lock only to publish: dead lists, deferred
  merges, rewrites and reclamation work from a captured directory and are
  revalidated entry by entry before publication.
- `stannum.max_segments` is a soft bound enforced within the insert merge
  budget; only the 128-entry on-disk bound forces an unbudgeted merge.
- VACUUM reclaims pages a crash left unreferenced (`page N` warnings of
  `stannum.verify_index`) instead of requiring REINDEX.
- A phrase (with slop, gaps or a position filter) is ranked by the
  block-max walk of its words' conjunction; positions are read only for
  candidates that score into the top k. EXPLAIN reports `Positions Checked`.
- Ranked queries mixing terms, phrases and conjunctions under OR, AND,
  AT LEAST and AND NOT (`w OR "p q"`, `(a AND b) OR c`, `a AND (b OR c)`,
  `a AND NOT b`) are pruned by the disjunction walk over their scoring terms,
  each candidate tested against the query's shape; they scored every match
  (`a OR "i m"` 16.2 s to 1.6 ms on the 15 million row mock).
- Segment format `STN3`: a term's documents are stored once, as ordinals
  into the segment's document table with each member's term-frequency
  bucket beside it and a score bound per chunk, so scoring never reads
  positions; the positions stream holds positions only; and the document
  table maps ordinals to heap locations and back through the page table and
  a two-byte offset per document. A one-byte length class per document lets
  a ranked walk bound a candidate before reading its length, admitted
  candidates are checked against the visibility map before the heap, and an
  index build ends by compacting its directory under the segment byte cap. `COUNT(*)` over Boolean term
  queries folds those streams a chunk at a time instead of visiting each
  match (the 302 published Wikipedia count queries sum to 39 ms instead of
  3,767 ms in the replay harness); ranked scans prune over the same streams;
  dead lists are ordinal streams. Earlier `LSG` segments, which stored the
  document set a second time as tuple-location postings, are not read;
  `REINDEX` rewrites. `stannum.count_fold = off` disables the count path.
- Counts read the visibility map once and start over if VACUUM published a
  dead list meanwhile; earlier builds could count a tuple VACUUM had removed
  from a page it then marked all-visible.
- Counts check the matches of a heap page under one buffer lock.
- A built index is packed into its lowest pages and truncated; freed pages
  were reusable but never returned, and a build retires about as many pages
  as it keeps.
- Each dead list carries a stamp, so a reader's cached copy is not served
  once VACUUM replaces the list in the same pages at the same size.
- Packing a built index marks the pages it reuses as used in the free
  space map; it left every one of them listed as free, and an insert's
  page allocation then read stale entries one by one under the meta
  lock, for seconds at a time in the published write workload.
- Every run records its last page, so retiring a run joins pending chains
  with one page write instead of a walk of the run under the meta lock,
  and draining the pending list under that lock frees at most
  `stannum.reclaim_pages` pages at a time. The published write workload
  saw an update hold the lock for 20 to 30 s while such a walk read a
  retired merge input. The directory holds 96 entries (was 128).
- A view releases the index meta page before it loads segment readers,
  so a slow reload never queues a writer and, behind it, every reader.
  `stannum.reader_cache_mb` defaults to 384 (was 160), enough to keep the
  readers of a large directory resident across queries.
- The per-row scorer rewinds a term cursor instead of reopening the term:
  reopening parsed every chunk bound again, and an exhaustive reference
  over a hundred million rows did so for each row.
- Ranked disjunctions skip dead-listed documents in every chunk form, and
  per-row scores no longer depend on the order the executor hands rows over
  in (a join scores rows in its own order).
- Position payloads are read a span at a time from paged segments: the header
  and skip table, then entry-aligned spans of 64 skip slots as a cursor visits
  them. Whole extents were copied per term and query, which for frequent terms
  ran to megabytes and emptied the reader cache; the median phrase query over
  15 million Stack Exchange rows falls from 11 to 3 ms.
- A merge takes at most 3 GiB of input, dropping its largest members until it
  fits, and writing a segment longer than a run's 32-bit length is an error.
  Selection counted documents only, so at 50 million rows a tier merged into a
  segment over 4 GiB whose length wrapped: the build succeeded and every query
  then reported a corrupt segment.
- A pruned ranked scan that must read past its top k deepens the pruned search
  to 4k, 16k and so on before it scores every match. An update leaves its old
  version in the index beside the new one with the same score, so under
  steady updates a growing share of top tens held a row the snapshot could not
  see, and each such query scored millions of documents: eight clients ranking
  disjunctions over 15 million rows beside 1,000 updates a second fell to 2
  queries a second with a p99 of 20 s. They now sustain 125 with a p99 of
  208 ms.
- Ranked conjunctions with elided dense terms are pruned: an elided term's
  cursor joins the walk as a filter without a score bound. Over 15 million
  Stack Exchange rows the p99 of published AND queries falls from 165 to 38 ms.
- A ranked query whose terms are all elided or absent takes its top k from
  the heap-ordered candidate stream: every match scores zero and ties rank in
  heap order, so nothing is collected or sorted.
- Ranked disjunctions ordered by `stannum.score` are pruned when dense terms
  are elided: the walk covers the scoring terms and is exact whenever the top k
  score above zero. They were scored exhaustively before, 6.4 s at the median
  over 15 million Stack Exchange comments against 21 ms now.
- Ranked scans name a scored document through its term's ordinal stream
  instead of ranking its TID in the segment's document table, which has no
  skip structure and was decoded from the start by every ranked query: about
  15 ms per query at five million documents, growing with the corpus. Median
  top-10 latency over the published Wikipedia queries falls from 17 to 2.5 ms
  (OR), 16 to 1.1 ms (AND) and 17 to 1.7 ms (phrase).
- Segment format `LSG3`: one term bound for postings that fit a block, no
  payload skip slot for entry 0, and dictionary entries with gap-encoded
  extents; the 100k Wikipedia index shrinks by about a tenth with the same
  pruning. `LSG1` and `LSG2` segments remain readable; `REINDEX` rewrites.
- Foreground segment merges preserve dictionary/posting order through the
  validated direct-merge API, retaining existing encoding and publication locks.
  Oversized aggregate inputs retain reconstruction fallback; pending insert
  cancellation is checked after metadata unlock.
- VACUUM deferred merges and deletion rewrites use validated direct posting
  merges, with interruptible construction and unchanged stale-input publication
  checks. All-dead inputs leave no empty successor.
- After a fold, an insert frees reclaimable runs and merges one due tier of
  up to `stannum.deferred_merge_docs` documents outside the metadata lock.
  At 1,000 updates a second against 15 million Stack Exchange rows for ten
  minutes, ranked disjunctions went from 120 to 156 queries a second, the
  directory from 129 segments to 34 and the longest query from 34 s to 11 s.
- `target_segment_count`, `max_mutable_segment_size`, `max_merged_segment_size`
  and `dead_percent_threshold` now shape maintenance for the index that sets
  them instead of being ignored; unset, the `stannum.*` settings apply.
- A pruned disjunction whose scoring terms match fewer than k documents fills
  the rest of its top k from the matches of its elided terms, which tie at
  zero in heap order, instead of scoring every match. In the same write
  workload the longest query went from 10 s to 0.55 s and throughput from 156
  to 180 queries a second.
- The pruned ranked walk checks a row's snapshot visibility as it enters the
  top k, so the dead version an update leaves beside its successor never
  takes a place and the scan no longer deepens for it. With 300,000 of 15
  million Stack Exchange rows updated, one ranked disjunction in six had
  needed that second walk; none does now, and their total time fell 12%.
- The pruned disjunction walk sums its terms' maxima once per step instead
  of folding every prefix, and a sparse stream seeks through its bounds
  table without decoding the first posting of each block it skips. Mixed
  ranked queries at eight clients on 15 million rows: 313 to 340 a second.
- Benchmark harness: `--save-database` copies a built, vacuumed and checked
  database out of its container and `--load-database` starts a later run
  from that copy, so one build serves every workload of a campaign;
  `--ranked-validation-queries` samples the exhaustive ranked check;
  `build-image --base` builds a native image for rehearsals; a benchmark
  stack may live up to 24 hours.
- Prototype of ranking over the ordinal streams (ADR 0003), behind
  `stannum.rank_by_ordinal` (off): block-max WAND over the terms' chunks
  with only the essential terms' members visited within a chunk. Chunk
  bounds are derived from block bounds at query time until the format
  stores them. Same rows and scores as the postings walk on the published
  disjunctions over 15 million rows, at 10.7 ms median instead of 18.1 and
  65 ms at the 99th percentile instead of 154; the mixed workload at eight
  clients went from 345 to 417 queries a second.
- Segment format `LSG5`: every ordinal stream stores a score bound per chunk
  and per occupied 1,024-document sub-block, and ranked disjunctions walk the
  ordinal streams by default (`stannum.rank_by_ordinal`), see ADR 0003. On
  15 million Stack Exchange rows the published disjunctions take 8.9 ms at
  the median instead of 17.3 and 58 ms at the 99th percentile instead of
  154, with the same rows and scores; the mixed workload at eight clients
  runs at 436 queries a second instead of 347. The index grows 5% and builds
  2% slower. `LSG4` and earlier segments remain readable and rank through
  their TID postings; `REINDEX` rewrites.
- Ranked conjunctions walk the ordinal streams too: the rarest term leads
  through its chunks, the others and any elided terms are aligned to each,
  and the shared members are tested by bit. On 15 million rows the
  published conjunctions take 1.25 ms at the median instead of 3.0 with the
  same rows and scores; at eight clients the conjunction-phrase workload
  runs at 500 queries a second instead of 437 and the mixed workload at
  479 instead of 343.
- A backend's private memory no longer grows with what a query reads:
  payload spans, ordinal chunks, document lengths and postings windows are
  read into buffers the cursor owns and replaces, where earlier every
  fetched range stayed in the reader's arena until the query ended (a
  phrase of common words held 480 MB per backend at 15 million rows, and
  the 150 million row run was killed for memory). The ranges are shared
  through a least-recently-used cache of `stannum.read_cache_mb` (64 MiB)
  per backend, so frequent terms' chunks stay warm; `stannum.reader_cache_mb`
  defaults to 160 MiB, and a segment's dictionary index is held at every
  sixteenth block rather than whole, so eight backends fit beside 24 GiB of
  shared buffers in a 32 GiB container at 150 million rows. A cursor over a term's
  TID postings streams its block-bound table the same way and keeps only
  the bounds between its block and its lookahead, where it decoded and kept
  every block it passed (500 MB for one phrase at 150 million rows). Ranked queries the
  scorer cannot prune, such as phrases, score candidates as the stream
  yields them and keep only the top `k` rows instead of every match.
- An oversized or deeply nested TINQL query is a clean ERROR instead of a
  backend crash: 30,000 words, a 30,000-term OR chain or 5,000 nested
  parentheses overflowed the backend's stack and restarted every session.
  Word and OR chains parse into one flat node however long; a recursive
  descent parser replaces the pest one, bounded to 1,000 nesting levels
  (1,000 nested parentheses take about 0.95 MiB of stack, where pest took
  4.5 MiB) and 10,000 terms; lowering bounds a span's nesting to 2,000
  levels, and every recursive pass over a query calls `check_stack_depth`,
  so a smaller stack ends in "stack depth limit exceeded" instead.
  PlanetScale TIN 1.0.3 answers 3,000 words or OR terms and 1,000 levels
  and crashes at 10,000 terms and 5,000 levels. `MATCHES` patterns scan in
  linear time (pest backtracked exponentially over unclosed groups) and the
  `AT LEAST` estimate is linear for thresholds near either end.
- `target_segment_count`, `max_mutable_segment_size` and
  `max_merged_segment_size` take TIN's domains (1..4096, at least 131072
  bytes, at least 100 MB) and reject other values with SQLSTATE 22023, as
  TIN 1.0.3 does; unset still leaves the `stannum.*` settings in charge.
- A term repeated in a flat AND or OR chain adds its boosts in scoring and
  `score_inspect`, as in TIN 1.0.3: `a a` weighs `a` 2.0 and scores as
  `a^2`, where the repeat was removed before scoring. Matching is unchanged.
