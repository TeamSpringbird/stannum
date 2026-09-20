# Spilled merge crash/restart recovery

On 2026-09-20, eight isolated PostgreSQL 18.6 experiments preserved committed
search results after actual process termination during opt-in spilled merging.
Every case removed temporary files on recovery, reclaimed orphan pages through
real VACUUM, and completed a clean retry. This supports the existing recovery
path; no production behavior fix was needed. Output spilling remains disabled
by default.

## What was exercised

The debug `pg_test` extension ran in private socket-only clusters, without
changing an installed extension or using an existing database. Each cluster had
`fsync=on`, `full_page_writes=on`, and `synchronous_commit=on`. Two documents,
each with 20,000 repeated term pairs, were committed and checkpointed. A third
insert forced a merge with a 1 KiB output budget.

A test-only hook paused either after a spilled append or after the first
unpublished output page was written. The driver required positive on-disk
temporary-file bytes before proceeding, and verified the paused PID against
`pg_stat_activity` in that same private cluster.

The driver then either:

- Sent SIGKILL to that backend, allowing the postmaster to terminate other
  scratch backends and perform automatic crash recovery.
- Immediately stopped the entire scratch server with `pg_ctl -m immediate`,
  then explicitly started it again. This uses immediate termination, not a
  graceful transaction rollback.

Both modes ran once per checkpoint with ordinary WAL behavior and once with a
**test-only `XLogFlush` at the pause**. The latter deliberately makes unpublished
page WAL durable; it does not imply that production merging flushes at that
point. Server logs verify termination and actual WAL redo.

## Results

| WAL at pause | Termination | Phase | Temp bytes on disk | Orphans recovered | Result |
|---|---|---|---:|---:|---|
| Ordinary | Backend SIGKILL | Spilled append | 16,384 | 6 | Pass |
| Ordinary | Backend SIGKILL | First output page | 80,019 | 7 | Pass |
| Ordinary | Immediate server stop | Spilled append | 16,384 | 6 | Pass |
| Ordinary | Immediate server stop | First output page | 80,019 | 7 | Pass |
| Explicit flush | Backend SIGKILL | Spilled append | 16,384 | 6 | Pass |
| Explicit flush | Backend SIGKILL | First output page | 80,019 | 7 | Pass |
| Explicit flush | Immediate server stop | Spilled append | 16,384 | 6 | Pass |
| Explicit flush | Immediate server stop | First output page | 80,019 | 7 | Pass |

After recovery, committed IDs remained `[1, 2]`; the interrupted ID 3 was absent.
Four search shapes (term, disjunction, ordered proximity, and an absent term)
matched forced sequential scans exactly. Retained EXPLAIN plans confirm the
other path used `docs_idx`. Temporary files were gone. Real
`VACUUM (INDEX_CLEANUP ON)` eliminated all verifier findings, and retrying the
insert produced `[1, 2, 3]` with a clean index and no leaked temporary files.

The ordinary-WAL runs left extended but unwritten orphan pages reported as
`unreferenced and unreadable: not a Stannum LDP2 page`. Explicitly flushed WAL
recovered decoded run pages reported as
`run page referenced by nothing; VACUUM reclaims it`. Both were reclaimed. The
harness permits only these precise warning forms, not arbitrary corruption.

An initial harness attempt paused before the triggering term was appended and
correctly failed the positive-disk-byte check. The retained failed attempts also
include a private extension-path setup correction and the overly narrow first
orphan-warning assertion. They are harness development evidence, not passing
crash cases or production defects.

## Reproduction and evidence

Build the private package while holding `/tmp/stannum-pgrx.lock`:

```sh
cargo pgrx package --debug --test --package stannum \
  --features 'pg18 pg_test' --out-dir /absolute/path/spill-crash-package
```

The driver acquires that shared lock itself for all cluster lifetimes:

```sh
python3 benchmarks/spill_crash_recovery.py \
  --package-dir /absolute/path/spill-crash-package \
  --pg-bindir /absolute/path/postgresql18/bin \
  --output benchmarks/results/spill-crash-ordinary
python3 benchmarks/spill_crash_recovery.py \
  --package-dir /absolute/path/spill-crash-package \
  --pg-bindir /absolute/path/postgresql18/bin \
  --output benchmarks/results/spill-crash-flushed --flush-wal
```

Output directories must be new. All logs, SQL plans/results, server settings,
library/SQL hashes, and per-case receipts are retained. Successful clusters are
removed only after confirmed shutdown; failed cluster directories are retained.
The harness requires PostgreSQL 18's private extension search paths.

Committed summary: [spill-crash-recovery-results.json](spill-crash-recovery-results.json).
Local full evidence: `benchmarks/results/spill-crash-recovery-evidence.tar.gz`,
SHA-256 `51b9767aa1a1f2867cd37ff508d7d5637ee677e07f76d59d41a58be84c906d17`.
The archive includes the exact added Rust patch on base
`f76f135b93651b1cd1469757d222604204be4cc1`, frozen driver, package/source receipts,
and final and failed-development logs. Raw evidence is ignored by git; the
archive does not include the package binary or retained failed data directories.

Validation also passed seven PostgreSQL spill tests, all-target Stannum Clippy
with warnings denied, and three Python process-target/quoting checks.

## Remaining gaps

This is a tiny deterministic debug-build experiment on ARM macOS, not a
performance result or proof against arbitrary crashes. It does not emulate
power loss, torn storage writes, disk-full recovery, multiple temporary
tablespaces, replicas, or every metadata/publication boundary. Linux and PG17
crash campaigns remain untested; this driver specifically uses PG18 settings.
It adds no streaming-input coverage and does not enable spilled VACUUM merging.
Larger concurrent crash workloads and fault cases around directory publication
remain useful follow-ups before considering default enablement.
