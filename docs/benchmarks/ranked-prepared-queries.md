# Ranked prepared-query diagnosis

September 19, 2026. The published trace exposed a planning difference between
literal and parameterized ranked queries. On the 1000-document Wikipedia
fixture, the same disjunction took roughly 63–80 ms through a prepared statement
and less than 1 ms as a literal. Both selected `id`, `body`, and BM25 score.

## Cause and change

`score_support` copies the search query from the original parse-tree quals to
bind `stannum.full_score(ctid)` to its index. Those quals have not necessarily
been simplified. Even a custom prepared plan could therefore contain a constant
search predicate but a `$1` argument in `score_bound_indexed`. Ranked-path
recognition requires the score and search query to expose the same constant.
Without that recognition, the broad disjunction used a sequential scan followed
by a top-N sort instead of Stannum's block-max ranked scan.

The fix calls PostgreSQL's `eval_const_expressions` on the copied query using
the current planner context. Custom plans can expose their bound value; generic
plans retain parameters and their existing fallback. No planner settings are
forced during measurement. No scoring formula, storage format, or pruning
algorithm changes.

A minimal reproduction against the benchmark's `documents` table is:

```sql
PREPARE ranked(text) AS
SELECT id, body, stannum.full_score(ctid) AS score
FROM documents WHERE body ==> $1 ORDER BY score DESC LIMIT 10;
SET plan_cache_mode = force_custom_plan;
EXPLAIN (ANALYZE, BUFFERS)
EXECUTE ranked('wisconsin OR attorney OR general');
DEALLOCATE ranked;
RESET plan_cache_mode;
```

Before the fix, the score sort key retained `$1`. Afterward, custom plans can
select `Stannum Text Search Scan`, `Order: score DESC`, and `Top K: 10`.
Generic-plan ranked execution remains a separate optimization opportunity;
PostgreSQL can legitimately select a generic plan under `plan_cache_mode=auto`.

## Validation

The real-server regression failed before the change with a sequential scan and
sort. It passes with the fix, asserting ranked-plan selection for custom plans
and comparing exact ordered score bits with exhaustive scoring. It changes
query parameters repeatedly beyond the first five executions and checks custom,
automatic, and generic plan modes, including queries with no matches.

The full PostgreSQL 17 and 18 suites each passed all 120 tests. A read-only review found no
blocking correctness issue.

An initial candidate smoke used the unchanged pinned driver, 1000 documents,
two clients, five seconds of warmup, twenty seconds of measurement, and a fresh
native ARM64 PostgreSQL 18.6 container capped at four CPUs and 4 GiB RAM. It
measured all 906 query forms and passed all 906 exhaustive same-engine top-10
checks. Overall throughput was 5270.1 queries/sec with p95 0.814 ms; disjunction
p95 was 0.898 ms. The original observation was 147.6 queries/sec with overall
p95 68.846 ms. These separate observations are diagnostic, not a paired estimate.
Raw candidate artifacts: `benchmarks/results/ranked-parameters-smoke`.

Alternating baseline/candidate measurements use the new `tin.py compare` command.
The benchmark also captures parameterized ranked plans; the original plan
artifacts only explained literal count queries and missed this distinction.

## Controlled paired result

`ranked-parameters-paired` completed four fresh-container trials in the order
baseline, candidate, candidate, baseline. Each used the same 1000-document
prefix, pinned driver, seed, runtime settings, two clients, five-second warmup,
and thirty-second measurement. Every trial measured all 906 forms and passed
membership and exhaustive top-10 checks; all 906 full query counts agreed
between images. The only changed source files were `postgres/src/score.rs` and
its regression tests in `postgres/src/lib.rs`.

| Trial | Image | Queries/sec | p95 ms |
| --- | --- | ---: | ---: |
| 1 | Baseline | 147.0 | 72.869 |
| 2 | Candidate | 5324.8 | 0.805 |
| 3 | Candidate | 5274.7 | 0.814 |
| 4 | Baseline | 148.1 | 72.679 |

Median paired throughput improvement: **35.920x**, range **35.627–36.214x**.
Median p95 across baseline trials was **72.774 ms**, versus **0.8095 ms** for
the candidate. Two pairs describe observed variation, not a confidence
interval. This is a small, resident-corpus diagnostic and does not estimate
production capacity or compare relevance with another search engine.

The [compact results](ranked-prepared-results.json) preserve image/source
identities, the driver hash, trial metrics, correctness summaries, and the
aggregate artifact hash. Raw trial artifacts remain under
`benchmarks/results/ranked-parameters-paired` in the benchmark worktree. The
new runner rejected no compatibility or coverage checks for this comparison.

## 100k-document ranked baseline

`ranked-parameters-100k` ran the candidate with two clients, ten seconds of
warmup and sixty seconds of measurement under the same four-CPU/4-GiB server
limits. It completed **1741.1 queries/sec**, with **p95 2.150 ms** and **p99
4.097 ms**. All 906 forms were measured. All 906 exhaustive same-engine top-10
checks and the 1000-row membership sample passed. This is a candidate baseline,
not a paired 100k speedup estimate. The corpus remains resident in memory.

| Family | p50 ms | p95 ms | p99 ms |
| --- | ---: | ---: | ---: |
| Conjunction | 0.888 | 1.927 | 2.758 |
| Disjunction | 1.081 | 2.453 | 4.435 |
| Phrase | 0.787 | 2.081 | 5.589 |

The slowest individual forms are now useful, bounded profiling targets:

- Source 302's long disjunction: p50 11.158 ms, p95 13.257 ms.
- `"to be or not to be"`: p50 9.655 ms, p95 12.457 ms.
- `"the book of life"`: p50 7.897 ms, p95 11.099 ms.

Each has only about 115–116 timed samples, so no per-query p99 is reported.
Profile these on a larger corpus before changing codecs or adding SIMD. Runtime
ranked paths for generic prepared plans remain another distinct opportunity.

The paired runner was validated separately by 107 Python tests and a review
that caught acceptance of requested update workloads with zero completed writes.
That case now fails closed. Re-rendering this read-only comparison with the
hardened check still passes. No old mutation/VACUUM harness was removed.
