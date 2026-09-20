# Small TIN contract checks against Lead

The opt-in `--boundaries` mode in `benchmarks/oracle.py` checks documented search
contracts with exactly 100 nonempty documents. It leaves the existing 47 shapes
across five mutation states and the 906-form Wikipedia campaign unchanged.
This is semantic verification, not a performance benchmark or a claim about
closed-source TIN behavior.

Run through the existing launcher with a disposable local PostgreSQL server:

```sh
BOUNDARIES=1 LEAD_REF_DIR=/path/to/clean/pinned/lead \
  script/reference-oracle benchmarks/results/tin-boundaries
```

The launcher builds both extensions and records the actual Lead revision. Its
boundary verification budget defaults to 120 seconds, excluding builds. On a
shared development machine, hold `/tmp/stannum-pgrx.lock` across installation,
server use, and cleanup. Existing installation users can invoke `oracle.py
--boundaries --left local-stannum.env --right local-lead.env --reference-source
/path/to/lead --output /new/result/path` directly. That path records checkout and
installed library fingerprints but cannot independently prove how a preinstalled
library was built; the launcher provides that build linkage.

The 24 cases (19 distinct probes and five top-level projection controls) cover:

- Density at 9, 10 and 11 occurrences per 100 documents, with and without an
  explicit unit boost; a ratio above one disabling density elision.
- `max_score` constant across every match and equal to that statement's exhaustive
  maximum. This is checked within each engine, independently of cross-engine rank.
- `term_add` and `term_replace`, their mutual exclusion, inconsistent arguments
  between scoring calls, and mixing `score` with `full_score`.
- Empty/zero-token text versus invalid empty phrase, alternatives and standalone
  NOT syntax, preserving complete errors and comparing SQLSTATE.
- Exact explicit-query highlight labels and the disputed BEFORE example.

Cross-engine checks compare membership, ranking, zero-score membership, maxima
invariants and exact highlighting. They intentionally do not compare score bits:
existing Lead differences in corpus statistics make that a separate contract.
Every unexpected error remains a failure. Matching rejections are accepted only
for cases explicitly expected to fail; missing capabilities are never skipped.
Reports retain SQL, raw results, diagnostics, callable function signatures and
classification of engine disagreements versus documented/invariant failures.
Classifications guide investigation; they do not automatically assign fault.

See [the documentation audit](../research/tin-search-documentation.md) for the
source contracts and the unresolved BEFORE prose/example conflict. Follow-up
coverage still includes statement lifecycle/DML discovery, changed prepared
parameters, partitions, locks and physical maintenance; this suite does not
claim to cover those with its small read-only fixture.

## Measured result, 2026-09-20

The [retained observations](tin-contract-boundaries-results.json) came from local
PostgreSQL 18.6 on macOS ARM, using Lead
`bd95c7e51b6afce81396790852ee2f2c169570ad` and the Stannum engine source recorded
in the report. Both were freshly installed from clean engine source under the
shared lock; no remote database was used. Verification took **1.55 seconds**,
excluding installation and cluster setup. The isolated cluster was removed.

All **24 cross-engine comparisons agree**. However, only **13 cases satisfy the
documentation-derived expectations/invariants**. Eleven cases report shared
failures: six distinct findings plus five projection controls. The opt-in suite
therefore exits nonzero; these discrepancies are deliberately not allowlisted.
They do not establish that Stannum differs from Lead, or that closed-source TIN
behaves like either engine.

| Finding | Both Lead and Stannum | Interpretation |
| --- | --- | --- |
| Default density at 10/100 | Scores remain positive; 11/100 elides | The documented decimal boundary differs from actual float32 threshold behavior. |
| `max_score` with ratio 1.1 | Maximum differs from exhaustive configured scores | Nondefault sibling scoring policy is not fully propagated. |
| `max_score` with `term_add` | Maximum differs from exhaustive configured scores | Same policy-binding question. |
| `max_score` with `term_replace` | Maximum differs from exhaustive configured scores | Same policy-binding question. |
| Two inconsistent density arguments | Query succeeds | Documentation says these arguments must agree. |
| `score` mixed with `full_score` | Query succeeds | Documentation says these cannot share a scanned relation. |
| BEFORE highlighting | Both the witnessing `alpha` and qualifying `gamma` are wrapped | Supports the documentation example, not its conflicting prose. |

The [scoring documentation](https://planetscale.com/docs/postgres/search/scoring)
was rechecked for these policy and maximum expectations. The
[function reference](https://planetscale.com/docs/postgres/search/reference/functions)
also states that the maximum is constant across matches.

The density result follows the shared implementation: converting float32 `0.10`
to float64 before multiplying by 100 gives approximately `10.000000149`, so an
integer document frequency of ten is below the threshold. This is a source-based
explanation, not an observation of TIN internals. Five controls project score
functions as independent target entries instead of nesting them inside JSON;
the scoring-policy discrepancies persist in those controls.

Membership, default/pinned maxima, unit-boost retention, empty versus invalid
syntax, term-add/replace mutual exclusion, and explicit highlight labels agree.
Errors retain SQLSTATE and full diagnostics. No expansion-score exception from
the older oracle applies to this suite, and no score-bit equality is claimed.

Next steps are to clarify these contracts upstream before changing engine
behavior: the project's Lead compatibility goal argues against silently changing
Stannum alone to match prose. Keep merge-memory work ahead of that investigation.
This small suite can be rerun against a newly pinned Lead revision independently
of the approximately fifteen-minute published-trace check.

Validation: all **144 benchmark harness unit tests** pass, including 16 oracle
tests; shell syntax and source-header checks pass. No Rust files changed.
