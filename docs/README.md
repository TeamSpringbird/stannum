# Stannum documentation

Start with the [project README](../README.md) to build Stannum and run a
search. Run commands from the repository root.

## Using Stannum

- [Query language](query-language/README.md): the TINQL reference, starting
  with the [introduction](query-language/introduction.md), then
  [terms and expansions](query-language/terms.md),
  [Boolean operators](query-language/boolean-operators.md),
  [phrases](query-language/phrases.md),
  [alternatives](query-language/alternatives.md),
  [proximity](query-language/proximity.md),
  [span relations](query-language/span-relations.md),
  [positional filters](query-language/positional-filters.md),
  [boosts](query-language/boost.md),
  [precedence](query-language/precedence.md),
  [keywords and escaping](query-language/keywords.md) and
  [recipes](query-language/recipes.md).
- [SQL permissions and security review](SECURITY.md): what each function
  requires and how settings are bounded.

## Operating

- [How Stannum works](architecture/segmented-storage.md): storage settings,
  maintenance and autovacuum, the per-backend caches, `verify_index` and its
  operator guide, and current limits.
- [Recovery, persistence and parallel execution](architecture/recovery-and-parallel.md):
  temporary and unlogged indexes, hot standbys and parallel scans.
- [Releasing and upgrades](RELEASING.md): versioning, the release checklist
  and on-disk compatibility.

## Internals and decisions

- [How Stannum works](architecture/segmented-storage.md): the segment format,
  query execution, ranking and durability rules.
- [Architecture decision records](adr/README.md):
  [0001](adr/0001-preserve-posting-order-before-changing-encoding.md) merge by
  sorted order,
  [0002](adr/0002-batched-posting-execution-before-format-migration.md) batched
  page bitmaps (superseded),
  [0003](adr/0003-address-postings-by-document-ordinal.md) documents as
  ordinals, and [0004](adr/0004-rank-by-document-ordinal.md) ranking over
  ordinals.

## Compatibility and conformance

- [Compatibility with TIN](compatibility.md): what matches TIN 1.0.3, the
  documented improvements and gaps, index options and the Lead reference
  oracle.
- [TIN conformance suite](../conformance/README.md): the engine-agnostic
  cases, TIN's recorded answers and the runner.
- [TIN behavior catalog](tin-behavior-catalog.md): TIN's documented and
  measured behavior, with sources, from which the conformance cases were
  written.

## Testing and benchmarks

- [Testing](testing.md): every kind of test and how to run it.
- [Benchmarks](benchmarks.md): workloads, correctness checks, current results
  and how to reproduce them.
- [Local benchmark runs](../benchmarks/local/README.md): rehearsing the
  published workloads on a laptop when the index does not fit in memory.

## Project

- [Changelog](../CHANGELOG.md)
- [Source attribution](ATTRIBUTION.md): copyright notices, the provenance
  manifest and the header check.
- [Upstream synchronization](UPSTREAM.md): how Lead changes are reviewed and
  taken.
- [Boldi–Vigna crate](../boldi-vigna/README.md): the positional operator
  evaluator inherited from Lead.
