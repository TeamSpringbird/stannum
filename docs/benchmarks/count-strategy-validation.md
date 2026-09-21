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

## Seeded Boolean and mutation fuzzing

`count_fuzz.py` builds an AST for each generated AND/OR expression, renders that
AST as TINQL, and evaluates it independently in Python against token sets read
from the reader's snapshot. Both exact IDs and custom COUNT results must agree.
It runs default selection and forced page counting; default selection can itself
choose pages, so the receipt records actual plan strategies rather than assuming
that force-off means scalar. Generated expressions have depth up to five and
include dense, sparse, repeated and absent terms.

```sh
python benchmarks/count_fuzz.py --seed 20260920 --seeds 3 --output NEW_OUTPUT
python benchmarks/count_fuzz.py --replay NEW_OUTPUT/20260920-fixture.json --output REPLAY_OUTPUT
```

Run under the same installation lock and disposable PostgreSQL environment as
the deterministic visibility check. Fixtures are written before execution and
include documents, ASTs, write-buffer size and the complete mutation schedule.
Failures preserve the active seed, phase, query, mode and error; a unique schema
is dropped afterward. Replay reads the saved fixture directly. Inputs must be
trusted locally generated fixtures.

The default schedule performs two committed mutation batches and one rolled-back
batch, with insertion, deletion, indexed-text and HOT-eligible payload updates.
It vacuums while a repeatable-read snapshot is retained, then checks a fresh
snapshot and vacuums again after releasing the old one. These are deterministic
interleavings, not a scheduler-race or long-running production stress test.

Local PostgreSQL 18 validation passed 6,264 checks across seeds 20260920–20260922,
87 expressions per seed, three mutation rounds and four snapshot phases. The
fixtures used different segment layouts. Raw receipts and fixtures are retained
in `benchmarks/results/count-fuzz-analyzed/`. A saved-fixture replay is retained
in `benchmarks/results/count-fuzz-replay/`. No production strategy or storage
format changed, and this does not establish performance or full TINQL coverage.
