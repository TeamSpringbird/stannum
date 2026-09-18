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

## Why standby selective reads remain disabled

A recovery snapshot still uses heap fallback, even after promotion. New snapshots
on the promoted primary use segmented reads normally. The shared planner check
also selects heap-based scoring during recovery and for recovery-origin snapshots;
previously ranking could select the indexed scorer despite the scan fallback.
The heap scorer also uses a read-only SPI catalog lookup: pgrx's convenience
`Spi::get_two` opens a mutable connection and requests an XID, which recovery
cannot assign.

Two independent hazards prevent safely removing the guard:

1. A reader copies a segment directory, then reads immutable runs after releasing
   the meta-page lock. Primary reclamation may reuse those pages while a standby
   snapshot still references the old directory. Generic WAL replay does not emit
   a snapshot-removal conflict for Stannum's pending-run records.
2. Index VACUUM can publish a directory omitting tuples still visible to an old
   standby snapshot before heap cleanup's WAL conflict is replayed. Merely
   preventing page reuse does not restore these missing index candidates.

`GlobalVisCheckRemovableXid` on the primary honors feedback/slot horizons, but
`hot_standby_feedback=on` on the replica is not an acknowledgement that the
primary received and retained this particular snapshot's xmin. Feedback is
asynchronous, can disconnect, and can be changed. Looking only at the standby's
pending list cannot detect already reclaimed runs. Enabling index reads based
only on that setting would turn a timing race into incorrect results.

There is therefore no currently implemented runtime condition that proves a
recovery snapshot safe for selective index reads. This change deliberately
retains the fallback instead of claiming first-class standby index execution.
A future design needs WAL records carrying removal horizons, replay-side snapshot
conflict handling *before* publishing removals/reusing pages, and tests for feedback
loss, reconnect, transaction-ID wraparound and promotion. A custom resource manager
would also change deployment/preload requirements. An acknowledged feedback lease
could be an alternative, but a local GUC is insufficient.

The lifecycle harness holds repeatable-read standby snapshots while the primary
forces folds, merges, updates and VACUUM, with feedback off and on. It checks 320
answers, observes feedback xmin on the primary, and verifies post-promotion
behavior. `max_standby_streaming_delay=-1` allows ordinary heap recovery conflicts
to wait for the test snapshot instead of cancelling it. With a finite production
delay PostgreSQL may legitimately cancel a conflicting standby query; correctness
does not imply guaranteed availability. Ranked standby queries are checked to use
the heap scorer.

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

- [Generic WAL implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/transam/generic_xlog.c): page-image/delta replay, without Stannum removal horizons.
- [Generic WAL documentation](https://www.postgresql.org/docs/18/generic-wal.html).
- [Index construction](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/catalog/index.c): init-fork creation before `ambuildempty`.
- [Replication configuration](https://www.postgresql.org/docs/18/runtime-config-replication.html): feedback timing and standby conflict delays.
- [Parallel plans](https://www.postgresql.org/docs/18/parallel-plans.html): parallel bitmap heap scans and partial-plan requirements.
