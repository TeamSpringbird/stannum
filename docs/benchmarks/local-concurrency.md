# Local concurrent snapshot comparison

The next local experiment measures real completed requests with 1, 2, 4 and 8
clients, using the same clean/mutated million-row snapshots as the single-client
count experiments. Compare main, candidate default and forced bitmaps. Three
rounds rotate variant order; client-count order reverses on alternating rounds.
Each trial restores a fresh physical snapshot and starts a new server.

A 100k, 1/2-client, two-second smoke matrix runs first. It is harness validation,
not a performance claim. Only if it passes does the full matrix run: two states,
four client counts, three variants, three rounds, 20 seconds each (72 trials;
24 minutes of timed windows, plus substantial restore/warmup overhead).

Every trial checks all 302 counts and custom-plan selection before timing. Each
client has its own persistent PostgreSQL connection and deterministic shuffled
trace, shared seed per comparable round. Every timed result is checked against
previously independent-oracle-validated expected counts. Errors invalidate the
comparison. Warmup and connection setup are outside the measurement window.

This is closed-loop load: a client waits for its result before sending the next
query. Request latency uses a monotonic client clock and includes localhost
protocol/driver overhead. There is no EXPLAIN inside the timed window. Request
p50/p95/p99 include slow requests draining after the deadline; QPS counts only
successful completions within the fixed window. Drain time, errors and distinct
query coverage are reported, including trials that do not cover all 302 queries.
Request mix can differ between time-limited trials; raw query IDs are retained.
These are not open-loop queueing latencies or server-only EXPLAIN times.

Query-backend CPU time is read before and after each client's work and normalized
by the actual elapsed window plus drain. Python process CPU is recorded too. This
helps distinguish query saturation from a one-process Python driver bottleneck.
It excludes auxiliary PostgreSQL process CPU and is not a host-wide utilization
metric. Host load averages provide additional context. Shared macOS hardware,
uncontrolled filesystem caches and Python logging overhead limit generalization.
Do not call an 8-client throughput plateau database saturation without CPU and
client-headroom evidence. We do not assert equivalence to AWS's eight-CPU cap.

Protocol/settings: native PostgreSQL 18.6, shared_buffers 256MB, work_mem 16MB,
JIT and autovacuum off, custom plans, sequential scans discouraged. Private
extension bindings, pinned binary hashes, clean shutdowns and the shared local
lock prevent installation changes or overlapping benchmark jobs.

Run manually:

```sh
python benchmarks/snapshot_load_local.py --snapshot-run <validated-million-run> \
  --queries <queries.json> --output <new-directory> \
  --states clean mutated --clients 1 2 4 8 --seconds 20 --rounds 3
```

The durable queue lives at `benchmarks/results/concurrent-loop-r1/`. It waits for
the million-row serial comparison to succeed and for the independent PR45 queue
to terminate. A PR45 failure is retained but does not block this unrelated count
experiment; a failed count prerequisite or smoke test does block it. Each trial
retains plans, per-client JSONL request samples, summary and manifest. Completed
campaigns produce `report.md` and `summary.csv` with repeated QPS/p95/p99 results.
No incomplete campaign gets a final aggregate report.
