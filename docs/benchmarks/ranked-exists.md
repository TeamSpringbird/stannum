# Ranked EXISTS and NOT EXISTS binding

The [query-shape suite](query-shapes.md) found two SQLSTATE XX000 errors on the
baseline: ranked EXISTS and NOT EXISTS queries could not bind `full_score(ctid)`.
They reproduce outside the validation wrapper.

PostgreSQL's `pull_up_sublinks` can wrap the original FromExpr in a semi/anti join,
then add a new top-level FromExpr with empty quals. Scoring support previously
searched only that empty top-level qual. The search predicate still existed in
the nested FromExpr. See [PostgreSQL's implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/optimizer/prep/prepjointree.c).

The fix searches WHERE quals throughout the join tree, preserving parent-first
binding order. It follows join children without adding JOIN ON expressions or
crossing nested query scopes to the binding rules. Stack-depth checks protect
recursive traversal. Index selection, scoring arithmetic, and storage are unchanged.

## Validation

A regression test failed before the fix with the original scorer-binding error.
Afterward, all 125 PostgreSQL tests passed, along with Clippy (-D warnings) and
formatting. The new test compares ranked results against materialized unfiltered
scores with custom scans enabled/disabled, forced generic/custom prepared plans,
stacked EXISTS/NOT EXISTS, and an inner alias shadowing the outer relation.

The complete 44-case suite now passes at both 4,096 and 16,384 synthetic documents:
[4,096 results](ranked-exists-4096-results.json),
[16,384 results](ranked-exists-16384-results.json).
The baseline passed only 42/44. Each case retains independent membership checks
and the same-engine score-multiset oracle described in the suite documentation.
This is not a new independent LED comparison; CI retains the existing Lead oracle.

These measurements used the frozen `stannum-bench:ranked-exists` image built from
`4d4b4c6`, before rebasing onto the merged benchmark PR. They are local, warm,
single-client diagnostics. Image/build provenance and full plans are under
`benchmarks/results/ranked-exists-build`, `ranked-exists-4096`, and
`ranked-exists-16384`. No speedup is claimed for statements that previously errored.
