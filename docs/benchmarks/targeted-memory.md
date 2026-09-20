# Focused query-memory and reader-scaling probes

The repeated broad trace identified two pressure-sensitive source records:
`302` (a long sentence about search engines) and `169` (`time in denver`).
This exploratory trace uses their AND/OR/phrase forms plus record `1` (`the`)
as a common-term control: nine forms total, explicitly derived and hashed.

All runs use the same 500k-document prefix and immutable image, default build
batches, 2 GiB construction memory, 128 MiB shared buffers, 4 Docker CPUs,
15-second warmup and 90-second measured traffic. Each configuration runs once.

| Query memory | Readers | QPS | p95 ms | Guest reads MiB | Mean sampled CPU cores |
|---|---:|---:|---:|---:|---:|
| 2g | 2 | 286.2 | 36.43 | 15.9 | 1.84 |
| 512m | 2 | 292.4 | 35.97 | 2.1 | 1.85 |
| 2g | 4 | 578.0 | 36.34 | 1.3 | 3.66 |

The narrow trace shows no observed low-memory penalty: 286 versus 292 QPS is
within the scope of single-window variation, not a claim that less RAM is faster.
Four readers reach 578 QPS with similar p95 and roughly twice the sampled CPU
use. This demonstrates useful reader scaling for this warm focused workload;
it does not establish larger-corpus or mixed-write capacity.

The broad trace's long AND/phrase penalty disappears with frequent reuse.
That supports working-set/cache-reuse dependence, but does not isolate a
specific cache layer. Keep the broad trace as the regression control.

## Per-query latency

| Form | 2 GiB / 2 readers p50 ms | 512 MiB / 2 readers p50 ms | 2 GiB / 4 readers p50 ms | Minimum samples |
|---|---:|---:|---:|---:|
| 169:conjunction | 2.97 | 2.96 | 2.99 | 2862 |
| 169:disjunction | 3.35 | 3.34 | 3.37 | 2861 |
| 169:phrase | 3.86 | 3.83 | 3.67 | 2862 |
| 1:conjunction | 3.10 | 3.09 | 3.09 | 2861 |
| 1:disjunction | 3.07 | 3.06 | 3.06 | 2862 |
| 1:phrase | 3.05 | 3.02 | 3.08 | 2862 |
| 302:conjunction | 1.51 | 1.50 | 1.47 | 2862 |
| 302:disjunction | 36.23 | 35.82 | 36.17 | 2861 |
| 302:phrase | 0.42 | 0.41 | 0.41 | 2862 |

The long AND and phrase forms produce zero rows and perform no heap fetches
in the setup plans, yet touch roughly 3,300 shared buffers. Investigate index
traversal/matching and early rejection before optimizing heap retrieval for
these forms. The AND path reports block-max pruning; zero results do not imply
zero index work. The hot OR median remains around 36 ms. Its generic setup
plan scores only 134 candidates, fetches 10 heap rows and hits 3,952 shared
buffers. Profile cursor advancement and bound calculations before assuming
bulk scoring or heap retrieval dominates.

All nine timed forms have membership checks on 1,000 sampled rows and exhaustive
same-engine ranking checks over the imported corpus. These all pass. This is
not independent Lead verification. P99 is retained only when at least 1,000
samples exist for the corresponding distribution.

## Interpretation limits

Narrowing the trace changes cache competition; this cannot be substituted for
the broad 906-form pressure experiment. These single windows are diagnostic
reader-scaling observations, not capacity estimates or repeated speedups.
Guest block reads may be served by Docker host cache. CPU use is averaged over
the resource sampler's interior coverage, not the exact full timed window.

Prepared custom and generic plans run in a fixed order before warmup. Their
timing differences are confounded by cache warming and are not evidence that
one planner mode is faster. The retained plans are useful for path/work counts.

[Machine-readable evidence](targeted-memory-results.json) includes per-query
distributions, sample counts, resources, source/data identities and plan counters.
Full SQL, JSON plans and raw samples remain in
`benchmarks/results/targeted-memory-readers`.

## Reproduction

Select source records 302, 169 and 1, in that order, from the pinned Wikipedia
trace. Their complete engine query encodings and parent trace hash are preserved
in `targeted-memory-results.json` under `trace`; write that object to a JSON file.
Pass it as `--query-file`, with `--rows 500000 --validation-rows 1000
--validation-queries 0 --workload topk --style mixed --seconds 90 --warmup 15
--cpus 4 --build-memory 2g --shared-buffers 128MB --maintenance-work-mem 64MB`.
Run `(memory, clients)` as `(2g, 2)`, `(512m, 2)`, `(2g, 4)` sequentially,
using a new output directory each time and the recorded immutable image.
The combined raw archive and checksum are listed in the
[construction report](build-memory-results.md#reproduction-and-evidence).
