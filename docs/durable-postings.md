> Superseded. LDP1 was replaced by the segmented LDP2 format described in
> [segmented-storage.md](segmented-storage.md). Indexes in this format report
> `REINDEX required`. This document is kept as a record of the first slice.

# Durable candidate retrieval, format LDP1

New logged `tin` indexes now persist term-fingerprint → heap-TID postings.
Build and insert callbacks maintain them; term, Boolean and phrase bitmap scans return candidate
TIDs with recheck enabled, and VACUUM removes postings through PostgreSQL's dead-TID
callback. This is an initial storage implementation, not a competitive engine yet.

## Physical format

The index uses standard PostgreSQL pages and buffers. Block zero identifies the
format. Blocks 1–128 are fixed bucket heads; later blocks are overflow pages. A
stable FNV-1a 64-bit fingerprint chooses `1 + fingerprint % 128`. Matching requires
the whole fingerprint, not just its bucket. Collisions can only add candidates;
the existing text predicate and PostgreSQL visibility rechecks decide results.

Each page's payload follows the standard PostgreSQL page header:

| Offset | Bytes | Field |
| --- | ---: | --- |
| 0 | 4 | Format magic `0x4c445031` |
| 4 | 4 | Next overflow block, or `u32::MAX` |
| 8 | 4 | Tail block, meaningful on bucket heads |
| 12 | 4 | Posting count |
| 16 onwards | 16 each | Fingerprint u64, heap block u32, heap offset u16, reserved u16 |

Fields are little-endian. `pd_lower` covers the payload including every posting;
the unused region remains between `pd_lower` and `pd_upper`. Page magic, count,
boundaries and chain links are checked. This version has 509 postings per standard
8 KiB page and a 129-page (~1 MiB) minimum index footprint. It stores neither full
terms nor positions, document lengths or scoring statistics. Changes to this layout,
hash, bucket count or tokenizer contract require a format change and REINDEX.

Indexing uses the same default tokenizer as the existing `==>` predicate. It indexes
the expression values supplied by PostgreSQL, including partial/expression index
membership, and skips NULLs. Scoring reloptions do not change predicate tokenization
in the current code; pruning follows the predicate's behavior.

## WAL and locking

All changes modify copies returned by PostgreSQL's
[generic WAL API](https://www.postgresql.org/docs/18/generic-wal.html), followed by
`GenericXLogFinish`. Fresh pages use full images. Overflow publication changes the
bucket head, old tail and new page within one generic record, registering buffers
in lock acquisition order. Buffer ownership is scoped by a guard.

A bucket head serializes its chain: writers and VACUUM hold it exclusively;
readers hold it shared while copying candidate TIDs into the bitmap. Writers then
lock the tail and briefly take the relation-extension lock if allocating a page.
No operation holds two bucket heads. Readers check for interrupts between pages;
writers do so between term insertions. Dead/aborted postings remain harmless until
VACUUM removes them because every candidate is rechecked against the heap.

Generic WAL alone does not supply an index-VACUUM standby snapshot-conflict
protocol. Consequently scans during recovery deliberately use full heap-page
candidates. Selective recovery/standby reads remain a later milestone.

## Scope and compatibility

* Lowered terms, AND/OR, boosts and conservative positional expressions select
  persisted postings. Phrase candidates intersect term membership and still require
  exact positional heap rechecks. Unsupported OR branches force fallback; AND can
  retain a supported positive branch. NOT never complements an approximate set.
  PostgreSQL bitmap composition handles duplicate, unordered and lossy candidates.
  A 128-node plan budget bounds recursive execution; scratch bitmaps share a
  work_mem target, which PostgreSQL can exceed when lossification is insufficient.
* Old zero-page indexes remain usable through the fallback. `REINDEX INDEX name`
  builds the new format. Fresh logged indexes use it automatically.
* Temporary/unlogged indexes keep the zero-page reference implementation. This
  avoids introducing an unvalidated init-fork durability path. Unlogged reset and
  subsequent inserts have been tested.
* Unknown/nonempty formats fail rather than silently treating unreadable storage
  as an empty result. Downgrading binaries and then upgrading again requires a
  rebuild: the original implementation does not maintain these postings.
* VACUUM compacts each page; it does not unlink/recycle overflow pages. Inserts use
  available space at the current tail. Long-running update churn can therefore grow
  the file despite free space earlier in a chain. REINDEX reclaims that space.
* Fixed buckets, per-term WAL work and head-lock contention are known limitations.
  Large-corpus build/write performance is unmeasured. Ranking still reconstructs
  its original query-time corpus state. No speedup against competitors is claimed.

## Validation and next step

On native PostgreSQL 18, the extension suite passes 43 tests, including existing
expression/partial-index, update/delete, truncate, scoring and highlighting checks.
New tests cover overflow chains, rolled-back inserts and selective access. A query
matching one of 1,500 rows visits one heap page; a missing term visits zero.

`postgres/tests/postings_lifecycle.py` creates an isolated cluster with page
checksums and tests against an independent same-snapshot text oracle. It covers
VACUUM and reuse, concurrent writers and index creation, old snapshots during
VACUUM, REINDEX, immediate shutdown/WAL recovery, unlogged reset, truncation and a
clean restart. It also starts a streaming standby to verify the reference fallback.
This is targeted lifecycle coverage, not exhaustive crash/fault injection or a
production-readiness claim. PostgreSQL 17 has not been run locally for this slice.

```sh
export PATH="$(brew --prefix rustup)/bin:$(brew --prefix postgresql@18)/bin:$PATH"
cargo pgrx test pg18 --package tin --no-default-features --features pg18
python3 postgres/tests/postings_lifecycle.py
cargo clippy --locked --workspace --all-targets --no-default-features \
  --features 'pg18 pg_test' -- -D warnings
```

Next: measure paired original/fork performance, improve bulk insertion and page
reuse, then run the frozen corpus workload
against a separately identified fork image. Keep the baseline image unchanged.

## Safety review follow-up

The shared-branch [quality review](storage-quality-plan.md) introduced a private
checked page codec, buffer-owned read views and scoped mutable WAL views without
changing LDP1 bytes. Validation now covers all page bounds, roles, links, tuple
locations and terminal tails. Scan state is owned by a PostgreSQL memory context
so ERROR/cancellation can reclaim it even without normal end-of-scan. VACUUM
reports estimated heap-row counts rather than physical term-posting counts.

Snapshots acquired during recovery retain the fallback after promotion; newly
acquired primary snapshots can use selective retrieval. The lifecycle suite now
tests cancellation/backend reuse and this promotion behavior. Forty-three extension
tests and strict workspace Clippy pass on PostgreSQL 18. Full entry validation
during append increases insertion work; measure that cost before changing it.
