# Streaming unordered search

Implementation: `4faff8f`, branch `perf/streaming-boolean-scans`, following the
[page-bitmap count work](page-bitmap-counts.md).

## Execution

Unordered custom searches now merge candidate cursors as the executor requests
rows. Dense Boolean queries use page masks; sparse and positional queries use
scalar cursors. Both strategies remove dead postings and deduplicate across
sources without collecting and sorting the complete result set first.

The stream owns a boxed view of the index sources. Its private borrowed cursors
are dropped before that owner. FETCH calls retain this captured view, and
executor rescans rebuild cursors against the same view. Buffer appends, cache
refreshes, and directory changes therefore do not replace an active scan's
candidate set. Heap visibility, HOT-chain following, conservative text rechecks,
and remaining SQL filters still run before a row is returned.

`EXPLAIN ANALYZE` reports `Candidate Strategy: streaming page bitmaps` or
`streaming scalar`, plus `Candidates Visited`. A LIMIT can stop before the total
cardinality is known; visited candidates are not reported as the total match
count. Candidate traversal CPU moves from planner startup cost into run cost,
but estimated index I/O remains in startup: opening terms can still fetch their
encoded postings bytes up front.

This bounds decoded candidate buffering, not all index memory or I/O. Ranked
retrieval, PostgreSQL bitmap scans, and the adaptive Count node retain their
existing execution strategies. The index format is unchanged.

## Validation

- 438 core unit tests and four documentation tests passed, with three optional
  probes/fixture writers ignored. Existing randomized page-set tests now consume
  offset masks through the new destructive lowest-offset iterator as well.
- 105 extension tests passed on each of PostgreSQL 17.11 and 18.6 locally.
- Formatting and Clippy with warnings denied passed for both PostgreSQL majors.
- New SQL regressions verify LIMIT consumption, paused cursors across same-backend
  writes/cache refreshes, additional SQL filters, and nested-loop rescans.
- The lifecycle harness pauses a cursor while another backend changes every
  matching document, vacuums, folds new writes, and vacuums again. The old cursor
  returns exactly its original snapshot; a new query sees only the new matches.
- Existing lifecycle, standby, recovery, error-cleanup, and concurrent ranked
  smoke checks passed against the integrated build: 335 standby snapshot answers
  with no wrong answers (one expected recovery conflict), and 766 ranked
  comparisons covering 66,437 rows across five concurrent smoke scenarios.
- All six [CI jobs](https://github.com/TeamSpringbird/stannum/actions/runs/35402817435)
  passed at the implementation commit, including PostgreSQL 17/18 on ARM64 and
  x86-64, formatting/harness checks, and the upstream Lead reference oracle.

## Measurement protocol

The baseline is `60f0e89`, the previous page-count implementation, rather than
Lead, GIN, or TIN. Both release libraries read the same baseline-built indexes
in disposable PostgreSQL 18.6 databases on native ARM64 macOS. Shared buffers
are 512 MB and maintenance memory is 256 MB; there are no container CPU or
memory limits. The machine-wide installation lock prevents tests/installations
from changing the extension during a measurement; binary hashes are checked
before and after each window, and fresh backends load each selected library.

Each fixture has 100,000 documents: the existing synthetic fixture and the
verified Wikipedia dataset. Five pairs alternate baseline/changed order.

The LIMIT workload returns `id` with `LIMIT 10` for every existing count-case
predicate. The synthetic workload also includes `common AND filler`,
`common OR filler`, and `common AND NOT rare`. Two clients run five seconds of
warmup and ten seconds of measurement, with no writes. Full match sets for the existing fixture cases are
checked against the independent fixture oracle before each window. All LIMIT
queries are also checked for duplicate IDs and the ten-row upper bound; the
three additional synthetic predicates are not part of that fixture oracle.

Each window separately measures a full traversal using `sum(id)` with two
clients, two seconds of warmup, and five seconds of measurement. Its predicate
is `common AND filler` for the synthetic fixture and `history` for Wikipedia.
SUM ensures that PostgreSQL consumes every matching row rather than using the
custom Count optimization. Plans and per-query pgbench samples are retained.

These are short, warm-cache, read-only measurements. They do not establish
cold-cache, larger-than-memory, or sustained-write performance. Latencies refer
to the median of per-window p50 samples unless otherwise specified.

## Results

All twenty measured windows completed without reported transaction failures.
The existing fixture-oracle comparisons had zero differences throughout.

| LIMIT 10 workload | Baseline median queries/s | Streaming median queries/s | Ratio |
| --- | ---: | ---: | ---: |
| Synthetic | 2,421 | 37,589 | 15.53× |
| Wikipedia | 6,234 | 37,325 | 5.99× |

Per-pair aggregate throughput (baseline → streaming, queries/s):

| Pair | Synthetic | Wikipedia |
| --- | ---: | ---: |
| 1 | 2,399 → 38,248 | 6,230 → 37,500 |
| 2 | 2,415 → 37,589 | 6,194 → 37,325 |
| 3 | 2,421 → 36,563 | 6,234 → 36,421 |
| 4 | 2,458 → 36,594 | 6,354 → 36,313 |
| 5 | 2,438 → 37,873 | 6,302 → 37,399 |

Selected per-query LIMIT p50 latencies:

| Query | Baseline ms | Streaming ms |
| --- | ---: | ---: |
| Synthetic dense AND | 1.679 | 0.049 |
| Synthetic dense OR | 1.264 | 0.049 |
| Synthetic AND NOT | 2.478 | 0.051 |
| Synthetic rare term | 0.049 | 0.041 |
| Wikipedia common term | 0.169 | 0.041 |
| Wikipedia AND | 0.411 | 0.049 |
| Wikipedia common phrase | 1.730 | 0.048 |
| Wikipedia rare term | 0.043 | 0.041 |

Full-traversal p50 latencies, measured separately:

| Fixture | Baseline median ms (range) | Streaming median ms (range) |
| --- | ---: | ---: |
| Synthetic | 9.553 (9.138–9.965) | 6.232 (6.000–9.826) |
| Wikipedia | 3.153 (2.824–4.057) | 2.686 (2.473–3.977) |

LIMIT throughput improved in every pair. Full traversals were noisier: the
changed build was slightly slower in synthetic pair three and Wikipedia pair
four. The lower median full-traversal latency is encouraging, but needs longer
runs before treating it as a stable percentage improvement.

Changed synthetic plans confirm that dense LIMIT queries visit ten candidates
and fetch ten heap tuples; the full traversal visits and fetches all 100,000.
This directly verifies early stopping rather than merely a different count
plan. Small selective/missing-result cases did not show a material regression
in these measurements. There is no direct TIN or GIN comparison here.

## Artifacts

Raw logs, plans, correctness results, comparison JSON, library metadata,
validation/benchmark drivers, and CI metadata are retained locally under the
ignored `benchmarks/results/streaming-search/` directory. The disposable server
data directory is excluded. The installed library was verified as the changed
build after measurement and the disposable server was stopped.

Release library SHA-256 values:

- Baseline: `7f5b8aff06719e7fd68bdb64b26962e064b8c5a620775bd86faf30fa96d131fa`
- Streaming: `7f134858587029c2eb57bb3a0aca9ceec9fea57bf09613b839c2534b677b2ee5`

The next performance target is foreground buffer-fold/merge stalls under
sustained writes. These read-only results do not resolve that bottleneck.
