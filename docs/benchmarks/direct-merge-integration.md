# Foreground direct-merge integration

Foreground merges call `segment::merge::merge` while retaining the existing
exclusive metadata lock. Tier selection, merge budgets, generation allocation,
LSG3 output, WAL publication and retirement remain unchanged. VACUUM retains its
separate unlocked reconstruction/revalidation path. This does not include the
unlocked-fold experiment in PR #10.

The new path loads each selected source blob and dead set, verifies all sources,
and traverses their sorted dictionaries and postings. Those owned inputs are
released before output-run allocation. Aggregate encoded bytes and document
counts must fit `u32::MAX`; otherwise the existing per-source reconstruction path
runs. This fallback preserves merges whose aggregate input exceeds one run's
format limit but whose live output still fits. The admission limits are format
bounds, not a peak-memory budget. Existing infallible codec allocations remain;
process-wide OOM is not recoverable through this API.

## Cancellation and failure recovery

PostgreSQL's buffer content LWLock holds off interrupts. The merge callback
checks for interrupts but respects that deferral. Insert checks once more after
metadata publication and lock release. A pending cancellation can therefore
complete merge construction and publication before aborting the SQL statement.
This does not promise bounded cancellation latency.

Two PostgreSQL regression tests exercise distinct cases:

- An injected ERROR after merge construction, before publication, leaves the
  directory and visible results intact. A retry succeeds. Unpublished fold pages
  are the only verifier findings, and existing VACUUM cleanup reclaims them.
- A pending query cancellation at a merge checkpoint is deferred under the lock,
  then delivered after unlock. Construction completes; the statement rolls back
  and the next insert succeeds with correct visible results and a clean index.

The codec API independently tests cooperative cancellation at every exposed
checkpoint. Existing lifecycle tests cover dead lists, CTID reuse, ranked
queries, crash recovery and standby WAL replay.

## Measurement protocol

Compare a release build of main `26b9d1e` against this integration, using retained
library copies and SHA-256 identities. Run the existing contention harness with
20-second windows, two writers, two readers, 32-document buffers and a 1,024-document
merge budget. Use three alternating pairs each for short and long documents
(repetition counts 20 and 200). The private PostgreSQL 18 cluster uses 512 MB
shared buffers, a 30-minute checkpoint timeout and 16 GB maximum WAL size, with
an explicit checkpoint before each harness invocation. Retain the server log to
check for checkpoints during traffic.

The memory probe runs each method in a separate release process, with fixture
construction in a prior process. `/usr/bin/time -l` measures peak resident memory
on macOS. Each fixture contains eight segments with interleaved TIDs and no dead
documents. Reference reconstruction retains one encoded source at a time;
direct merging retains all encoded sources. Parsing, file I/O and output
construction are timed, with output-file writing excluded from the timer but
included in process RSS. Every output must match byte for byte. These are
isolated codec-process measurements, not PostgreSQL backend peak-memory figures.

```sh
cargo build --release -p segment --example merge_memory
target/release/examples/merge_memory prepare /tmp/merge-fixture 512 400 400
/usr/bin/time -l target/release/examples/merge_memory reference /tmp/merge-fixture /tmp/reference.segment
/usr/bin/time -l target/release/examples/merge_memory direct /tmp/merge-fixture /tmp/direct.segment
cmp /tmp/reference.segment /tmp/direct.segment
```

Validation and measurement are in progress; this integration is not yet approved
for promotion based on runtime performance.
