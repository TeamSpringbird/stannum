# Fused sparse page decoding experiment

Experimental branch `perf/fused-sparse-pages`; core prototype `e64006f`,
rotated comparison harness `b841df5`. No index-format or strategy-policy change.
The sparse reader decodes varints directly into a complete page mask, retaining
one lookahead Tid. Unlike the parked wrapper specialization, this bypasses
SparseCursor and its per-posting enum/ordinal/current handling. It is scalar
page-at-a-time decoding, not multi-page batch consumption or explicit SIMD.

Two randomized/malformed-stream tests passed, followed by all 124 active segment
tests (four manual probes ignored) and 310 TinQL tests. Full PostgreSQL/real-query
performance validation remains necessary before promotion.

## Focused second-size pilot

36,000 heap pages, nine rotated repetitions per strategy, exact row membership
checked before timing; final count/page count checked each measurement. Same
binary compares fused pages and original scalar-to-page adaptation. Construction
of synthetic inputs is outside timing; cursor construction is inside. These
are whole synthetic union timings, not isolated decode CPU time or database QPS.

| Fixture | Original adapter ms | Fused ms | Reduction |
|---|---:|---:|---:|
| Rare short | 0.263 | 0.265 | -0.8% |
| Sparse wide | 4.446 | 4.448 | approximately 0% |
| Common short | 2.989 | 2.736 | 8.5% |
| Common wide | 26.355 | 23.444 | 11.0% |
| Dense wide | 12.720 | 12.605 | 0.9% |
| Overlapping segments | 7.655 | 6.851 | 10.5% |

[Raw timings](fused-sparse-micro.csv). Grouped encodings retain the same direct
mask decoder in both arms; sparse control uses the scalar postings cursor and
Rows adapter. An earlier 12,000-page pilot likewise favored common/overlapping
cases, but sparse-wide was about 5% slower. Do not derive a selection threshold
from these fixtures or call this an end-to-end speedup.

Next: compare frozen original/fused libraries on the million-row snapshot,
forcing pages in both arms to isolate execution. Then default-policy behavior,
per-query regressions and eight-client clean/mutated loads. Only if evidence
supports different winners should a cheap pre-execution feature gate be tested
on fresh query identities, charging decision time to execution. Preserve sparse
fallback and do not enable a globally tuned rule from retrospective winners.
