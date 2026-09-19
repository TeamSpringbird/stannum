# Benchmarking Stannum

The [fold and merge report](fold-and-merge.md) records independent codec gains,
correctness validation, and the unlocked-fold performance regression.

The [merge-cost probe](merge-costs.md) compares foreground maintenance budgets
and explains the direct forward-record ingestion optimization.

The [insert-preparation report](insert-preparation.md) validates moving text
preparation outside the exclusive metadata lock and records paired contention runs.

The [concurrent contention probe](concurrent-write-contention.md) measures
reader/writer tails, sampled waits, and correctness during foreground maintenance.

The [foreground write probe](foreground-writes.md) attributes insert latency to
buffer appends, folds, and merges, establishing the next optimization baseline.

The [streaming search report](streaming-search.md) covers unordered search,
LIMIT consumption, cursor lifetime checks, and paired full-traversal benchmarks.

The [page-bitmap count report](page-bitmap-counts.md) covers adaptive dense-count
execution, correctness checks, and paired count-only measurements.

The [round-four integration report](round4-integration.md) records the recovered
agent branches, combined-build validation, and a paired comparison with the
preceding published Stannum build. The Lead comparison below is historical.

Our first benchmark asks how much faster Stannum is than the original Lead
implementation it grew from, while preserving search results. The useful
comparison so far is **Lead locally versus Stannum locally**, using the same
100,000 Wikipedia articles, workload, and resource limits on the same machine.

## Local Lead-to-Stannum results

| Build | Count queries/second | Mixed count/ranked queries/second |
| --- | ---: | ---: |
| Original Lead | 0.39 | No successful runs; all five exceeded the memory limit |
| Stannum development build | 1,458.45 | 644.79 |

Both workloads ran alongside document updates. Count queries cover terms,
Boolean combinations, phrases, and misses; the mixed workload adds ranked top-ten
queries. Both engines had a four-CPU, 4-GiB server budget. Stannum sustained
approximately 20 updates/second in both trials and passed the before/after
membership checks and the mixed workload's ranking checks.

These results show substantial progress over Lead on this workload. They are
preliminary: Lead's count value is the median of five trials, while Stannum has
one trial per workload. Runs were performed sequentially, not as alternating
pairs. Lead has no successful mixed result from which to calculate a speedup.
The Stannum measurements also predate the rename and recent upstream fixes;
they are not measurements of the current commit.

Build identities and reproduction details are in the
[local campaign guide](local.md#historical-100k-comparison-provenance).
Raw artifacts are retained locally and are not yet published.

## Correctness evidence

The local benchmark checks exact match membership before and after updates, plus
ranked result membership, cardinality, finite scores, and score order. Those checks
do not establish that every query returns the globally correct top ten.

Separately, a development build matched TIN's document sets and score bits across
205 query/state pairs: 41 queries across five mutation states. This is sampled
compatibility evidence, not proof of complete equivalence or a speed comparison.

### The behavioral regression suite

Two differential oracles share one fixture and one query list
(`benchmarks/oracle.py`): 47 TINQL shapes covering terms, Boolean forms, phrases,
gaps, slop, proximity, relations, positional filters, wildcards, regular
expressions, ranges, fuzzy matching, AT LEAST, boosts, the match-all form,
and tokenizer-sensitive accents, numerics, apostrophes, hyphens, URL hosts and emoji,
observed after build, after deletes, after VACUUM, after inserts into the write
buffer and after REINDEX.

* **Lead reference, in CI on every push** (`script/reference-oracle`, job
  "Reference oracle against upstream Lead"). PlanetScale's Lead is checked out
  directly from its repository (`main` unless `LEAD_REF` pins a revision) and
  built under its own extension name `tin` into the same server as Stannum, so
  the comparison tracks Lead as it is maintained. Match sets and the rank
  order of full and dense scores, and exact HTML/ANSI highlighted strings must
  agree (`--scores order`). Score bits are
  not compared: Lead counts a token-less document present at index build in its
  corpus size, which shifts every IDF in the last bits; TIN and Stannum do not.
  Expansion shapes (wildcards, regular expressions, ranges) compare match sets
  and highlights, because Lead scores their matches as zero where TIN scores the
  expanded terms. Highlight defects require an explicit reason in
  `REFERENCE_UNHIGHLIGHTED`; score exclusions never suppress highlight checks.
  The [compatibility audit](../compatibility.md) records the inspected function
  signatures and local results.
* **TIN, locally by hand** (`benchmarks/oracle.py --right-engine tin` against
  a PlanetScale database, with the connection in a libpq env file that never
  enters the repository). Bit-for-bit scores, including `max_score`, and exact
  HTML/ANSI highlights. This is
  the standard the Lead oracle cannot provide; it is not part of CI because it
  needs a live TIN instance and credentials.

Add to the query list whenever a behavior is fixed or a gap is found; a new
shape costs one line and is then checked against both references. Regressions
found by hand belong in `postgres/src/lib.rs` as pg_tests or in
`postgres/tests/postings_lifecycle.py` when they need a real server.

## Building comparable benchmarks

We are working toward repeatable comparisons where hardware, PostgreSQL settings,
corpus, query shapes, client load, and write schedules are held constant. Local
Stannum and remote PlanetScale TIN timings do not meet that standard, so we do not
use them to rank the engines or report a speedup over TIN.

The next steps are:

1. Repeat the local Lead-to-Stannum comparison with pinned builds and five trials
   per workload, alternating execution order and retaining failed runs.
2. Establish a baseline for the current Stannum build before optimizing it.
3. Extend workloads to inserts, deletes, changing match sets, and sustained
   maintenance; report tail latency and correctness alongside throughput. The
   harness now has a [mutation profile](harness.md#sustained-mutation-profile)
   for this; its results are not yet part of the campaign summary.
4. Compare with TIN when both engines can be measured in an equivalent environment
   under the same protocol.

The evaluation stays at 100k documents for now. Better-controlled measurements
are more useful than increasing the dataset size at this stage.

See the [harness reference](harness.md) and [local campaign guide](local.md) for
commands, configuration, and result collection.
