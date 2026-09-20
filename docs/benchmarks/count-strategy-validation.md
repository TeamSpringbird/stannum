# Count strategy validation and query diagnostics

`benchmarks/count_visibility.py` is an integration check for the experimental
`stannum.force_count_pages` control. It creates a uniquely named schema in a test
database and removes that schema in a finally block. It requires psycopg and
libpq connection settings. Do not run against production or during measurements.

```sh
python benchmarks/count_visibility.py > count-visibility.json
```

Seven query shapes cover single terms, AND, OR, nested Boolean operations,
repeated terms, and absent terms. Each explicitly exercises scalar and page
bitmap count plans and compares against independent regular-expression predicates.
The fixture uses sparse terms, 16 overlapping immutable segments, and a write
buffer. It checks a vacuumed baseline, a repeatable-read snapshot retained across
another connection's committed HOT-eligible payload updates, deletes, indexed
text updates and insertion, a fresh snapshot, and a post-VACUUM snapshot.
Heap fetches in the fresh snapshot prove coverage of the visibility-checking path.
Payload updates are HOT-eligible; the check does not claim every update was HOT.

Local PostgreSQL 18 validation passed all 56 combinations. The retained snapshot
kept its prior results and the fresh snapshot observed the committed changes.
This is a deterministic two-session visibility test, not a concurrency stress
benchmark or exhaustive oracle run. It currently runs manually rather than in CI.

## Per-query before/after report

```sh
python3 benchmarks/query_comparison.py BASELINE_RUN CANDIDATE_RUN --output NEW_OUTPUT
```

Inputs must be completed single-engine Stannum runs with the same existing
comparison contract (trace, corpus, settings, clients, machine characteristics,
correctness evidence, and starting visibility state). The tool rejects query
errors, truncated timing exports, missing samples, and differing measured query
sets. No input files are modified. Output includes JSON and Markdown.

Each query has sample counts, mean, p95, summed query-time share, percent change,
and contribution to mean latency under the baseline's query-frequency mix.
Positive changes mean slower execution. Baseline weighting prevents a faster
candidate's changed completion frequencies from masquerading as an improvement.
Summed query time is neither elapsed time nor CPU time. Single-run comparisons
are diagnostic; use repeated trials to establish repeatability.

Validation: five unit tests cover weighting, coverage mismatch, identity,
invalid samples, stale exports, and timed errors. An identity comparison against
the saved full AWS eight-client run covered all 302 forms with exactly zero
weighted change. This does not yet demonstrate a performance gain.
