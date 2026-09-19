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
