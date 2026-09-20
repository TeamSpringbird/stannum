# Release published segment buffers before maintenance

Publication now consumes the encoded segment vector and drops it immediately
following the page write. Both CREATE INDEX and write-buffer folding use this
path. Previously the caller retained it while maintenance read the new segment
back into another owned vector. This changes ownership only: no on-disk format,
query execution, merge policy or batch defaults change.

## Observations

The original Wikipedia prefix contains 500,000 documents. The pinned PlanetScale
trace supplies 906 query forms. Local Docker uses four CPUs, 128 MiB shared
buffers, 64 MiB maintenance_work_mem and default 32,768-document batches.

| Case | Result |
|---|---|
| 1.25 GiB construction cap | Still OOM; Docker confirms OOMKilled |
| 2 GiB construction/query cap | Completes; 66.26 s construction |
| Query window: two readers, 60 seconds | 619.18 QPS, 5.867 ms p95; all 906 forms observed |
| Selected correctness checks | 18 membership and 18 exhaustive same-engine ranked forms, zero mismatches |

Sampled construction anonymous memory peaked at 1,130.2 MiB versus 1,181.0 MiB
in the preceding output-reuse image's earlier run. These are single observations,
not a paired causal estimate or a guaranteed 51 MiB saving. Total cgroup memory
includes page cache and reaches the cap; the smaller-cap failure is retained.
Query throughput is consistent with prior controls, but this run establishes no
speedup. Timings include client transport and are not production capacity claims.

All 126 local PostgreSQL 18 integration tests pass; Clippy passes with warnings denied. It exercises construction,
folding, merging, updates/deletes, cancellation and maintenance correctness.

## Next step

Both changes so far remove avoidable overlap, but merge still retains all encoded
inputs plus output. Evaluate an explicit working-memory budget and streaming or
spill-backed inputs/output against this retained OOM reproducer. Include directory
size and query read amplification in acceptance criteria; lowering batch size
alone previously increased index size and hurt the million-document case.

[Compact evidence](release-published-segment-results.json) records source/image,
corpus/trace/config identities, correctness, samples summary and the OOM outcome.
Raw local files live under `benchmarks/results/release-published-build`,
`release-published-memory` and `release-published-2g`; the orchestration scripts
are copied into each campaign. They are not committed.
