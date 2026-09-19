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
