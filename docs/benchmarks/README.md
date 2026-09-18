# Benchmarking Stannum

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
   maintenance; report tail latency and correctness alongside throughput.
4. Compare with TIN when both engines can be measured in an equivalent environment
   under the same protocol.

The evaluation stays at 100k documents for now. Better-controlled measurements
are more useful than increasing the dataset size at this stage.

See the [harness reference](harness.md) and [local campaign guide](local.md) for
commands, configuration, and result collection.
