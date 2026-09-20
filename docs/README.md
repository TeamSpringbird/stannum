# Stannum documentation

Start with the [project README](../README.md) to build Stannum and run a search.

- [Query language](query-language/introduction.md): syntax and examples for
  terms, phrases, Boolean queries, proximity, and ranking boosts.
- [Architecture](architecture/segmented-storage.md): how indexing, searching,
  scoring, and maintenance work; source locations and known limits.
- [Testing under concurrency](testing.md): the ranked-scan fuzzer, its oracle,
  and the bug classes it hunts.
- [Round-four integration validation](benchmarks/round4-integration.md): recovered
  agent work, combined-build correctness checks, and paired measurements.
- [Benchmark results](benchmarks/README.md): local Lead-to-Stannum results and
  the plan for controlled comparisons.
- [Run benchmarks](benchmarks/local.md): prepare the corpus and run a campaign.
- [Benchmark tools](benchmarks/harness.md): individual runners, correctness checks,
  and result files.

- [Source attribution](ATTRIBUTION.md): copyright notices, provenance, and header checks.
- [Upstream synchronization](UPSTREAM.md): disposition of Lead changes and validation.

Run commands from the repository root. Keep raw results in ignored
`benchmarks/results/` and connection credentials outside Git.

- [TIN across machine sizes](benchmarks/tin-machine-comparison.md)
- [Expanded TIN strategy and capacity experiments](benchmarks/tin-expanded-experiments.md)
- [Early filtering and core bitmap comparison](benchmarks/early-filter-ranking.md):
  two controlled experiments, negative results, and the next ranked-search target.
- [Filtered top-k retry experiments](benchmarks/filtered-prefix-ranking.md):
  targeted wins, adversarial regressions, and limits of repeated ranking.
