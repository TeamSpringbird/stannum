# First matched GIN mutation/count measurements

All 18 local windows passed exact membership checks and mutation row accounting,
with repeated VACUUM and oracle coverage inside the writer window. Stannum also
passed deep structural verification after cleanup. These measurements use the
[live-target protocol and GIN configurations](gin-mutation-baseline.md), with no
runtime index changes. Compact per-window data and comparison-guard outcomes are
in [gin-baseline-results.json](gin-baseline-results.json).

Hardware/software: macOS arm64, 16 logical CPUs, PostgreSQL 18.6, private cluster,
512 MiB shared buffers; WAL durability defaults enabled. One retained Stannum
release (`aa228e9fdc68319c5ae3ff55c60ab51d75bce0b2bfe959eb67b13f49fcd98472`)
was used throughout. Full source/fixture identities, plans, query-level samples
and settings remain in the local `benchmarks/results/gin-matched-short` and
`gin-matched-wikipedia` artifacts. No builds or concurrent benchmark campaigns
ran during timing.

## Synthetic fixture

50,000 short repetitive documents, two writers, two readers, 500 offered count
queries/sec, equal-weight insert/delete/update traffic. Each window lasts 30
seconds after one second of reader warmup. VACUUM runs every five seconds;
independent oracle rounds run every ten seconds. Engine/case order reverses on
the second repetition.

| Offered writes/s | Engine | Windows | Actual changes/s | Reads/s | Scheduled reader p95 (ms) | Index MiB after cleanup | Traffic + observer WAL MiB |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2000 | GIN off | 2 | 1,975.3–1,975.6 | 493.2–493.6 | 46.7–84.1 | 0.4 | 35.4 |
| 2000 | GIN on | 2 | 1,975.9–1,977.3 | 489.2–490.9 | 98.0–205.4 | 0.9 | 25.7–25.8 |
| 2000 | Stannum | 2 | 1,975.4–1,977.3 | 493.6–494.0 | 5.3–5.4 | 4.7 | 27.4–27.5 |
| 8000 | GIN off | 1 | 7,977.6 | 422.2 | 3,821.5 | 0.4 | 101.4 |
| 8000 | GIN on | 1 | 7,974.2 | 378.6 | 6,046.5 | 2.3 | 92.3 |
| 8000 | Stannum | 1 | 7,974.4 | 489.2 | 6.1 | 10.6 | 108.5 |

Two first-repetition 8,000/sec comparisons were rejected by the strict harness
identity guard because a CLI error-message edit changed the harness hash between
windows. The raw successful runs are retained, but the table excludes that whole
three-engine group; the 8,000/sec row therefore has only one accepted window per
engine. No guard was waived. Ranges above are per-window observations, not
confidence intervals or aggregate percentiles.

At 8,000 offered writes/sec, all engines delivered nearly the requested mutations.
GIN readers accumulated backlog; Stannum readers kept pace. Thus this test found
a read-service limit under this mix, not GIN's maximum write capacity. The larger
Stannum index is part of the tradeoff, not an omitted cost.

## Verified Wikipedia fixture

100,000 sampled articles from Wikimedia's 20231101.en snapshot, verified by the
existing dataset manifest/checksums (source revision
`b04c8d1ceb2f5cd4588862100d08de323dccfbaa`). Input is normalized to lowercase ASCII
word tokens, limited to 8,192 tokens per document, with a mutable suffix. Original
body sizes have median 1,296 bytes and p95 10,121 bytes. This is a representative
text fixture, not untouched production text or a production workload.

Two writers, two readers, 200 offered mutations/sec and 20 offered count queries/sec.
Each of the six windows lasts 90 seconds after three seconds of warmup; VACUUM
runs every ten seconds and oracle rounds every 30 seconds. All matched pairs
passed the comparison guard. Throughput uses each process's observed elapsed
time, including any final in-flight query completion.

| Offered writes/s | Engine | Windows | Actual changes/s | Reads/s | Scheduled reader p95 (ms) | Index MiB after cleanup | Traffic + observer WAL MiB |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 200 | GIN off | 2 | 197.7 | 3.5 | 66,816.8–67,948.0 | 114.7 | 1,384.9–1,386.5 |
| 200 | GIN on | 2 | 197.6–197.7 | 3.5 | 68,005.5–68,145.3 | 118.7–118.9 | 858.5–893.0 |
| 200 | Stannum | 2 | 197.6–197.7 | 19.9 | 17.1–18.2 | 340.1–344.1 | 130.3–131.6 |

All engines kept up with the requested mutations. Both expression-GIN variants
were read-overloaded; their high scheduled p95 includes client backlog and is
not a per-query execution-time speedup ratio. The initial GIN common-phrase plan
already took approximately 3.9 seconds before mutations, while its common-term
plan took about 6 ms. Turning pending-list buffering off did not remove the
read bottleneck. These observations motivate testing stored `tsvector` values;
they do not establish that pending-list cleanup never matters.

WAL above includes heap, primary-key, index and maintenance work through completion
of in-flight observers. Final drain WAL is recorded separately in the JSON. Index
sizes are search-index files after cleanup; heap sizes are retained in the JSON.
Pending-list peaks are sampled, so short bursts may be missed. Neither final
cleanup nor short-window stability proves a long-term maintenance or memory bound.

## Next controlled experiment

Before using these results as a broad comparison with PostgreSQL full-text search,
add a stored-`tsvector` GIN variant and permit normal planner choices. The current
baseline uses expression GIN and forces index access on all engines. Measure
phrase-query execution separately from offered-load queueing, then repeat longer
windows at rates below and around each configuration's read-service limit.
Keep native ranking versus BM25 outside this membership/count comparison.
