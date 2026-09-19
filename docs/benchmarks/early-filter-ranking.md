# Early integer filtering: correct prototype, generally slower on this workload

The default-off `stannum.experimental_early_filter` prototype passes the local
score checks, but should **not** become an automatic execution strategy on this
evidence. Fetching every text candidate to evaluate an SQL predicate usually
costs more than the scoring it avoids. A second comparison of existing native
bitmap paths also rejects a blanket strategy switch. The clearest next target
is avoiding expensive completion of an insufficient ranked prefix, with native
bitmap execution retained as a measured alternative for narrowly selective OR.

This report describes the experimental source on branch
`perf/early-filter-prototype` ([PR #39](https://github.com/TeamSpringbird/stannum/pull/39)).
The experimental GUC is not available on main; reproducing the first comparison
requires the measured prototype source/image identified below. Publishing the
findings does not promote that implementation.

## Measured result

On 100k Wikipedia documents, the same frozen image ran with the planning-time
GUC off/on/on/off. Numbers below are the median of two run medians; each run
retained seven interleaved repetitions after discarding two. Positive change is
slower. These are instrumented server execution times, not client throughput.

| Query / SQL filter / bound | Off (ms) | On (ms) | Change |
| --- | ---: | ---: | ---: |
| `history OR war`, 10% IDs, literal LIMIT 10 | 5.890 | 5.034 | −14.5% |
| `history OR war`, 25% IDs, literal LIMIT 10 | 5.494 | 5.338 | −2.8% |
| `history OR war`, 25% IDs, runtime LIMIT 10 | 3.415 | 5.478 | +60.4% |
| `history`, 10% IDs, literal LIMIT 10 | 2.681 | 3.813 | +42.2% |
| `history`, 25% IDs, runtime LIMIT 10 | 2.077 | 3.905 | +88.0% |
| `"united states"`, 10% IDs, runtime LIMIT 10 | 3.605 | 4.664 | +29.4% |
| `history`, 100% IDs, literal LIMIT 10 | 0.564 | 4.323 | +667.2% |

The 10% OR/literal case improved in both paired comparisons (on/off ratios
0.829 and 0.883). The 25% OR/literal case did not: ratios 0.939 and 1.006 make
its small aggregate improvement unsuitable as a promotion argument. Controls
that did not select the prototype also moved by several percent between runs;
do not claim those timing differences as improvements from filtering.

The test covered three text shapes (`history OR war`, `history`, and
`"united states"`), five ID filters (0%, 1%, 10%, 25%, 100%), both literal and
parameterized LIMIT 10, and six unfiltered controls: **36 cases, 144 ordered
score/membership checks across four rounds, all passing**. SQL filter percentages
describe the 100k-document corpus, not the fraction of text matches.

With normal planner settings, the prototype was selected for the 10%, 25% and
100% filters. At 0%, PostgreSQL selected a primary-key index scan; at 1%, it
selected a BitmapAnd of the primary-key and text indexes followed by a bitmap
heap scan and sort. The GUC did not alter those plans. We did not force a custom
path or claim an early-filter crossover at these selectivities. Unfiltered
queries are ineligible and remained controls.

## What the counters explain

For 25%-filtered OR with a runtime limit:

| Work | Off | On |
| --- | ---: | ---: |
| Exhaustive score calls | 27,946 | 6,974 |
| Early visible heap fetches | 0 | 27,946 |
| Final visible heap fetches | 34 | 10 |
| Top-k completions | 0 | 0 |

Avoiding roughly three quarters of the scorer calls still made this query
approximately 60% slower. The prototype first fetches all text candidates to
find eligible roots, then fetches emitted survivors again. The added heap work
is explicit in the counters. This supports investigating that cost; it does not
separately measure time spent in heap access versus other phases.

The same filter with a literal limit has a different baseline: block-max
traversal reports 570 scored candidates, then one completion makes another
27,946 exhaustive scorer calls. The prototype removes that completion and
scores 6,974 survivors, but still performs 27,946 early heap fetches. It barely
breaks even. Runtime limits with residual quals remain unbounded under the
conservative rule established by the runtime-bound experiment, so their baseline
does not pay for an unsuccessful initial pruned prefix.

At 100% selectivity on `history`, ordinary block-max pruning fetches ten rows
without completion. The prototype bypasses pruning, visits all 22,063 text
candidates, fetches all of them, and scores all of them. Its roughly 7.7× runtime
is expected from this extra work. A broad filter must not automatically select
this strategy merely because it is syntactically eligible.

`Exhaustive Score Calls` excludes block-max work and projected score lookup.
`Early Filter Heap Fetches` and `Heap Fetches` count successful visible fetches
in separate phases, not physical disk reads or all attempted fetches. Full
plans and counters are retained in the artifacts below.

## Fixture, correctness and limits

- PostgreSQL 18.6 ARM64 in a local Docker container: four CPU quota, 4 GiB RAM,
  1 GiB shared buffers, 512 MiB maintenance memory, 16 MiB work memory, JIT off.
- One fixture/index reused across all four server restarts, 100,000 rows with
  IDs 1–100,000. CSV SHA256:
  `d01f490c802313190924a1edd1722d9b1c543793a8ccd0950108d813cb8b7da1`.
- Explicit VACUUM ANALYZE before the comparison; measured all-visible pages
  remained **10,467** before and after every round, out of 10,496 heap pages.
  This is a mostly visible, read-only, warm-query fixture, not a dirty/deletion
  or cold-storage result.
- Force generic prepared plans; set the GUC before PREPARE. Compare the same
  statement and image with only the experimental setting changed. Literal
  limits remain literals within prepared statements; runtime limits use a
  direct bigint parameter. Ordinary scan methods remain enabled.
- An exhaustive MATERIALIZED scoring reference with the experimental setting
  off provides score bits for every matching document. Each result must contain
  unique eligible IDs with the exact reference score bits and the expected
  descending score sequence. Tied IDs may differ where SQL leaves them
  unspecified. This is differential Stannum validation, not a replacement for
  the separate Lead semantic gate.
- Measured projection is IDs and scores. No full-body transfer, network latency,
  concurrency, mutation, million-row run, or confidence interval is claimed.

Image ID:
`sha256:a44daedaef674df8718fd5547c79600c771b50e26ba01a61b1b5421f6ce751ed`.
Its recorded source commit is `1b3a836bf422c882a1c8cf997c8e5395ce4bdab6` and
source SHA256 is
`bc6ce79d58000af27e111235da0d8b13a30125e7e8c19121a248027c1c5a43fd`.
Branch ancestry changed after image construction; the image labels and frozen
build-source manifest identify the executed implementation.

## Decision and next experiment

Keep the strategy default-off and preserve these negative results. Do not run a
larger corpus simply to search for a favorable aggregate: this fixture already
shows that syntactic eligibility and fewer scorer calls are insufficient.

Before proposing a new eligibility pipeline, we tested PostgreSQL's existing
alternative. A second same-image A/B/B/A run set
`stannum.enable_custom_scan=on/off/off/on`, with the early-filter GUC **off in
both alternatives**. The same 36 cases and 144 score/membership checks passed;
visibility again remained 10,467 pages throughout. The settings changed before
PREPARE, so the comparison measures the planner-selected core alternative, not
the early-filter prototype. Other scan methods remained enabled.

| Query / SQL filter / bound | Custom scan enabled (ms) | Core paths (ms) | Change |
| --- | ---: | ---: | ---: |
| `history OR war`, 10% IDs, literal LIMIT 10 | 5.372 | 3.621 | −32.6% |
| `history OR war`, 10% IDs, runtime LIMIT 10 | 3.341 | 3.827 | +14.5% |
| `history OR war`, 25% IDs, literal LIMIT 10 | 5.223 | 5.975 | +14.4% |
| `history OR war`, 25% IDs, runtime LIMIT 10 | 3.341 | 5.950 | +78.1% |
| `history`, 10% IDs, literal LIMIT 10 | 2.704 | 2.744 | +1.5% |
| `history`, 25% IDs, runtime LIMIT 10 | 1.958 | 4.559 | +132.8% |
| `"united states"`, 10% IDs, runtime LIMIT 10 | 3.581 | 4.491 | +25.4% |
| `history`, unfiltered, literal LIMIT 10 | 0.550 | 8.291 | +1406.2% |

The 10%-filtered OR/literal improvement held in both pairs (core/custom ratios
0.680 and 0.667). Its core plan used BitmapAnd of the text and primary-key indexes,
fetched 2,779 matching rows, and sorted them. At 25%, the same approach fetched
6,974 rows and lost to the current ranked path. Even at 10%, the core alternative
lost to the runtime-bound baseline, which already avoids the failed pruned-prefix
attempt. Thus a lower scorer count, selectivity alone, or the presence of an SQL
filter does not establish which implementation should win.

The next bounded experiment should avoid the full restart after a filtered
literal-limit prefix proves insufficient, or skip an initial pruning attempt
when evidence supports doing so. Preserve exactness for invisible rows, residual
quals, offsets, ties, cursor continuation and concurrent changes. Measure both
literal and runtime bounds; the latter is an important control because it already
skips that attempt when residual quals remain. Existing core bitmap execution is
another candidate for the narrow OR/10% case, not a globally preferable fallback.

Only if these smaller changes leave a demonstrated gap should a new ranked
eligibility-set pipeline be attempted. Integrating secondary-index TIDs into
ranking still requires HOT root identity, lossy bitmap rechecks, visibility,
memory and planning-cost work. Neither experiment proves that such integration
will beat the current ranked implementation.

Raw outputs are in the ignored directory
`benchmarks/results/early-filter-100k/`: off/on case SQL, frozen source manifest,
image labels, reference scores, all 1,296 instrumented plans, correctness checks,
and visibility snapshots. The existing `benchmarks/server_times.py` harness ran
the measurements; temporary orchestration is copied into the artifact directory.
Checked-in [cases](early-filter-cases.json), [results](early-filter-results.json),
and [representative plans](early-filter-plan-examples.json) preserve the comparison
without adding another general-purpose benchmark runner. The container and its
temporary data volume were removed after the run.

The second comparison is preserved separately in
`benchmarks/results/core-ranking-100k/`, with its explicit `comparison.json`,
another 1,296 plans, image/source metadata and correctness/visibility checks.
[Core comparison results](core-ranking-results.json) and
[representative core plans](core-ranking-plan-examples.json) are checked in.
Its container and temporary volume were also removed. Across both comparisons,
all **288** score/membership checks passed. Do not compare timings across the two
fixtures as a paired estimate; only each comparison's own A/B/B/A run is paired.

## Additional local validation and evidence archive

The prototype passed all 126 native PostgreSQL 18 tests and Clippy with warnings
denied. A 45.7-second concurrent ranked-fuzzer run (seed 17, 700 documents, two
readers and two writers) passed 387 comparisons while performing 2,199 writer
operations, 371 vacuums and 347 cursor fetches. Eight observed plans selected
early filtering. This is bounded correctness coverage, not concurrent throughput
evidence or exhaustive coverage of MVCC behavior.

A local archive preserves both raw campaigns, build provenance, the fuzz result
and temporary orchestration: `benchmarks/results/early-filter-evidence.tar.gz`
in the prototype worktree. It is ignored and is not distributed by this PR.
Archive size: 2,023,203 bytes; SHA256:
`96d0fd2b6599db08a316086d413509428cb096b8f9abcb60b9f1ca8046c26097`. The checked-in summaries and representative plans remain available
without that local archive.
