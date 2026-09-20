# SQL lifecycle checks against Lead

A four-document differential fixture agrees across Stannum and pinned Lead on
all 14 observations. Ten cases satisfy their explicit expected results; four
outer projections lose implicit highlighting context in both engines. These are
shared behavior/documentation questions, not Stannum-only failures.

The [highlighting documentation](https://planetscale.com/docs/postgres/search/highlighting)
states that implicit queries can be discovered through CTEs, subqueries and DML.
The [operator reference](https://planetscale.com/docs/postgres/search/reference/operator)
allows prepared query parameters. This suite makes those two claims testable
without changing either engine or the existing oracle campaigns.

## Findings

| SQL shape | Stannum and Lead |
| --- | --- |
| Direct SELECT implicit highlighting | Expected membership and exact HTML |
| UPDATE RETURNING implicit highlighting | Expected HTML for the updated document |
| Inline CTE, outer implicit highlight projection | Missing-binding error |
| MATERIALIZED CTE, outer implicit highlight projection | Missing-binding error |
| Derived-table subquery, outer implicit highlight projection | Missing-binding error |
| Writable CTE, outer implicit highlight projection | Missing-binding error |
| Separate explicit-query controls for those four outer projections | Expected membership and exact HTML |
| Highlight inside MATERIALIZED CTE, outer projection of rendered text | Expected HTML |
| Highlight inside writable CTE RETURNING, outer projection of rendered text | Expected HTML |
| Forced custom prepared plan, changing parameters | Expected membership, HTML and six custom plans |
| Forced generic prepared plan, changing parameters | Expected membership, HTML and six generic plans |

The failures concern **moving the implicit highlighter outside the query level
that contains its matching predicate**. They do not establish that all CTE or
DML highlighting is unsupported. Exact explicit-query controls are separate
statements, so the failing implicit call cannot hide their successful results.
The errors and their SQLSTATE remain in the report; no case is skipped.

Prepared statements execute `alpha`, `beta`, an absent term, an empty query,
`eclair` matching accented text, and `alpha` again within one connection. Concrete
expected rows catch stale parameter reuse, incorrect empty results and dropped
repeated observations. `pg_prepared_statements` counters verify that the requested
custom/generic path actually ran. Both implicit and explicit HTML must match
handwritten expectations, as well as matching the other engine.

DML cases change document text inside transactions that are rolled back. No score
bits or mutation-statistic invariants are compared. This is not a test of locking,
physical recovery, production concurrency or partition scoring. Multi-column
scoring, partition ranking and multi-session locking remain separate follow-ups.

## Reproduce

```sh
LIFECYCLE=1 LEAD_REF_DIR=/path/to/clean/pinned/lead \
  script/reference-oracle benchmarks/results/tin-lifecycle
```

Use a disposable local PostgreSQL instance and hold `/tmp/stannum-pgrx.lock`
across installation, server use and teardown when sharing the development host.
The launcher installs both engines and records the reference revision. Direct
`oracle.py --lifecycle` also accepts the existing `--left`, `--right`,
`--reference-source` and `--output` arguments. The verification budget defaults to
120 seconds in the launcher, excluding builds. Lifecycle, boundary and published
trace modes are mutually exclusive.

The [retained report](tin-sql-lifecycle-results.json) records local PostgreSQL 18.6,
Lead `bd95c7e51b6afce81396790852ee2f2c169570ad`, Stannum source identity, installed
library hashes, callable signatures, exact SQL, errors and all observations.
The actual run took **0.84 seconds**, excluding installation and cluster setup.
Its result is intentionally `mismatch` with exit 1 because the four shared errors
contradict the tested documentation expectation. All 14 cross-engine normalized
comparisons agree; closed-source TIN itself was not tested.

The shared runner's boundary regression was rerun separately, preserving the
same 24 cases and 11 shared documentation/invariant findings. The original 47-shape,
five-state oracle and 906-form approximately 15-minute campaign remain unchanged.

Validation: all **148 benchmark harness tests** pass, including 20 oracle tests.
Shell syntax and source-header checks pass. No Rust engine code changed.
