# Repeated Wikipedia memory-pressure measurements

Four fresh builds, in 2 GiB / 512 MiB / 512 MiB / 2 GiB query-memory order.
Each build receives 2 GiB. Each trial uses 500,000 original Wikipedia documents,
4 Docker CPUs, 2 readers, 128 MiB shared buffers, 64 MiB maintenance_work_mem,
15 seconds of warmup and 60 seconds of mixed ranked top-10 traffic.

| Trial | QPS | p95 ms | Guest reads MiB | Build peak anonymous MiB |
|---|---:|---:|---:|---:|
| 01-2g | 628.1 | 5.83 | 2.6 | 1198.6 |
| 02-512m | 582.7 | 6.68 | 11893.1 | 1154.7 |
| 03-512m | 579.6 | 6.57 | 11177.7 | 1203.1 |
| 04-2g | 631.0 | 5.85 | 27.5 | 1187.9 |

The paired throughput ratios are 0.928 and 0.919: a 7–8% loss at the lower
query-memory cap. Median p95 rises about 13%. These repeated windows narrow
the initial pilot's estimate; they do not demonstrate physical-storage performance.

## Query families

Median of the two trial p95 values for each memory setting; not a pooled percentile.

| Family | 2 GiB p95 ms | 512 MiB p95 ms |
|---|---:|---:|
| conjunction | 4.72 | 5.33 |
| disjunction | 6.57 | 7.11 |
| phrase | 6.15 | 7.39 |

## Profiling candidates

Largest increases in median per-trial query p50. These are exploratory rankings,
not significance tests; selection across 906 forms is subject to noise.

| Query ID | 2 GiB p50 ms | 512 MiB p50 ms | Minimum samples per trial |
|---|---:|---:|---:|
| 302:conjunction | 7.28 | 16.24 | 38 |
| 302:phrase | 6.19 | 12.72 | 38 |
| 169:conjunction | 5.11 | 9.01 | 38 |
| 88:disjunction | 8.30 | 11.94 | 38 |
| 301:disjunction | 33.44 | 35.36 | 38 |
| 146:conjunction | 3.79 | 5.60 | 38 |
| 302:disjunction | 39.02 | 40.76 | 38 |
| 95:disjunction | 4.83 | 6.55 | 38 |
| 301:phrase | 39.06 | 40.76 | 38 |
| 52:phrase | 4.79 | 6.48 | 39 |
| 52:disjunction | 6.04 | 7.64 | 38 |
| 88:phrase | 5.22 | 6.70 | 38 |
| 39:disjunction | 4.41 | 5.66 | 38 |
| 181:phrase | 4.71 | 5.95 | 38 |
| 283:conjunction | 3.02 | 3.97 | 38 |

Source query 302 is a long common-word sentence about search engines; its AND
and phrase forms roughly double in median latency under pressure. Query 169 is
`time in denver`. The broad trace supplies only about 38–42 samples per form,
so these should seed dedicated longer runs with plans and buffer counters.
The common-word `to be or not to be` forms are already expensive at 2 GiB;
include them as controls when separating memory sensitivity from CPU cost.

All trials observed all 906 timed forms. Each trial passed the explicitly sampled
18-form membership and exhaustive same-engine ranking checks. This is not an
independent Lead correctness proof. Two repetitions per setting expose variation
but do not establish production capacity or confidence intervals.

Docker guest block reads may be served by host caches. Timings include local
client transport; other host services remain running. Resource peaks are sampled,
and anonymous memory is container-wide, not an allocation profile.

Raw trial manifests, plans, samples and resource counters are retained in
`benchmarks/results/published-memory-repeated/`. `orchestration.py` records the
exact run commands. The immutable engine image matches the preceding pilot. Engine source hashes
and all workload settings except query memory match across trials. The checkout
became dirty only from report edits; that flag is excluded from the comparison.

[Machine-readable measurements](repeated-memory-results.json).

## One-million-document scale probe

A subsequent single run doubles the imported prefix to 1,000,000 documents,
keeping the 2 GiB build/query caps, two readers and other settings fixed.
It completes at **351.8 QPS, 10.84 ms p95 and 26.75 ms p99**. All 906 timed
forms are observed; the selected 18 membership/ranking checks pass with no
query-window OOM. This is a single diagnostic, not a repeated scale estimate.

The index is 1.77 GiB and total relation storage 3.00 GiB. Construction takes
128.3 seconds with a sampled anonymous-memory peak of 1,234 MiB. Query-window
guest block reads total only 11.0 MiB across the sampled interior intervals,
and sampled query memory peaks at 1.28 GiB. Therefore the roughly 44% throughput
loss relative to the half-million 2 GiB median cannot simply be labeled a
storage-pressure cliff. More postings/candidates, segment layout, and the
changed term/document distribution in the larger prefix are possible factors.
Plans, work counters and reader scaling should distinguish them.

Family p95 is 8.36 ms for AND, 11.55 ms for OR and 12.27 ms for phrases.
The phrase p99 is 42.70 ms. These tails strengthen the case for targeted phrase
and common-word experiments before claiming production capacity.

[One-million-document evidence](million-memory-results.json) retains configuration,
source/input identities, correctness outcomes, per-query distributions and
resource counters. Raw data is under `benchmarks/results/published-million-memory`.
Reproduce with the command below using `--rows 1000000 --memory 2g` and a fresh
output directory. This run does not substitute for full Wikipedia or Stack Exchange.

## Reproduction

With the pinned driver prepared and the recorded engine image available, run
this schedule sequentially (each invocation owns its own container and volume):

```sh
for trial in 01-2g 02-512m 03-512m 04-2g; do
  python3 benchmarks/tin.py --driver /path/to/verified/tin-driver run \
    --published-corpus wikipedia --dataset /path/to/planetscale-wikipedia \
    --image stannum-bench:published-corpus \
    --output "benchmarks/results/memory-repeat/$trial" \
    --rows 500000 --validation-rows 1000 --validation-queries 18 \
    --seconds 60 --warmup 15 --clients 2 --cpus 4 \
    --workload topk --style mixed --engines stannum \
    --memory "${trial#*-}" --build-memory 2g \
    --shared-buffers 128MB --maintenance-work-mem 64MB || break
done
```

Image tags are mutable; compare the resolved immutable image and source hash
in each manifest with the recorded evidence. Do not pool a partial schedule.

Raw evidence archive (CSV copies excluded; input checksums remain in manifests):
`benchmarks/results/published-memory-evidence.tar.gz`, SHA-256
`3f9a6ac8a857b62c8b9ccdd47d8a1408abc641352200cf11333c4cc0257ed3ad`.
The archive is local, not committed; compact measurements above are tracked.
