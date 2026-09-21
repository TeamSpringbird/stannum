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
