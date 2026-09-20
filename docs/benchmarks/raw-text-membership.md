# Raw-text membership validation

The timed harness now supports the raw prepared Stack Exchange corpus. It keeps
bodies and queries unchanged, defaults to the confirmed Stack Exchange trace,
and compares each index against its own evaluator on an unindexed sample.
The independent pinned Lead campaign remains the semantic compatibility gate.
Wikipedia's normalized lexical reference remains unchanged.

## Positive witnesses

`benchmarks/oracle.py --raw-text` runs eight concrete query expectations on both
indexed and sequential paths in both Stannum and Lead: mixed case, adjacency,
AND without adjacency, newline/punctuation phrase matches, reverse order,
repeated words, a Unicode phrase and a negative phrase. EXPLAIN plans must prove
the requested search-index or sequential path. Empty answers cannot satisfy
positive expectations. These checks do not compare scores.

The first fixture had a primary key; PostgreSQL chose it for ORDER BY instead of
the search index. The plan assertion rejected that run. Removing this unrelated
index made the intended access path testable; no extension behavior changed.

## Native adapter smoke

On 100 unchanged published Stack Exchange documents, all 3,762 forms passed
exact membership comparisons for Stannum and GIN against their respective heap
references. However, 616 forms had differing sampled membership between the two
engines. These require explicit exclusion/disclosure in same-result performance
comparisons. Different analyzers are a plausible explanation; this smoke does
not diagnose each disagreement or establish that GIN implements Lead semantics. GIN ranking is also not a BM25 oracle.

Raw local evidence is retained under `benchmarks/results/raw-membership-live/`.
This native SQL smoke alone does not prove container-driver or throughput
behavior. See the accompanying result receipt for completed validations.

## Completed container smoke

A native ARM64 PostgreSQL 18 container campaign completed for Stannum and GIN,
using 100 raw documents, all 3,762 forms, two clients, three seconds warmup and
15 seconds measured traffic per engine. All 3,762 membership checks passed for
each engine, and each engine traversed all forms during traffic. The report
records 616 differing sampled membership sets and 581 differing full counts;
equal counts do not imply equal membership. No speedup ratio is inferred.

The initial container attempt stopped before traffic because the generated SQL
exceeded the OS argument limit. Large batches now use psql stdin with a single
transaction, preserving the previous `-c` transaction behavior. A regression
test covers that transport; short calls including VACUUM keep their original path.
All 155 harness tests pass. Containers and their owned volumes were removed.

[Result receipt](raw-text-membership-results.json) records source/image/input
identities and the local evidence archive hash. The archive retains both failed
harness attempts and successful runs. This is a correctness/integration smoke,
not a capacity result or a measurement of closed-source TIN. Ranked, update and
large-corpus campaigns remain to be exercised on this raw workload.
