# Million-document local snapshot campaign

Local build-to-build experiments use the first 1,000,000 rows from the verified
published Wikipedia CSV. `published_dataset.prefix` preserves source order and
field contents; its output is hashed in the receipt. The earlier 100k run is the
size control. This campaign retains native macOS ARM snapshots; they are not
portable copies of the AWS Linux cluster.

Main 750872d builds the clean index. The count candidate 54c2d9e reads a physical
copy with the same page layout. Independent full-corpus token counts validate
all 302 normalized published OR queries, using a streaming corpus scan to avoid
retaining all document text in Python. Candidate checks run before and after
mutations, vacuum and restart; the focused transaction-visibility suite remains
part of the gate. No extension installation files are replaced.

The mutated snapshot follows the AWS hash schedule: about 5% id=id updates,
10% body whitespace updates, and 1/101 deletes. It is captured after correctness
checks and CHECKPOINT, before VACUUM, with autovacuum disabled. It was written by
the candidate; replay under main additionally checks that main can read its
results. This is not an index-construction comparison.

After compatibility succeeds, clean and mutated snapshots each receive a
three-round comparison of main, candidate default, and candidate forced bitmaps.
Each trial starts from a fresh physical copy. Variant order follows a balanced
Latin square; query order is randomized per round and shared across variants.
Each query receives an untimed count check and nine timed EXPLAIN ANALYZE samples.
The total is 48,924 timed executions across both states.

Keep shared_buffers=256MB and work_mem=16MB, matching the 100k experiment. This
holds those knobs fixed, not total resident memory: OS caches remain uncontrolled
on the shared development machine. Results are per-query server timings under
one client, not production/concurrent p95 or throughput. Report p50/p95/p99 of
302 per-query medians, run-to-run spread and individual regressions. No new
selector is enabled. Do not extrapolate timings to AWS or published TIN.

The campaign runner and frozen scripts live under
`benchmarks/results/local-million-campaign/`. Compatibility evidence and physical
snapshots are in `compatibility/`; comparison outputs are `latency-clean/` and
`latency-mutated/`. Retain failed phases explicitly. AWS remains necessary to
check results against the published PlanetScale hardware/workload target; local
experiments screen candidates and narrow the work worth spending cloud time on.

## Compatibility result

The one-million-row compatibility gate completed successfully: 302 baseline
checks plus 604 candidate checks in each of restored, mutated, vacuumed and
restarted states (2,718 total), followed by the 56-check visibility suite.
The mutated corpus has 990,189 rows. Clean and mutated snapshots occupy about
4.0GB and 4.6GB respectively. [Receipt](local-million-compatibility.json).
Repeated latency comparisons started after this gate; the compatibility receipt
alone is not a latency result.

## Full-corpus local merge-validation tier

The same native snapshot protocol is now queued for all 5,032,104 Wikipedia
rows. This host has 128 GiB RAM, 16 logical CPUs and approximately 2.9 TiB free
at setup. The verified CSV is 8,093,810,896 bytes, SHA-256
`7e4cba73338f9aba3a344de9f7d63007fa51f0f4ee85d70d7bd34fe95ff6e9a8`.
No new corpus download or AWS instance is needed.

The reusable local image is a cleanly stopped native PostgreSQL physical
snapshot plus its pinned extension libraries, source/binary hashes, query hash,
settings and correctness receipt. It is macOS ARM specific, not a portable
Docker image or a copy of an AWS cluster. Clean and pre-VACUUM mutated snapshots
are retained separately. Load/index construction runs once; each trial restores
a copy and binds a private library before restarting PostgreSQL.

Artifacts live under `benchmarks/results/local-full-campaign/`:

- `compatibility/`: full corpus load/index, independent OR-count oracle,
  candidate restore/mutations/VACUUM/restart checks and snapshot images.
- `merge-validation.json`: pinned build/test/run recipe consumed by the existing
  `benchmarks/experiment_queue.py`, with absolute host-local artifact paths.
- `merge-validation/status.json` and numbered logs: fail-closed queue progress.
- `merge-binaries/`: separately rebuilt main/cached/packed binaries and hashes.
- `cached-v-main/` and `packed-v-cached/`: two alternating rounds per variant
  and clean/mutated state, 600 seconds each, always eight clients.

The two comparisons contain 160 minutes of timed load in total, excluding
setup, compilation, correctness and warmups. The queue waits for full snapshot
compatibility and the million-row regression replay. Builds and tests share
the exclusive benchmark lock; timed runners acquire it themselves. Comparison
mode does not set experimental strategy GUCs on main-only release binaries.
The older pilot instrumentation is excluded from the two proposed release PRs.

PR #81 (`f5b41ff`) and #82 (`0db1479`) isolate cached and packed heads onto main
`750872d`. They remain drafts until exact-build performance, regression screens,
full correctness and cross-platform CI pass. A complete queue is measurement
evidence, not an automatic performance approval or automatic merge.

Full corpus size tests scale; it does not reproduce AWS CPU, storage or OS cache
behavior. `shared_buffers=256MB` is not a total memory cap. A later explicitly
constrained-memory run is needed to claim memory-pressure coverage. Retain the
million-row tier for fast diagnosis and use this tier for merge candidates.
