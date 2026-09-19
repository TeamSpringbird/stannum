# Sustained mixed-write validation

This extends the existing mutation profile on top of PRs #20 and #21. It changes
benchmark tools and CI, not the index implementation or storage format. The goal
is to measure reads, actual row changes, and repeated maintenance together before
choosing the next runtime optimization.

## Protocol

`benchmarks/sustained.py` starts a private PostgreSQL cluster with 512 MiB shared
buffers and controlled checkpoint settings. Each window uses a fresh database,
the same retained release artifact, and warm readers. Cases vary total offered
write rate and writer connection count; repetitions reverse case order. The
reader rate is independent of the writer rate. There are no builds or binary
swaps during a campaign; use the shared installation/timing lock when applicable.

The workload has count and ranked forms of term, Boolean, phrase and miss
queries. Writers choose inserts, deletes and match-changing updates with equal
weights. Inserts copy corpus documents under new IDs. Updates/deletes probe the
current maximum ID, select the first unlocked live row at or above a seeded
random point in that key range, and wrap around if needed. This samples key
space, not rows uniformly; sparse gaps can bias selection and the extra lookup
cost is part of every engine's timed workload.

Each DML statement asserts exactly one returned row in the same autocommit SQL
statement. Zero or multiple effects raise an error and roll back the statement;
any failed transaction invalidates the run. This removes per-no-op shell calls.
Successful logged transactions therefore equal affected rows. If the table is
empty or every target is locked, the benchmark fails explicitly rather than
reporting successful no-op throughput. The final heap count must equal initial
rows plus committed inserts minus committed deletes. The protocol identity is
`live-key-wrap-v2`; old marker-accounted results are not paired comparisons.

Table autovacuum is disabled for this controlled profile. One scheduler runs
serial VACUUM operations; it records scheduling delay and missed intervals rather
than silently queueing catch-up work. Separate threads sample physical layout
and compare query membership with regex heap scans in a shared SQL snapshot.
Ranked checks establish distinct, finite, ordered, matching results and correct
cardinality, **not independent BM25 arithmetic or globally optimal top-k**. The
fixed-immutable-directory scoring guard from the earlier VACUUM benchmark is not
valid during writes and is not reused here. Periodic oracle and sampling work
compete for server resources; their durations and missed slots are recorded.
They are not embedded in every measured reader transaction.

The campaign requires at least two complete VACUUM cycles and one oracle round
inside the writer window. Once readers, writers and observers finish, two
additional VACUUM passes run without traffic. These are explicitly labeled
`drain`, followed by a membership/ranking check and deep structural verification.
Quiescent cleanup demonstrates recovery after load stops; it does not establish
that maintenance kept pace during load. Failures retain raw artifacts and the
stopped private cluster for diagnosis. Failed shutdown preserves cluster data.

## Measurements and interpretation

- Reader and writer logs report scheduled latency, execution latency, scheduling
  lag, and first/last scheduled-arrival deciles separately. A p99 is absent when
  fewer than 1,000 observations exist in that group.
- Nominal arrivals are rate × duration, an expectation for seeded Poisson traffic.
  Compare that with logged completions. Arrivals never emitted at shutdown are
  absent; zero logged failures is not proof of sustained capacity. Lag measures
  client scheduling pressure, not a server queue length.
- Samples report segment count, buffered documents, known dead entries, index
  and heap bytes, generations, PostgreSQL statistics, WAL LSN and reusable pages.
  Known dead entries exclude deletions not yet discovered by VACUUM. Samples
  taken during writes are not an atomic heap/index consistency oracle.
- Index file size need not shrink when pages become reusable. Interpret size
  together with free pages, live population, dead entries and maintenance events.
- `after.json` captures traffic plus completion of in-flight observer operations;
  `after-drain.json` separately captures quiescent cleanup. Traffic latencies
  exclude the drain. `samples.json` and `vacuums.json` label the phase.

Treat the load sweep as evidence for this fixture, hardware, duration and
maintenance policy. A useful operating point needs stable reader/writer lag,
meaningful affected-row throughput and repeated maintenance without accumulating
work. Longer runs and representative corpora are needed before calling it a
production capacity limit. This campaign does not measure allocator peak memory.

## Running it

Install a release containing the upstream changes, retain a copy of its library,
and put the matching PostgreSQL tools on PATH. From the repository root:

```sh
python3 /tmp/stannum-pgrx-lock.py python3 benchmarks/sustained.py \
  --artifact /absolute/retained/stannum.dylib \
  --output benchmarks/results/sustained-baseline \
  --rows 50000 --seconds 60 --rounds 3 \
  --write-rates 100,1000,3000 --writer-counts 1,2 --read-rate 100
```

Use the equivalent installation lock on other machines. `--body-repeat 20`
increases synthetic filler length while preserving query membership; it is still
a repetitive synthetic corpus. `--dataset` accepts an existing verified dataset
instead and cannot be combined with a nondefault filler multiplier. `--set`
overrides recorded storage settings. The controlled defaults use a 256-document
write buffer, 2,000-document build segments, a 16-segment target, and a 2,048-document
foreground merge budget.

`campaign.json` records every completed window with loads, actual row changes,
maintenance and drain summaries. Each window retains manifests, plans, SQL,
pgbench logs, timeline, physical samples, oracle results and verification results.
The regular PG17/18 × ARM/x86 CI matrix runs a 20-second, two-writer smoke. It
checks the protocol and correctness, not a hardware-independent timing threshold.

## Historical load sweep (original no-op-accounted protocol)

Eight 45-second windows passed on native PostgreSQL 18.6, macOS arm64 (16 logical
CPUs). The release was built from runtime base `4beaaa3`, containing PRs #20/#21;
its SHA-256 is `aa228e9fdc68319c5ae3ff55c60ab51d75bce0b2bfe959eb67b13f49fcd98472`.
The parent was subsequently rebased after #20 merged, without changing its tree.
Per-window identities and measurements are retained in
[sustained-baseline.json](sustained-baseline.json). Raw artifacts remain locally
under `benchmarks/results/sustained-short` and `benchmarks/results/sustained-long`.

The short fixture had 50,000 documents and 500 offered reads/sec. The long fixture
had 10,000 documents, 20× repeated filler, and 100 offered reads/sec. Both used two
reader connections, equal-weight inserts/deletes/updates, one-second warmup,
VACUUM every 10 seconds and correctness checks every 15 seconds. These are
single observations per configuration, not repeated statistical estimates.

| Fixture | Offered writes/s | Writers | Actual row changes/s | Reader scheduled p95 (ms) | Writer lag p95, first → last decile (ms) |
| --- | ---: | ---: | ---: | ---: | ---: |
| short | 200 | 1 | 199 | 5.6 | 7.6 → 5.1 |
| short | 200 | 2 | 195 | 5.9 | 8.5 → 8.4 |
| short | 2000 | 1 | 1750 | 16.7 | 304.6 → 1.6 |
| short | 2000 | 2 | 1738 | 16.3 | 138.7 → 1.8 |
| short | 8000 | 1 | 553 | 5.4 | 3665.7 → 39218.3 |
| short | 8000 | 2 | 584 | 5.4 | 3534.7 → 38865.0 |
| long | 200 | 2 | 184 | 10.9 | 9.7 → 8.6 |
| long | 2000 | 2 | 1056 | 10.7 | 2774.8 → 11725.8 |

All eight windows completed at least two VACUUM cycles and one oracle round
wholly within writer traffic. Across their periodic rounds, all 144 query checks
passed; final checks, committed row-count accounting and post-drain deep index
verification also passed. No maintenance scheduling slots were missed. The
largest observed traffic VACUUM duration was 57 ms. Dead entries remained in some
segments after drain; successful verification does not mean every tombstone was
physically removed or that a long-term space bound has been proved.

At 2,000 offered writes/sec, the short fixture completed about 1,740–1,750 actual
row changes/sec and cleared its early scheduling backlog. At 8,000 offered
writes/sec, lag grew to roughly 39 seconds. This is overload of the tested system
and workload, not an isolated index capacity limit: random target selection uses
an ID ceiling based on offered inserts, and roughly 44% of completed transactions
were no-ops in those high-rate windows. Each no-op also invokes a shell to record
its marker. That client overhead and the changing useful-work fraction confound
high-rate capacity estimates. Reader latency staying low in those cases does not
mean the requested mutation throughput was delivered.

The longer-document 2,000/sec case also accumulated lag. Repeated filler exercises
position volume but not realistic vocabulary diversity. Neither synthetic fixture
establishes a production workload limit, a memory bound or a speedup over GIN.

### Next comparison

Add a shared membership/count workload for Stannum, GIN and optionally GiST,
with identical document text, mutation mix and maintenance policy. The existing
GIN adapter currently supports count profiles, not this mutation protocol.
Measure actual changes/sec, reader tails, WAL, index size and backlog through
multiple cleanup cycles. Include GIN pending-list measurements with `fastupdate`
on, plus an explicit off variant. PostgreSQL documents the cleanup/read tradeoff
in its [GIN implementation](https://www.postgresql.org/docs/18/gin.html#GIN-FAST-UPDATE)
and recommends GIN over the lossy GiST text-search signatures for general text
search in its [index comparison](https://www.postgresql.org/docs/18/textsearch-indexes.html).

Before calling the resulting plateau capacity, replace or calibrate the per-no-op
shell accounting and use a target-selection protocol whose useful-work fraction
does not collapse with offered rate. Compare ranked retrieval separately: native
PostgreSQL ranking is not the same computation as BM25. Follow the synthetic
probes with representative documents and longer, repeated windows.

## Live-target generator validation

The v2 generator passed the 30-second PG18 smoke at 8,000 offered mutations/sec
with two writers, repeated maintenance, row-count accounting, oracle checks and
deep verification. This is a protocol check, not a capacity result. The native
`benchmarks/mutation_targets.py` regression exercises sparse billion-scale keys,
wrapping around a locked maximum, exhaustion rejection and rollback of a
multi-row mutation. It runs in the PG17/18 × ARM/x86 CI matrix.
