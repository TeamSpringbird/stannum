# Stannum documentation

Stannum is an independent PostgreSQL search extension derived from PlanetScale
Lead. See the [project README](../README.md) for installation, SQL examples, and
compatibility limits. Documentation lives here; benchmark programs remain in
`benchmarks/` and extension code remains in its workspace crates.

## Current guides

- [Benchmark results and comparison plan](benchmarks/README.md): the 100k workload,
  measured results, caveats, and the next same-instance comparison with TIN.
- [Benchmark harness reference](benchmarks/harness.md): adapters, commands, and output.
- [Local campaign setup](benchmarks/local.md): corpus preparation and trial execution.
- [Validated benchmark environments](benchmarks/validated-environments.md): historical
  smoke-test versions and the limits of that evidence.
- [Storage and execution architecture](architecture/segmented-storage.md): the current
  segmented index, query execution, maintenance, and remaining limitations.
- [TINQL query language](query-language/src/SUMMARY.md): syntax and examples. Build the
  rendered guide from the repository root with `mdbook build docs/query-language`.

## Archived research and milestones

These notes preserve the reasoning and evidence behind earlier implementations.
Their Lead/TIN names and observations describe the version studied at the time;
use the current guides above for present behavior.

- [Original Lead performance investigation](archive/lead-performance-investigation.md)
- [TIN research and benchmark sources](archive/tin-research.md)
- [TIN configuration research](archive/tin-configuration-research.md)
- [Observed TIN storage and scoring](archive/tin-observed-shape.md)
- [Search engine plan](archive/search-engine-plan.md)
- [Index design proposal](archive/index-design-ideas.md)
- [Selective retrieval](archive/selective-retrieval.md)
- [First durable postings format](archive/durable-postings.md)
- [Storage quality plan](archive/storage-quality-plan.md)
- [First performance milestone](archive/performance-milestone-01.md)

## Keeping evidence and credentials separate

Run documented commands from the repository root. Keep new explanatory documents
in this tree and link them from this index. Keep generated corpora, raw results,
source snapshots, and graphics outside Git; `benchmarks/results/` and `artifacts/`
are ignored local output locations. Published summaries should identify the build,
protocol, and whether their supporting artifacts are publicly available.

Keep PostgreSQL connection strings, passwords, and libpq environment files outside
Git. Local `.env`, `*.env`, `.pgpass`, and `.pg_service.conf` files are ignored;
only sanitized example files may be committed. Review the staged diff and outgoing
history before publishing, since ignore rules do not protect already tracked files.
