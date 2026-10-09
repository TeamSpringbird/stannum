# Background maintenance workers

A fold turns the write buffer into a segment, and the segments it adds must
later be merged, rewritten when they are mostly dead, and their retired runs
freed. Without workers the inserting session that folded does that merge
after publishing (an insert's *deferred merge*, see
[segmented storage](segmented-storage.md#merge-policy)), and VACUUM does the
rest. TIN does this in a cluster-wide background worker instead, so a merge
never runs in a writing session unless the worker pool is exhausted. With
`shared_preload_libraries = 'stannum'`, Stannum does the same.

The code is in `postgres/src/maintenance/`: `queue.rs` (the shared-memory
queue, plain data and logic), `worker.rs` (the launcher and the worker),
`ops.rs` (the operation interface, below) and `sql.rs` (the SQL functions).

## Worker model

- **The launcher** is a static background worker, registered by `_PG_init`
  while the library is preloaded, with start time `RecoveryFinished`. Like
  PostgreSQL's logical replication launcher it connects to no database (only
  shared catalogs), so it shows in `pg_stat_activity` as `stannum maintenance
  launcher` and receives the postmaster's notices. It sleeps on its latch. A
  backend that queues a job sets the latch; so does a worker that exits, and
  the postmaster's notice that a worker started or stopped. When no worker is
  running and a job is queued, the launcher registers a dynamic worker
  (`stannum maintenance worker`) for the database of the oldest queued job,
  preferring a database other than the one it served last.
- **A worker** connects to that database, attaches to the cluster's one
  worker slot, and takes the database's jobs oldest first, each in its own
  transaction. It detaches and exits when its database has no queued job, or
  when it has finished `stannum.maintenance_jobs_per_db` jobs while another
  database has queued work; detaching and finding no job happen in one
  critical section, so a job queued after it wakes the launcher, which then
  sees no worker and starts one. A background worker binds to one database
  for life, which is why there is a worker per database rather than one that
  moves. One worker runs at a time, as in TIN's description of "a
  cluster-wide maintenance worker"; merges of one index are serialized by
  its maintenance lock in any case.
- **The queue** lives in shared memory under one LWLock: 256 job slots keyed
  by (database, index OID), each with a set of kinds (merge, rewrite,
  reclaim, and a reserved promote), the worker slot, and counters. A request
  for an index that already has a queued job joins it; a job a worker is
  running does not absorb a new request, because the work it asks for may
  postdate what the worker read. The queue holds hints, not state: the
  segment directory records what is due, so losing the queue (a crash, a
  restart, an abandoned job) loses nothing but a wakeup, and the next fold
  queues the index again.

## How backends hand work over

`storage::insert` asks `maintenance::plan` once per fold, under the meta
lock it already holds for the fold (lock order: meta page, then the queue's
LWLock; no queue holder takes a buffer lock):

| `stannum.index_maintenance_mode` | workers can take it | the fold's budgeted merges | after publication |
| --- | --- | --- | --- |
| `background` (default) | yes | none | queue merge, rewrite and reclaim for a worker |
| `background` | no | up to `stannum.max_merge_docs` | the deferred merge, inline |
| `foreground` | either | up to `stannum.max_merge_docs` | the deferred merge, inline |
| `manual` | either | none | nothing |

"Workers can take it" means the library is preloaded, the launcher is
running, its last worker registration succeeded, and the index is not a
temporary relation (another session's temporary relations are invisible to a
worker). Queuing takes the LWLock for a scan of the slots and sets the
launcher's latch; it never waits for a worker. If the queue is full the
backend runs the deferred merge inline, as without workers, and counts an
inline fallback.

`stannum.maintenance_status()` reports the launcher, the worker, the queue
length and the counters (requests, completed and abandoned jobs, inline
fallbacks, launches and refused registrations); `stannum.maintenance_jobs()`
lists the queued and running jobs.

## What a job does

`ops::run_job` takes the locks an insert takes, the table and then the index
in `RowExclusiveLock`, conditionally: behind DDL the job is dropped rather
than queued behind an `AccessExclusiveLock`, since the next fold queues it
again. It skips an index that was dropped, is not a `stannum` index, or is
not yet ready for inserts. Then the format's pass runs what the kinds ask:

- **reclaim**: free retired runs no snapshot can still read (VACUUM's
  `reclaim_pending`);
- **merge**: the lowest full size tier, else the smallest entries while the
  directory holds more than its target (`target_segment_count`, else
  `stannum.max_segments`), within the merge input cap; the budget that bounds
  an insert's merges does not apply;
- **rewrite**: segments whose dead fraction reached `dead_percent_threshold`.

These are VACUUM's cleanup jobs. VACUUM still runs its own: it is
PostgreSQL's maintenance scheduler for a table, a manual `VACUUM` is a
request to do that work now, and tests and operators rely on it finishing
the job synchronously. A worker's merge and VACUUM's can run concurrently;
publication keeps them from both applying.

## Locking and publication

A worker publishes exactly as VACUUM's unlocked merges do. It captures the
directory under a shared meta lock, reads its inputs and builds the merged
segment with no meta lock, writes the run, and takes the meta lock
exclusively only to publish, after matching every input against the
directory entry for entry; otherwise it discards the run and retries
against the new directory. Unlike VACUUM, which runs its orphan reclamation
in the same backend after its merges, a worker holds the index's
maintenance lock from before it writes a run until it has published or
discarded it, as an insert's deferred merge does, so a concurrent VACUUM's
orphan pass never frees a run before its publication. An insert that must
make room in a full 96-entry directory waits for that lock, so it can wait
for a worker's merge; with a worker keeping the directory near its target,
that bound is rarely reached.

The crash-safety rule is unchanged: nothing the meta page references
changes before publication. A worker adds no publication point of its own,
only a new process that reaches the existing one; the crash test kills a
worker between writing its run and publishing it.

## Fairness

`stannum.maintenance_jobs_per_db` (default 0, `PGC_SIGHUP`; a session `SET`
fails with 55P02) is TIN's setting. At zero a worker drains its database
before the launcher serves another; above zero a worker that has finished
that many jobs exits when another database has queued work, and the
launcher prefers a database other than the last one served. A job is one
pass over one index, not one merge.

## Crashes, errors and restarts

- A worker's error ends the worker (a background worker has no outer error
  handler). Its exit callback frees the worker slot and abandons its job;
  the transaction's abort releases its locks; pages it wrote and did not
  publish are orphans that the next VACUUM reclaims. Abandoning rather than
  retrying keeps an index that fails every pass (corruption, say) from
  restarting a worker forever; the next fold queues it again.
- A worker that exits before attaching (its database was dropped or no
  longer accepts connections) is noticed by the launcher, which drops that
  database's jobs.
- `SIGTERM` (`pg_terminate_backend`, shutdown) terminates a worker at its
  next interrupt check, as it does a backend: merge construction checks at
  every checkpoint.
- A crash reinitializes shared memory, so the queue starts empty; the
  launcher restarts after recovery (its restart interval is five seconds,
  after an error too).
- While a worker is connected to a database, `DROP DATABASE` and
  `CREATE DATABASE ... TEMPLATE` of it fail as with any other session; the
  worker exits once that database has no queued job.

## Standbys

The launcher starts only when recovery has finished, so a hot standby runs
no launcher and no worker; a standby takes no writes, so nothing would queue
a job. On promotion the launcher starts.

## Without shared_preload_libraries

There is no queue and no launcher, `plan` answers `Inline` in background
mode, and every backend maintains inline exactly as before, without a NOTICE
or any other message. The settings exist either way:
`stannum.maintenance_jobs_per_db` is defined whether or not the library is
preloaded, so a session `SET` of it is refused in every configuration.

## What still runs in the writing session

- **The fold.** The format has one write buffer, and an insert that finds it
  full must fold it before appending, so the fold (TIN's sealing and
  promotion together) runs under the meta lock in the writing session. A
  format with a sealed write segment can seal inline and queue a promote job
  (the reserved `promote` kind).
- **Making room in a full directory.** An insert into a directory at its
  96-entry on-disk bound must merge before it can fold, whatever the mode.
- **Everything, when workers cannot take it**: without preload, when the
  pool has no free slot for a worker (`max_worker_processes`), or when the
  queue is full. TIN documents the same fallback.

## SQL functions

`stannum.promote(index regclass, extent_cap_bytes bigint DEFAULT NULL)` and
`stannum.merge(index regclass, target_segment_count integer,
high_water_multiplier integer, max_fan_in integer, force boolean)` have TIN's
arguments and result columns and run in the calling session, under the same
locks and publication as a worker's job. Both need the `MAINTAIN` privilege
on the table (owners and `pg_maintain` have it).

- `promote` folds the write buffer into a segment now, whatever its size,
  and merges nothing: `consumed_controls` and `linked_segments` are 1 when it
  folded and 0 for an empty buffer, `docs_promoted` the documents folded,
  `terms_added` the new segment's distinct terms. `extent_cap_bytes` must be
  positive (XX000, TIN's message) and otherwise has no effect: Stannum writes
  one segment per fold. Unlike TIN's, it folds a buffer below the sealing
  threshold, and it does not change which terms scoring elides (TIN counts
  only promoted documents there; conformance case catalog.S-07).
- `merge` merges the smallest segments, at most `max_fan_in` at a time
  (default: no limit) and within the merge input cap, until the directory
  holds `target_segment_count` (default: the index's target). Unless `force`,
  it merges only when the directory holds more than `target_segment_count *
  high_water_multiplier` (default 1) segments. `considered_segments` is the
  directory size it started from, `merged_segments` the inputs merged into a
  successor, `retired_only_segments` inputs whose every document was dead,
  `linked_segments` the segments published, `output_docs` the documents and
  `output_postings` the postings (one per term and document) the merges
  wrote (a document merged twice counts twice), `replayed_kills` the dead
  documents dropped, and `no_op_reason` why nothing was merged, when nothing
  was.

`stannum.segment_info` adds TIN's `npostings` (NULL: TIN counts one posting
per term and document, which STN3 does not record and counting would read
every dictionary), `source_state` (`current`: retired
runs wait on the pending list and are not listed), `origin` (NULL: the
directory does not record how a segment was made) and `sequence` (the
segment's generation) after its own eight columns.

## The operation interface

`postgres/src/maintenance/ops.rs` is the seam between scheduling and the
storage format. The queue, the workers and the SQL functions know only its
types and three functions, each of which calls the format:

| function | today (`storage`) | a format provides |
| --- | --- | --- |
| `promote(index) -> FoldReport` | `fold_buffer`: fold the write buffer | seal the write segment, if any, and promote what is sealed |
| `merge(index, MergeRequest) -> MergeReport` | `merge_toward`: smallest entries down to the target | its merge policy toward the target |
| `run_job` → the format's pass, `PassRequest -> PassReport` | `maintenance_pass`: reclaim, tier merges, rewrites | the same kinds, plus `promote` once it queues it |

The contract, stated in the module: the caller holds the table and index in
`RowExclusiveLock` inside a transaction and no buffer lock; long work runs
without the metadata lock and checks for interrupts; nothing the published
metadata references changes before publication, and pages of an unpublished
result are written under the maintenance lock; publication rechecks inputs,
so jobs run safely beside inserts, VACUUM and each other. A new format also
answers `storage::present(index)` (whether the index has storage to
maintain) and, in its insert path, calls `maintenance::plan` once per seal
and `maintenance::request(index, kinds)` after publishing.

## Tests

- `postgres/src/maintenance/queue.rs`: deduplication, a full queue, which job
  a worker takes next, yielding after `maintenance_jobs_per_db`, the
  launcher's choice of database, abandoned jobs (`#[test]`, run by
  `cargo pgrx test`).
- `postgres/src/maintenance_tests.rs` (pg_tests, no preload): the SQL
  signatures, promote and merge results and validation, privileges, the
  settings, and the three modes without workers.
- `postgres/tests/maintenance_workers.py` (private cluster, preloaded): the
  worker merges after folds, manual mode queues nothing, two databases are
  served, an exhausted pool falls back to inline merges, a standby runs no
  launcher. Every wait polls for a state with a deadline.
- `postgres/tests/maintenance_worker_crash.py` (pg_test build, preloaded):
  a worker that crashes the server between writing its merged run and
  publishing it, and one terminated at a merge checkpoint, leave consistent
  indexes; `stannum.debug_maintenance_race`, a reload setting of pg_test
  builds, places the crash.
