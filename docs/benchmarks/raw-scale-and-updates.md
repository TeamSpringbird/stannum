# Raw Stack Exchange scale and update validation

Two local ARM64 PostgreSQL 18 campaigns completed: ranked reads on 100,000 raw
published documents, followed by concurrent reads/updates on 1,000 documents.
Both used the confirmed 3,762-form trace. Stannum and GIN passed every selected
check; in these runs all 3,762 forms were selected for validation.

## 100,000-row read-only result

Membership was checked on a 1,000-row sample, with full-corpus counts retained.
Every form's top-ten score multiset was checked against exhaustive same-engine
scoring over all 100,000 documents. Both engines passed with zero mismatches.
The separate 1,000-row Lead gate remains the independent compatibility check;
this run did not compare 100,000 rows against Lead.

Two clients ran for 60 seconds after five seconds warmup, in separate fresh
containers with four CPUs, 2 GiB memory, 128 MiB shared buffers and 64 MiB
maintenance_work_mem. Validation/builds were outside timed traffic. Stannum
completed 1,275.8 QPS with 6.685 ms p95 and traversed all 3,762 forms. GIN
completed only 712 forms in its window: its aggregate covers a different query
mix, so no matched-workload ratio is reported. GIN also uses a different ranker
and has recorded membership differences.

Stannum's sampled query memory peak was 244,150,272 bytes (232.8 MiB), versus
245,153,792 for GIN. No OOM events were recorded. Both were well below the 2 GiB
cap, so this does not prove memory-pressure behavior. Samples can miss peaks.
These are single diagnostic windows, not repeated capacity estimates or TIN
performance claims. Exhaustive validation was the dominant campaign cost;
future rapid loops may use explicit, recorded validation samples, while keeping
full-trace milestone runs separate.

## Concurrent update smoke and the new check

The update harness previously checked ranked results only before traffic.
It now repeats exhaustive ranked validation afterward and stores
`ranked-correctness-after.txt`. Paired ranked/update comparisons reject missing,
failed or differently sized post-update validation evidence. Older update
reports without this evidence must be rerun before using that comparison path.
Read-only comparisons retain their existing contract.

The live smoke used 1,000 raw rows, a full-row membership reference, two search
clients, three seconds warmup and 30 seconds traffic. The upstream-style serial
updater requested at most 100 starts/second, appending whitespace to indexed
bodies. It uses the existing live-row fallback. This is not uniform row sampling,
insert/delete traffic, or a maximum sustainable-write-rate test.

| Engine | Completed updates | Update errors | Timed forms | Post-update membership/count checks | Post-update ranked checks |
|---|---:|---:|---:|---:|---:|
| Stannum | 2,693 | 0 | 3,762 | 3,762 passed | 3,762 passed |
| GIN | 2,714 | 0 | 3,762 | 3,762 passed | 3,762 passed |

The requested 100/s ceiling was not achieved continuously; completed operations
are reported instead. Row counts and checked full-corpus match counts stayed
unchanged. Ranked validation checks each engine against its own exhaustive
scoring after updates, not exact pre/post score equality or cross-engine BM25
agreement. No OOM events were recorded. All 155 harness tests passed, including
rejection of missing, failed and zero-coverage post-update ranked receipts.

## Next gates

1. Give the 100,000-row GIN workload enough time to traverse the same full trace
   before presenting aggregate performance comparisons. Record achieved coverage
   and membership/ranker differences regardless of duration.
2. Repeat chosen baselines/candidates, then increase raw corpus size or lower the
   query memory cap deliberately to exercise memory pressure. Keep build and query
   limits explicit and record failed attempts.
3. Extend mutation coverage beyond whitespace updates where useful; keep the
   independent Lead semantic gate alongside same-engine execution checks.
4. Reuse the existing AWS lifecycle/measurement protocol once local gates pass.
   A fresh closed-source TIN comparison still requires a running PlanetScale DB.

## Evidence

[Compact receipts](raw-scale-and-updates-results.json) include image/source/input
identities, configurations, check coverage, distributions, update counts and
resource deltas. The raw archive is
`benchmarks/results/raw-scale-and-updates-evidence.tar.gz`; its SHA-256 is in
the receipt. It contains protocol snapshots, SQL, plans, observations, driver
output and samples. Both campaigns used the same immutable extension image.
The update run used the modified harness captured in its protocol directory.
Campaigns/tests were serialized using the shared native-test lock. All owned
containers and volumes were verified removed; no AWS resources were created.
