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
- Segment format `LSG4`: every term also stores its documents as ordinals
  into the segment's document table, and `COUNT(*)` over Boolean term queries
  folds those streams a chunk at a time instead of visiting each match. The
  302 published Wikipedia count queries sum to 39 ms instead of 3,767 ms in
  the replay harness. Earlier segments remain readable and are counted the
  old way; `REINDEX` rewrites. `stannum.count_fold = off` disables the path.
- Counts read the visibility map once and start over if VACUUM published a
  dead list meanwhile; earlier builds could count a tuple VACUUM had removed
  from a page it then marked all-visible.
- Counts check the matches of a heap page under one buffer lock.
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
