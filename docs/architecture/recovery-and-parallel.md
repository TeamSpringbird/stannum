# Relation persistence, recovery snapshots and parallel execution

## Temporary and unlogged indexes

New temporary and unlogged Stannum indexes use the same segmented main-fork
layout, tokenizer settings, folds, merges, bitmap scans, custom search/count and
ranking as permanent indexes. Older zero-page indexes retain heap fallback until
REINDEX. Temporary relations use PostgreSQL local buffers. Temporary and unlogged
main-fork writes prepare a complete page image, copy it into the pinned buffer
inside a critical section and mark it dirty; they do not invoke generic WAL.
Permanent index writes retain generic WAL.

`ambuildempty` writes a two-page unlogged init fork: a valid empty meta page and
its empty write-buffer head. It checksums the pages, logs full-page images and
fsyncs the fork. PostgreSQL restores it after an unclean shutdown, matching the
empty reset heap. Subsequent inserts immediately use the index; REINDEX rebuilds
it normally. The init fork must be durable even though main-fork changes are
unlogged. The implementation uses PostgreSQL's smgr, checksum and log_newpage
APIs on both supported majors.

pg_tests exercise local buffers, zero WAL records for temporary inserts, folds,
merges, updates/deletes, bitmap/custom/count/ranked scans and REINDEX. The lifecycle
harness checks immediate shutdown, empty indexed lookup, init-fork size, fresh
inserts and indexed REINDEX with data checksums enabled.

## Standby selective reads

Hot standbys read Stannum indexes selectively, the same segmented path a
primary uses, when the extension is preloaded on the primary and on every
standby that replays its WAL. Without that preload the primary still works and
a standby keeps the heap fallback, which remains correct. The planner and the
executor both ask [`storage::index_reads_allowed`](../../postgres/src/storage/mod.rs)
whether this session may read segments: on a primary always, during recovery
only when the mechanism below is active for the index.

### The two hazards

Generic WAL replays page images and deltas but carries no snapshot
information, so replaying it emits no standby recovery conflict. Two Stannum
operations remove data a standby snapshot may still need:

1. **Page reuse.** A reader copies the segment directory under the shared meta
   lock, releases it, and then reads immutable run pages with only per-page
   content locks. On the primary those pages are held off the free list until
   every snapshot that could reference them is gone (the pending list, keyed by
   transaction id, drained through `GlobalVisCheckRemovableXid`). Replaying the
   generic records that mark the pages free and reuse them gives a standby no
   such protection.
2. **Directory publication that drops visible entries.** A merge or VACUUM
   rewrite publishes a directory without runs whose tuples an old standby
   snapshot can still see, before heap cleanup's own conflict is replayed.

`hot_standby_feedback` does not fix either: it is asynchronous, can disconnect
and can be changed, so it never proves the primary retained this particular
snapshot's xmin. Inspecting the standby's local pending list cannot detect runs
the primary already reused.

### The mechanism: a removal-horizon resource manager

Stannum does what nbtree, GiST and hash do for page deletion and reuse on
standbys: it logs a *removal horizon* with every reclamation and resolves the
conflict on replay before the removal takes effect. Because generic WAL has no
field for a horizon and cannot run extra code on replay, the horizon travels in
a record of a **custom WAL resource manager**
([`storage::wal`](../../postgres/src/storage/wal.rs), registered with
`RegisterCustomRmgr`). Its single `RECLAIM` record names the index's relation
locator and a `snapshotConflictHorizon`: the latest transaction id whose
snapshots could still reference the pages about to be freed.

* **Emission.** `drain_pending` logs a `RECLAIM` record immediately before it
  marks a pending run's pages free (the generic records that actually free and
  later reuse them follow). The horizon is the transaction id stamped on the
  pending entry.
* **The horizon is an upper bound, stamped after publication.** `release`
  reads the next transaction id when it queues a run, but between that read and
  the publication of the directory *without* the run, another transaction can
  take that id and commit ahead of the publication in WAL. A standby snapshot
  taken in that window would have an xmin above the queued id yet still copy the
  old directory. So after the directory is published, `write_meta` re-reads the
  next transaction id and restamps the entries it just released with it. Every
  at-risk reader — one whose snapshot predates the publication — then has an
  xmin strictly below the stamped horizon, exactly nbtree's `safexid`
  discipline for `_bt_log_reuse_page`.
* **Replay.** On redo the resource manager calls
  `ResolveRecoveryConflictWithSnapshot(horizon, isCatalogRel, locator)` before
  the following generic records free the pages, so a conflicting standby query
  is cancelled or waited for exactly as for a heap prune or a btree page reuse.
  `max_standby_streaming_delay` governs the wait and `hot_standby_feedback`
  governs whether a conflict arises at all — both unchanged. A crash-recovering
  primary is never in hot standby, so its redo resolves nothing and needs no
  registered buffers or mask function.
* **Directory publication.** Dropping a visible directory entry always retires
  its runs through the same pending list, so the entry-drop hazard is covered by
  the same `RECLAIM` record that protects the pages; there is no separate path.

The meta page carries a flag (`FLAG_REMOVAL_HORIZONS`) that a writer sets only
while the resource manager is registered and the relation is WAL-logged. A
standby serves segmented reads for an index only when it both replays that
resource manager and finds the flag set, so an index last written by a primary
without the preload keeps the heap fallback until it is rewritten by one that
has it.

### Buffer reads during replay

Segment runs are safe once the reader holds the directory, because their pages
are freed only after a logged conflict. The write buffer is different: replay
applies a writer's buffer pages before its meta page, without the cross-page
meta lock a primary writer holds, so a standby holding an old meta page can find
a buffer page already rewritten from the head by a fold or VACUUM. A standby
`view` therefore records the meta page's LSN and rejects any buffer page written
by a later record, retrying the copy after the matching meta record is replayed
(page *links* are exempt, being set once and never changed). This is a
liveness-only retry; correctness never depends on it.

### After promotion

A snapshot taken during recovery keeps reading the index after the standby is
promoted. Its xmin stays advertised in the procarray for the life of the
transaction, and replay — the only writer that bypasses the meta lock — has
ended, so the primary's ordinary pending-list drain will not free a page the
snapshot still references. No fallback is needed, and none is imposed.

### Safety argument in one line

Every page a standby reader can still reach is freed on the primary only after a
`RECLAIM` record whose horizon covers that reader's snapshot, and replaying that
record cancels or waits for the reader before the free takes effect; buffer
pages are additionally validated against replay. A wrong answer would require
freeing a referenced page without a covering conflict, which the emission point
(before every free) and the after-publication horizon rule together exclude.

### Deployment requirements

- `shared_preload_libraries = 'stannum'` on the primary **and** on every
  standby that replays its WAL. PostgreSQL registers custom resource managers
  only at preload and fails recovery on a record of an unregistered manager, so
  the setting must match cluster-wide before any `RECLAIM` record is written.
- `stannum.wal_rmgr_id` (default 128, the experimental id) selects the manager
  id and must be identical on the primary and all standbys, and unused by other
  extensions. `SELECT stannum.wal_rmgr_id()` returns the registered id, or NULL
  when the extension was not preloaded.
- A primary **without** the preload needs no configuration and stays correct; it
  writes no `RECLAIM` records, and its standbys keep the heap fallback.
- `SELECT stannum.index_reads_allowed('<index>')` reports whether the current
  session would read the index selectively; `stannum.logs_removal_horizons`
  reports whether the last writer logged horizons.

### What the lifecycle harness proves

[`postgres/tests/postings_lifecycle.py`](../../postgres/tests/postings_lifecycle.py)
preloads the extension on the primary (copied into the standby by
`pg_basebackup`) and holds REPEATABLE READ snapshots on the standby while the
primary forces folds, merges, deferred merges, deletes, VACUUM and page reuse.
Each iteration compares the index answer with a heap regex scan **in the same
snapshot**, alternating the custom-scan and bitmap index paths. It runs with
`hot_standby_feedback` off and on, and with `max_standby_streaming_delay` both
`-1` and finite, counting matched answers, recovery cancellations and wrong
answers separately. The contract is **zero wrong answers**: with feedback on or
an infinite delay every snapshot answers, and with a finite delay and no
feedback a conflict may cancel the query instead — correctness never depends on
availability. It also promotes the standby with a recovery-era snapshot still
open and confirms the snapshot keeps reading the index, exercises indexed
scoring on the standby, and crash-recovers the primary so it replays `RECLAIM`
records as no-ops.

## Parallel safety versus splitting one scan

Unordered custom search and count paths can run inside a worker. They are complete
paths with private candidate lists, not partial paths: `parallel_safe=true` permits
worker execution where PostgreSQL allows it, while `parallel_aware=false` and zero
requested workers prevent distributing a full candidate list to multiple workers.
Temporary relations stay in the leader because PostgreSQL local buffers are not
shared. Ranked paths remain parallel-unsafe because score publication is backend
local.

The pg_tests run bitmap and custom paths with `debug_parallel_query=on`. A separate
committed-table lifecycle case asserts an actual launched worker, exact results,
and both custom providers in EXPLAIN ANALYZE. Core worker timing, rows and buffer
instrumentation remain visible; private Stannum counters are explicitly unavailable
for worker execution, rather than showing the idle leader's misleading zeros.
This distinguishes planner flags from real worker execution.

No DSM-shared cursor or `amcanparallel` change is included. Parallel bitmap heap
scans already parallelize heap work after one process builds the bitmap; changing
`amcanparallel` does not make bitmap construction parallel. A partial count needs
shared candidate ownership, a partial aggregate/finalization contract, rescan and
error cleanup, and instrumentation aggregation. Marking today's complete count
path partial would multiply the answer by the number of workers. There is no
validated implementation or measured 100k-corpus benefit to justify that change.

## PostgreSQL references

- [Generic WAL implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/transam/generic_xlog.c): page-image/delta replay, which carries no removal horizon of its own.
- [Custom WAL resource managers](https://www.postgresql.org/docs/18/custom-rmgr.html): `RegisterCustomRmgr`, the preload requirement and the reserved id range.
- [nbtree page reuse](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/nbtree/nbtpage.c): `_bt_log_reuse_page` and the `safexid` computed with `ReadNextFullTransactionId`, the model this design follows.
- [Standby recovery conflicts](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/storage/ipc/standby.c): `ResolveRecoveryConflictWithSnapshot`.
- [Generic WAL documentation](https://www.postgresql.org/docs/18/generic-wal.html).
- [Index construction](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/catalog/index.c): init-fork creation before `ambuildempty`.
- [Replication configuration](https://www.postgresql.org/docs/18/runtime-config-replication.html): feedback timing and standby conflict delays.
- [Parallel plans](https://www.postgresql.org/docs/18/parallel-plans.html): parallel bitmap heap scans and partial-plan requirements.
