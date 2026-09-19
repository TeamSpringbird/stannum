# Generic prepared-plan ranking

PR #28 made bound custom plans eligible for ranked scans. Generic plans still
retained `$1`, while ranked-path recognition required a constant query. The
result was a bitmap or sequential scan followed by scoring and a top-N sort.

The existing prepared-query regression now requires the ranked path under both
`force_custom_plan` and `force_generic_plan`. It reuses each statement beyond
five executions, changes term/Boolean/phrase queries, and compares exact ordered
score bits with materialized exhaustive scoring. Automatic plan selection is
checked for correctness without prescribing PostgreSQL's custom/generic choice.

## Diagnosis

The test-only commit `425c6ab` reproduced the generic fallback in CI. On ARM64
PostgreSQL 18, 119 tests passed and the new assertion failed on `common OR rare`:
the plan used a bitmap heap scan and top-N heapsort. This is the intended red
regression, not a newly introduced correctness failure. Saved earlier local
plans also show the performance gap; their timings are diagnostic examples,
not a general speedup claim.

The planner rejected the parameter before offering a ranked custom path.
Matching the query expression in the predicate and scoring call is necessary;
changing costs alone cannot make an absent path win.

## Change and scope

A ranked path may now use a direct external text parameter when the search and
bound scoring expressions refer to the same parameter and index. The parameter
is copied into `custom_exprs`, where PostgreSQL handles expression traversal,
parameter accounting and plan copying. Unknown queries use the existing fallback
estimate rather than pretending the planner knows the value.

Executor initialization compiles the parameter expression. First access binds
and copies the query text, then uses the existing ranked scorer and block bounds.
NULL returns no rows. Rescan forgets the scan-owned scorer and ranked results
before rebinding. Plain EXPLAIN initializes state without executing the query.
A separate regression forces nested-loop rescans with an additional row filter.
Malformed-query execution is also followed by valid execution of the same plan.

This does not add arbitrary query-expression evaluation, correlated-parameter
ranked paths, dynamic scoring arguments, or parameterized unordered/count custom
scans. A runtime LIMIT cannot provide a constant top-k pruning bound. Existing
constant-clause paths remain available when a parameterized clause cannot
supply the requested ordering. There is no scoring-formula or on-disk change.

## Validation status

The implementation is being validated on PostgreSQL 17 and 18, x86-64 and ARM64.
The existing Lead oracle provides separate semantic coverage; the generic-plan
regressions compare with exhaustive scoring directly. The million-document
benchmark in PR #31 runs against its original, unchanged extension image and
must not be reported as a measurement of this follow-up.
