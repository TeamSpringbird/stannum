# VACUUM workload coverage and fixed offered load

`paired_libraries.py --workload vacuum` now exposes deletion density, term
vocabulary/distribution, query mix, reader concurrency, duration, and offered
rate. It continues to compare retained release libraries against fresh,
identical fixtures in alternating order. Defaults retain the lightweight
32,768-document selective count probe; broader coverage is opt-in.

## Controlled workload dimensions

- `--scenario merge|rewrite|mixed`: eight live segments, one deletion rewrite,
  or eight segments with deletions followed by cleanup.
- `--delete-percent 0..99`: applies to rewrite/mixed. The existing 75% fixture
  preserves `id % 4 <> 0`; other values delete `id % 100 < percent`. Reported
  live count is measured, not rounded percentage arithmetic. Small incomplete
  hundred-document groups can differ slightly from the requested percentage.
  Single-source rewrite requires at least 50% dead to trigger the maintenance
  policy. All-dead cleanup is covered by runtime correctness tests, not this
  timing harness.
- `--docs`: positive multiple of eight; `--repeat`: document length (20 and 200
  are useful short/long comparisons).
- `--vocabulary`: at least eight, default 97. Each document gets a selective
  `wN` term. `--distribution hot` assigns 80% of documents to w0; remaining
  documents retain their vocabulary term. Both variants include common terms,
  the phrase `common filler`, and a unique hash. `dataset.json` records live
  document/body byte counts and minimum/maximum body bytes. The live fixture
  must contain at least one w7 match; empty selective fixtures fail before
  reader traffic. For example, hot distribution with a vocabulary divisible
  by five cannot generate w7, and deletion may remove otherwise valid matches.
- `--query-shapes mixed`: equally weighted broad count, selective full ID array,
  phrase count, and ranked top ten. Raw pgbench output and per-query results
  record realized counts/shares; probabilistic selection need not yield 25%
  each. `selective` retains the original cheap constant count assertion.
- `--reader-rate`: total offered transactions/second across `--readers`
  clients, using pgbench's seeded Poisson schedule. With no rate, traffic is
  closed loop. `--reader-seconds` defaults to four.

The paired driver now **stops the postmaster and all children before each
library replacement**, restarts it for that trial, and restores the original
library after confirmed shutdown. If shutdown fails, it retains the cluster
and original-library backup and leaves the mapped library in place. The older driver swapped libraries between workloads
while the server was running; the earlier report's statement that each swap
already stopped the server was incorrect. A lifecycle-ordering regression test
now enforces stopped-server replacements. Restarts also reset shared buffers;
comparisons must use this same protocol on both builds.

## Correctness and interpretation

No logical writes occur during a VACUUM trial. Broad/phrase counts and selective
ID arrays in the mixed workload are checked against arithmetic heap predicates
inside the same SQL snapshot. Final selective membership uses `EXCEPT ALL` in
both directions, layout accounting checks all remaining documents/dead entries,
and structural verification must produce no findings.

Ranked transactions use REPEATABLE READ for logical heap visibility. VACUUM can
still change physical index statistics used for BM25 inside that transaction:
the snapshot alone does not freeze scores. With custom scans disabled, they
materialize every match and score, then derive the exhaustive top ten. Custom
scans are then enabled for the measured top-k path. Its score multiset must
match the reference, it may not repeat IDs, and every returned ID must carry
its exhaustive-reference score. The reference includes an ID-to-score map of
all matches at or above the tenth result’s score, so equal-score boundary ties
may select different eligible IDs. Incorrect IDs carrying otherwise correct
scores cannot pass. A large boundary tie expands this map and adds oracle
overhead, which is included in transaction latency. Before mixed traffic, an
SQL self-check exercises the exact guard with a valid alternate boundary tie,
an incorrect ID carrying the correct score, swapped scores, duplicate IDs, and
a missing result. All fixture documents match the broad ranked OR query.
EXPLAIN must show score-descending order and `Top K: 10`; an ordinary full scan
and sort does not satisfy this check. The oracle shares the scorer and index
with the optimized path, so it tests execution/pruning equivalence, **not an
independent BM25 implementation**.

Separate SQL statements bracket the reference and candidate with the complete
`segment_info` directory, ordered by ordinal: kind, root block, document/dead
counts, summed document lengths, pages, and generation. The initial fingerprint
finishes before the reference starts; the final fingerprint starts after the
candidate finishes. This avoids SQL expression evaluation-order assumptions.
For this immutable-only fixture with no writers or DDL, generations cannot repeat
and dead sets only grow, so matching fingerprints establish a stable scoring
state. This is not a general guard for concurrent inserts, REINDEX, or DDL.

An unchanged fingerprint requires full ranked identity/score equality. A changed
fingerprint invalidates that score comparison; duplicate IDs still fail. Each
completed ranked transaction emits exactly one stable or invalidated shell
marker, and those counts must equal pgbench's ranked completion count. Results
report invalidations separately, never as successful correctness comparisons.
At least one stable timed comparison and one stable standalone comparison both
before and after maintenance are required. Traffic-wide stable counts cannot
establish phase-specific ranked correctness coverage. Fingerprint reads and
one shell marker per ranked transaction add symmetric overhead. Oracle execution
is included in transaction
latency; mixed-workload timings must not be presented as query-only latency.

Full-window logs include transactions crossing VACUUM boundaries. The phase
subset includes only executions wholly inside VACUUM, computing actual execution
start after schedule lag. It can contain few or no samples for an individual
shape; per-shape p99 remains absent below 1,000 samples. A short phase is not
sufficient evidence of tail improvement.

`offered_load` records nominal offered rate × duration, logged schedules,
completed/failed/skipped counts, lag p95/max, transactions with over 1 ms lag,
and scheduled-latency p95 in the first/last scheduled deciles. Rate-limited
pgbench latency includes schedule lag. Lag describes client queue accumulation,
not server queue depth; increasing last-decile latency can reveal an overloaded
client/server combination. Seeded Poisson traffic is not an exact request count;
arrivals never emitted at shutdown are not in logs. No latency limit is used,
so the harness does not intentionally skip queued requests. Any logged failure
or skip fails the correctness gate. Phase-load metrics are conditional on the
contained subset and must not be interpreted as arrival accounting for that
interval.

VACUUM wall time, WAL bytes, and backend RSS samples are retained. RSS includes
shared mappings; 20 ms sampling can miss peaks and can yield only a few samples
for short operations. It is not allocator accounting or a reliable process high
water mark. There is one maintenance operation per trial; this measures client
queue pressure around cleanup, **not a sustained writer/maintenance backlog**.
The existing contention harness covers concurrent writers separately.

## Bounded campaign

Use the installation lock around each campaign. Both retained binaries must
match the installed extension SQL. For example, in the repository root:

```sh
python3 /tmp/stannum-pgrx-lock.py python3 benchmarks/paired_libraries.py \
  --baseline /absolute/baseline-library --integrated /absolute/candidate-library \
  --installed-library /absolute/installed-library --checkpoint-control \
  --workload vacuum --scenario mixed --delete-percent 50 \
  --docs 32768 --repeat 20 --query-shapes mixed --reader-rate 200 \
  --reader-seconds 10 --rounds 3 --output benchmarks/results/vacuum-mixed-50
```

A focused comparison matrix is:

| Profile | Scenario | Deleted | Docs | Repeat | Vocabulary/distribution | Rate/s | Seconds |
| --- | --- | ---: | ---: | ---: | --- | ---: | ---: |
| Live merge | merge | 0 | 32768 | 200 | 97/uniform | 200 | 10 |
| Short half dead | mixed | 50 | 32768 | 20 | 97/uniform | 200 | 10 |
| Short mostly dead | rewrite | 75 | 32768 | 20 | 997/hot | 200 | 10 |
| Larger long documents | mixed | 90 | 262144 | 200 | 4093/hot | 50 | 30 |

For `merge`, omit `--delete-percent` because it has no deletions. Run three
alternating pairs with `--query-shapes mixed`. The larger profile is an explicit
follow-up, not an automatic CI requirement. Rates are probe settings, not known
capacity: inspect lag before treating a profile as steady state. VACUUM must
finish while readers are active, and at least one complete checked reader
transaction must overlap it. Increase duration/size as needed; do not suppress
these gates to make an undersized or overloaded trial appear valid.

Strategy calibration can use `--baseline-vacuum-strategy` and/or
`--integrated-vacuum-strategy auto|direct|reconstruct`. Only set these for a binary
that implements `stannum.experimental_vacuum_merge_strategy`. The probe loads the extension and inspects `pg_settings` in the same backend,
requiring a real registered `pg_settings` entry with the requested value; an unknown
custom-GUC placeholder cannot falsely label an old baseline. Strategy choices
and binary hashes are recorded separately for each build. Omitted choices leave
the binary's default policy intact.

## Harness validation

77 Python harness tests passed, including scheduling/phase boundary arithmetic,
failed/skipped accounting, oracle structure, and library swap lifecycle ordering.
Local PG18 smoke runs use identical retained candidate libraries on both sides;
they establish harness functionality, not a speedup. Artifacts are retained in
`/tmp/stannum-e2e-coverage-smoke-topk` and
`/tmp/stannum-e2e-coverage-smoke-hot`. The first preliminary smoke used a ranked
full-sort query; a subsequent plan assertion caught an incorrect plan-provider
name before traffic. Neither is a performance result. Final top-k smoke requires
the actual plan's `Top K` and `Order` fields.

Both final smoke profiles completed 802 scheduled transactions per side at a
nominal 800 arrivals, zero failures/skips, with all four query shapes exercised.
The 50% mixed fixture retained 16,369 documents; the hot 75% rewrite retained
8,192. All final layout/membership/verifier gates passed. Across the four windows,
schedule-lag p95 was 8.043–10.013 ms and maximum lag 12.879–15.441 ms; these numbers
illustrate recorded load metrics, not a comparison between different binaries.

The strategy-setting guard also has a targeted PG18 smoke under
`/tmp/stannum-e2e-forced-strategy-smoke-v2`: 1,024 documents, mixed75, repeat20,
forced direct then reconstruct against the same new binary. Both passed layout,
reader, and verifier gates (256 live documents). The old baseline was deliberately
run with a forced strategy under `/tmp/stannum-e2e-unsupported-strategy-smoke`;
it correctly failed registration before traffic, despite the custom-GUC
placeholder. Earlier checker attempts failed setup because the fresh backend
had not loaded the library and because enum display labels were capitalized.
The guard now loads and inspects in one backend and compares enum labels without
case sensitivity. These setup failures and tiny smokes are not performance data.


The scoring-epoch fix passed a PG18 paired smoke against identical binaries at
`/tmp/stannum-e2e-scoring-epoch-smoke`: each window had 184 stable timed ranked
comparisons, zero sampled invalidations, and stable pre/post comparisons. All
other correctness gates passed. This validates the guard and accounting, not a
performance improvement.

A separate diagnostic copy under `/tmp/stannum-e2e-epoch-invalidation-harness`
pauses after capturing the exhaustive reference and waits for that point before
starting VACUUM. It adds no index locks or database writes. The four-reader
PG18 paired run under `/tmp/stannum-e2e-epoch-invalidation-smoke-v2` reported
68 stable comparisons and four invalidations out of 72 ranked completions on
each side, plus stable pre/post comparisons. All normal harness gates passed.
This demonstrates actual publication invalidation, rather than treating it as
successful rank equality. The initial two-reader/20-per-second diagnostic
observed invalidation but failed the independent complete-reader-overlap gate;
it is retained separately and is not a passing benchmark. Artificial sleeps
and coordination exist only in the diagnostic copy, and neither diagnostic
provides performance evidence.
