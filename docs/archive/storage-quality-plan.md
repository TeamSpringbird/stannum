# Storage quality and incremental implementation

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../../README.md) and [BENCHMARKS](../benchmarks/README.md).

Two agents reviewed the first durable-posting slice on the shared branch. The
requested Fable 5.1 model was unavailable; both used the session's inherited model.
Their assignments are complementary and file ownership is explicit:

| Responsibility | Owned implementation | Review |
| --- | --- | --- |
| Storage interfaces and checked page format | `postgres/src/postings.rs`, private `postings/page.rs` | Independent safety agent |
| Scan ownership, snapshot policy, VACUUM statistics | `postgres/src/am.rs` | Independent safety agent |
| Lifecycle validation and integration | Root agent, `postgres/tests/postings_lifecycle.py` | Results reviewed centrally |

The safety review distinguished demonstrated code defects from prospective risks:

* A Rust allocation freed only by `amendscan` lacks PostgreSQL ERROR/cancellation
  cleanup. Its owner must be tied to the scan's PostgreSQL memory context, with
  normal teardown taking the inner allocation and reset reclaiming any remainder.
* LDP1 requires complete page-header bounds. Validating only `pd_lower` permits
  malformed `pd_upper`/`pd_special` to reach Generic WAL modification. Invalid
  pages must fail before publication, not inside a WAL critical section.
* Returning physical term-posting counts as indexed-row counts changes statistics
  units. VACUUM cleanup should expose an explicitly marked heap-row estimate.
* Raw page slices with caller-selected lifetimes make future misuse easy. Views
  should borrow their actual buffer owner or be scoped to a WAL edit operation.
* Tail termination, page roles, TIDs, and link/bucket invariants need checked access.

No normal-operation aliasing, primary snapshot/TID-reuse, WAL publication or
lock-order defect was demonstrated by the review. That is not proof of absence.
The snapshot-origin recovery guard is conservative hardening; the possible
promotion/WAL-order edge was not reproduced as a false-negative defect.

## Review and test gates

Preserve LDP1 bytes and query results during this refactor. Test valid and malformed
pages through a pure byte-codec interface, including capacity boundaries. Verify
memory-context reset and normal-end ownership without double destruction. Repeat
the PostgreSQL lifecycle suite, including cancellation followed by backend reuse,
and old recovery snapshots across promotion. Run strict workspace Clippy.

Compile and run extension/server tests in a recorded gap between baseline trials.
Never replace the benchmark's frozen image, runner or corpus. Do not drop failures
or shorten in-flight measured traffic to make development convenient.

## Subsequent slices

1. Connect persisted AND/OR and conservative phrase candidates. Persisted postings
   are insertion-ordered and can repeat TIDs; streaming set iterators require
   sorted, unique identities. Use PostgreSQL bitmap composition or explicitly
   normalize identities. Do not connect these representations without a contract.
2. Improve build/write batching and reuse space reclaimed by VACUUM. Add maintenance
   pacing and study whole-bucket lock contention. Preserve crash-safe publication.
3. Persist document lengths/frequencies for ranking, with explicit visibility and
   scoring-statistics semantics. Avoid reconstructing the entire corpus per query.
4. Compare each complete slice against frozen baseline cohorts using a separately
   identified fork build. Report correctness, tail latency, achieved writes, index
   size and failures alongside throughput.

Keep each slice independently reviewable. Safety fixes precede new query features;
new storage abstractions should hide a concrete invariant rather than predict every
future backend.

## Completed first review round

Both agents completed their assignments and the safety agent reviewed the integrated
patch. The root agent ran 40 extension tests, the full private-cluster lifecycle
suite, and strict workspace Clippy; all passed. Lifecycle coverage includes query
cancellation with backend reuse and a recovery snapshot retained across promotion.
The frozen benchmark launcher was resumed after these checks.

The next implementation slice remains persisted Boolean/phrase candidates. The
quality refactor intentionally changes neither disk format nor query results.
Insertion overhead from validating page entries is recorded as a measurement
follow-up, not treated as a performance win.

## Persisted Boolean/phrase follow-up

The next slice is implemented in `am.rs`: a bounded candidate plan composes native
PostgreSQL bitmaps over persisted term lookups. This avoids assuming that append-order
postings are sorted. Unsupported expressions retain conservative fallback behavior;
phrase candidates retain exact heap rechecks. Scratch contexts and owned bitmap
guards keep allocation lifetimes explicit.

The implementation agent and an independent safety reviewer worked on this slice.
All 43 PG18 tests and strict workspace Clippy pass, including low-work_mem lossy
composition, reversed operands, negative positional expressions and rescans. The
private-cluster lifecycle suite also passes after this change. Paired release-build
measurements are recorded separately from correctness tests.
