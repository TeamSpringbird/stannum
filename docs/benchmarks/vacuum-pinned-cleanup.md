# Reader-pinned heap pages and VACUUM accounting

PR #23 exposed an intermittent failure in the existing VACUUM workload smoke on
ARM64/PostgreSQL 17: after deleting 75% of 32,768 rows, the index contained 8,199
physical documents instead of the 8,192 visible rows. Search/ranking checks had
passed. This was an invalid benchmark assertion, not evidence of a merge losing
or resurrecting rows.

## Reproduction and cause

Twenty repetitions of the original workload on local PG18 passed. Holding a
primary-key cursor on the last, partially filled heap page reproduced the exact
seven-entry excess on both PG17 and PG18. The cursor starts after the delete,
so its snapshot does not protect the deleted rows. VACUUM's verbose output says:

> tuples missed: 7 dead from 1 pages not removed due to cleanup lock contention

PostgreSQL can skip pruning when it cannot immediately obtain the heap buffer
cleanup lock. Those tuples are not supplied as removable CTIDs to the index's
bulk-delete callback. This behavior is described in the PostgreSQL
[`lazy_scan_noprune` implementation](https://github.com/postgres/postgres/blob/REL_17_STABLE/src/backend/access/heap/vacuumlazy.c).
The index correctly retains those entries and heap visibility excludes them
from query results.

The reproduction reduces to 32 rows with eight live rows. The first VACUUM
produces one segment with 15 documents and zero known dead documents. Releasing
the cursor and vacuuming again produces 15 documents, seven marked dead. A
sparse dead list need not trigger a physical segment rewrite. The required
quiescent invariant is therefore `documents - dead_documents = live_rows`, not
`documents = live_rows` with no tombstones.

## Correction and validation

`vacuum_cleanup.py` retains concurrent-reader checks and deep verification. It
saves the concurrent layout before assertions and records verbose VACUUM output.
After all readers exit, a separately reported cleanup pass establishes exact
live-entry accounting, checks search results and deep verification, and repeats
the stable ranked proof. Cleanup is excluded from the measured VACUUM duration,
WAL, RSS and reader window; it does not establish that cleanup kept up with load.
Both VACUUM passes now retain verbose diagnostics, so timings should be compared
only with runs using the same harness revision.

`benchmarks/vacuum_cleanup_pins.py` is a real PostgreSQL regression added to every
PG17/18 × ARM/x86 CI job. It holds the cursor until VACUUM reaches the buffer-pin
wait, then closes it normally. There are no fixed race sleeps or backend kills.
The fixture must demonstrate deferred pruning and retained sparse tombstones;
search results and deep verification must pass both before and after cleanup.
The old physical-count assertion failed this minimized case; the corrected
live-count assertion passes. Unit counterexamples still reject missing live
entries, unaccounted extra entries and invalid dead counts.

Local validation: the minimized regression passed on PG17 and PG18; all 88
benchmark unit tests and the source-header check passed. All 12 full-workload replays passed with direct and reconstruction strategies,
three repetitions per strategy and PostgreSQL version. Raw artifacts are retained locally under
`benchmarks/results/vacuum-pin-fixed-pg17` and `vacuum-pin-fixed-pg18`.

No runtime index, merge, storage-format or locking code changed. The prevention
lesson is to distinguish physical entries, known-dead entries and visible heap
rows, and to state whether a cleanup assertion applies during readers or after
quiescence.
