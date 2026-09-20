# Grouped pivot bounds for wide ranked disjunctions

The disjunction walker previously folded all term bounds after adding every term to the pivot prefix. When many cursors share one document TID, several folds select the same possible pivot. The change adds the whole equal-TID group before testing its bound. It applies only when a source has at least 32 scoring cursors; smaller cases keep the existing per-term fold.

## Correctness argument

The original walker extends its selected pivot to include every cursor at that TID before checking block bounds. Term upper bounds are nonnegative. Adding the rest of an equal-TID group cannot undo a successful threshold comparison; testing at the group end therefore chooses the same pivot TID. The canonical left-to-right f32 fold is retained. This does not replace it with a differently ordered running sum. Exact scoring, tie ordering, block pruning, storage, and heap visibility rules are unchanged.

This removes repeated folds within a group, not every source of wide-query cost. The worst case with distinct cursor TIDs remains unchanged in complexity. The 32-term cutoff is a conservative measured choice, not a claimed universal crossover point.

## Measurements

Four restarted rounds baseline/candidate/candidate/baseline. Nine interleaved observations per case, first two discarded; arithmetic mean of two round medians. Descriptive ratios, not confidence intervals.

Both datasets ran locally with one client, four Docker CPUs and 4 GiB RAM per server. Builds and native tests did not run alongside measurement. Unrelated host services remained running. The synthetic fixture uses session-local temporary tables. The Wikipedia run uses 100,000 real documents and normal 32,768-document segment sizing, yielding four immutable sources. No cloud resources or TIN server were used.

| Dataset / ranked query | Baseline ms | Candidate ms | Reduction |
|---|---:|---:|---:|
| synthetic / or_2_ranked | 0.498 | 0.510 | -2.5% |
| synthetic / or_8_ranked | 2.567 | 2.497 | 2.7% |
| synthetic / or_32_ranked | 16.499 | 13.233 | 19.8% |
| synthetic / or_128_ranked | 146.705 | 56.541 | 61.5% |
| wikipedia / or2 | 2.147 | 2.112 | 1.6% |
| wikipedia / or8 | 6.813 | 6.630 | 2.7% |
| wikipedia / or32 | 51.809 | 45.989 | 11.2% |
| wikipedia / and2 | 1.360 | 1.336 | 1.8% |
| wikipedia / or2_filtered | 6.222 | 5.825 | 6.4% |

Read small percentage changes as diagnostic variation. Every case and both round medians are in the JSON companion; no unchanged controls are hidden. Some synthetic fallback cases varied noticeably between rounds, including a prefix-query outlier. The defensible result is the large repeated wide-OR improvement, supported by the smaller real-corpus 32-term win, not a universal speedup.

The earlier unguarded experiment improved the wide case but did not demonstrate a short-query gain. Both the unguarded and final evidence are retained. The final short-query Wikipedia controls do not show a slowdown.

## Validation and provenance

- All 44 synthetic cases passed correctness in each of four rounds (176 case validations).
- All seven Wikipedia cases passed exact ranked score-bit/membership/cardinality checks in each of four rounds (28 validations).
- All 126 PostgreSQL tests passed, including the new wide-disjunction regression. It exercises widths 31/32/33/128, boosts, ties, four immutable sources plus mutable postings, updates/deletes, and LIMIT/OFFSET. It compares IDs and score bits with exhaustive ranking and asserts that the wide query selected block-max pruning.
- Clippy with warnings denied passed for the engine change. No diagnostic timers remain.

Measured baseline image: `stannum-bench:ranked-exists`, commit `4d4b4c6`. Final candidate image: `stannum-bench:grouped-wand-wide`, commit `fefc918`. Subsequent changes add the regression/report and rebase onto the merged correctness fix; the measured algorithm is unchanged. Source/image manifests, fixture hashes, exact SQL, every observation plan, and orchestration scripts are retained in the evidence archive.

These are same-engine optimization measurements. They do not compare Stannum with TIN, prove production concurrency capacity, or substitute for the separate LED gate. The original full query-shape catalog and limitations remain in [query-shapes.md](query-shapes.md).

Local archive: `benchmarks/results/grouped-wand-evidence.tar.gz` (2,211,538 bytes), SHA-256 `f2b4d8b92bd285f8c157f345a9421fcb1cef7d306d7ee231408dd7722722fa29`.
