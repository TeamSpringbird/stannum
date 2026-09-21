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
