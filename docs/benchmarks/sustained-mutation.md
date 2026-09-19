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

The existing workload has count and ranked forms of term, Boolean, phrase and
miss queries. Writers choose inserts, deletes and match-changing updates with
equal weights. Inserts copy corpus documents under new IDs; deletes/updates
select a live row at or above a random ID. Concurrent deletion or gaps beyond
the last live ID can cause no-ops. Each DML statement now returns its affected
row count; only no-ops emit a marker. With failed transactions rejected, completed
transactions minus no-ops gives actual row changes. The final heap count must
equal initial rows plus affected inserts minus affected deletes.

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
