# From Lead compatibility to a real search engine

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../../README.md) and [BENCHMARKS](../benchmarks/README.md).

Investigation date: 2026-09-17. Source baseline: `3fcf441ac7c3d183de179b1f846ceb0ef83e1358`.

## Conclusion and evidence boundary

Substantial speedups are plausible because Lead does almost none of the work avoidance expected of an inverted index. It is a SQL compatibility implementation, not a slower implementation of TIN's storage engine. Approaching TIN would mean building a new persistent search engine behind the existing interface. The public article supplies architectural direction, not a complete implementation specification or evidence that a particular speedup is attainable.

This is a source investigation and an experiment plan, **not a measured performance diagnosis**. No query timings, profiles, or speedups were collected. The initial investigation lacked a Rust/PostgreSQL environment. The user subsequently installed the pinned toolchain and Homebrew PostgreSQL 18; the pure Rust suites and 32 PostgreSQL extension tests passed. See the [current engine plan](search-engine-plan.md), which supersedes proximity to TIN as the objective. Unrelated running databases should not become the benchmark environment.

See [TIN research and benchmark sources](tin-research.md) for the external evidence and available reproduction assets.

## What the source actually does

| Path | Observed behavior | Consequence inferred from the code |
| --- | --- | --- |
| [`am.rs`, `build_callback`, `aminsert`](../../postgres/src/am.rs) | Build counts tuples; insert persists no postings. | Index creation does not precompute document search data. Lead's index build time/size is not comparable to a functional search index. |
| [`am.rs`, `amgetbitmap`](../../postgres/src/am.rs) | Reads heap block count and calls `tbm_add_page` for every block. Scan keys only determine whether the scan is active. | Even a nonexistent term offers no index pruning. A bitmap path nominates the entire heap for recheck. |
| [`operator.rs`, `evaluate_text`](../../postgres/src/operator.rs) | Parses, subtokenizes, lowers, tokenizes the document, then evaluates on each invocation. | Query preparation repeats across rows; document analysis repeats across searches. |
| [`runtime/eval.rs`, `TokenizedDoc`, `evaluate`](../../tinql/src/runtime/eval.rs) | Allocates token strings, position maps, and match intervals. | Boolean predicates pay for representations also useful to positional queries. Whether these allocations dominate needs profiling. |
| [`score.rs`, `build_corpus`, `load_documents`](../../postgres/src/score.rs) | On a cache miss, SPI reads all non-null indexed column/expression values, tokenizes all documents, derives document frequencies, and scores every document. | Ranked queries incur corpus-wide work even if only a few documents match and only ten results are requested. |
| [`score.rs`, `SCORE_CACHE`, `score_bound`](../../postgres/src/score.rs) | One cached corpus per backend thread; key includes query, relation/index, transaction/command, and scoring options. Scores are keyed by full document text. | It does **not** rebuild for every score call with the same key. Alternating keys can evict each other; text ownership and hashing add memory/CPU costs. Cache hit behavior must be measured explicitly. |
| [`am.rs`, `amcostestimate`](../../postgres/src/am.rs) | Fixed 0.1 selectivity; estimated pages derived from tuples/512. | Estimates do not model term frequency or the actual all-page scan. Planner selection can obscure engine comparisons. |

Let `N` be visible documents, `T` their total token count, and `Q` scoring terms. A full bitmap recheck performs document processing across the heap, plus repeated query preparation. A scoring cache miss adds roughly `O(T + Q*T)` token traversal and holds owned documents and tokens in memory during construction. These are structural estimates, not fitted timing models; expression evaluation, allocation, I/O, token lengths, query expansion, and cache behavior also matter. `LIMIT 10` does not bound this corpus construction.

Existing tests explicitly check lossy heap scans, heap growth/TRUNCATE, expression and partial-index rechecks, unions, and update/delete visibility in [`postgres/src/lib.rs`](../../postgres/src/lib.rs). They passed after the development environment was installed.

## What can be reused

Keep TINQL parsing/lowering, tokenizer pipelines, the positional solver, BM25 arithmetic/policy, and the SQL surface. Preserve the current evaluator as an independent reference path. The missing pieces are durable term dictionaries/postings/positions, incremental statistics, selective query execution, ranking integration, and the storage lifecycle.

Do not assume the article specifies top-k pruning, positional compression, storage layout, or recovery protocols. Those need their own designs and measurements. A proposed WAND/block-max ranking engine, for example, would be our design choice, not a demonstrated reconstruction of TIN.

## Establish the measurement loop first

Use a dedicated PostgreSQL 18 instance and the repository's pinned Rust 1.96.0 / pgrx 0.19.1 toolchain. Start with the existing public tests:

```sh
cargo test --locked -p tinql -p tokenizer -p boldi-vigna
cargo pgrx test pg18 --package tin --no-default-features --features pg18
cargo pgrx package --package tin --no-default-features --features pg18 --release
```

Check CLI options against the installed pinned pgrx before automating packaging. Install the resulting package in the isolated benchmark instance. Performance runs must use an optimized build; test builds alone are not a baseline. The optional private regression runner needs private TIN sources and excludes performance/recovery/lifecycle coverage according to [`private-regress.manifest`](../../private-regress.manifest).

First use a deterministic synthetic corpus at 10k, 100k, then 1m documents, increasing only after the smaller case is practical. Give every row `common filler`, every 100th row `rare`, and every 1,000th row `alpha beta`; vary document length in separate fixtures. At sizes divisible by 1,000, expected matches are:

| Query | Expected count | Purpose |
| --- | ---: | --- |
| `absenttoken` | 0 | Exposes work on guaranteed misses |
| `rare` | N/100 | Selective lookup |
| `common AND rare` | N/100 | Intersection skew |
| `common OR rare` | N | Dense union |
| `"alpha beta"` | N/1000 | Exact positions |
| `"beta alpha"` | 0 | Phrase false positives |

Use `COUNT(*)`, matching row retrieval, and `ORDER BY tin.score(ctid) DESC LIMIT 10` as distinct workloads. Also run `tin.full_score`; dense-term scoring policy can otherwise make a synthetic ranking test uninformative. Validate scores and tie groups separately from throughput. Add duplicate text, null/empty documents, Unicode, wildcard/fuzzy queries, negatives, expression indexes, and partial indexes to correctness fixtures.

Capture `EXPLAIN (ANALYZE, BUFFERS, WAL, SETTINGS, FORMAT JSON)` for diagnostic samples. Record actual rows, rows removed by index recheck, exact/lossy heap blocks, buffer hits/reads, and sort behavior. Nested SPI scoring work may require separate instrumentation; the outer plan alone is insufficient to attribute its cost. Use regular queries, not instrumented EXPLAIN, for throughput/latency runs.

Run with normal planner settings first. Repeat targeted samples with `enable_seqscan=off` to expose the bitmap path, recording that setting rather than conflating forced-plan results with normal SQL performance. Capture peak backend RSS and profile CPU/allocation costs. Count operator invocations, query preparations, scoring cache misses, and corpus builds in diagnostic builds, removing or disabling counters for final timings.

Measure first-touch and warmed-buffer runs separately; a fresh connection is not a cold disk cache. Use distinct query texts and realistic distributions so one cached score corpus does not become the entire benchmark. Retain raw samples, warm-up protocol, run duration, concurrency, timeouts/errors, commit/build flags, hardware, PostgreSQL settings, corpus/query hashes, and repeated runs. Do not report p99 from a handful of queries.

## Experiments after the baseline works

These are proposed experiments, not established bottleneck rankings from a profiler:

1. **Selective access:** if all-page rechecks dominate, a term candidate index should sharply reduce heap blocks and predicate calls for `rare` and `absenttoken`. If it only reduces parser time, it has not solved selection.
2. **Query preparation:** cache the parsed/lowered query within an appropriately scoped execution context. Predict fewer preparations with unchanged row/tokenization counts; measure how much latency remains. Test varying RHS values and tokenizer settings, not just a constant string.
3. **Scoring statistics:** replace repeated query-term scans through all token arrays with per-document term counts and aggregated statistics. Predict lower corpus-build CPU, particularly as Q grows. This intermediate improvement still pays to load/tokenize the corpus on cache misses.
4. **Predicate-only evaluation:** avoid materializing unnecessary result intervals where semantics permit. Predict allocation reduction without any change to matches, particularly on repeated common terms. Phrase/span/negative semantics require differential checks.

Change one variable at a time and retain each result even if the experiment loses. Do not start with SIMD: eliminating unnecessary document processing can change the scaling behavior; vectorizing the current full scan cannot make it selective.

## Incremental implementation path

1. **Baseline and oracle.** Deliver an isolated reproducible environment, fixture/trace runner, unmodified-Lead baseline, and differential correctness runner. Save baseline outputs before changing matching or scoring.
2. **Bounded CPU improvements.** Use measurements to choose query preparation or scoring work reduction. Cache lifetimes must respect snapshots, changing query arguments, index options, and memory contexts. Validate read-only transactions and concurrent writers; a transaction/command identity must not be assumed to uniquely identify every relevant snapshot.
3. **Persistent selective candidates.** Start with term-to-page or term-to-ctid postings and retain exact heap rechecks. Term-page candidates can be a useful milestone even before exact offsets. Unsupported syntax must conservatively fall back to the full scan. AND/OR candidate operations must have no false negatives; NOT cannot simply complement an approximate candidate set. Measure term lookup and count before designing ranked scans.
4. **Index-resident statistics and positions.** Store term frequency, document length, positional data, and aggregate statistics so queries stop reconstructing the corpus. Establish scoring-statistics semantics under MVCC and partial indexes before claiming compatibility. Use the existing floating-point and frequency-bucket policy as a reference.
5. **Compressed page/offset postings.** Benchmark scalar operations and adaptive encodings on real term-frequency distributions before adding architecture-specific paths. This machine is ARM64; the article's x86 vector results cannot be extrapolated from local measurements.
6. **Count and ranked execution.** Add visibility-aware count execution and bounded top-k traversal once posting/statistic correctness is established. Any score bound used to skip work must be conservative under actual boosts, scoring options, and extra SQL filters. An early LIMIT before visibility/filtering is incorrect.
7. **Sustained writes and storage efficiency.** Introduce mutable/immutable segment management and efficient merging only with lifecycle coverage and measurements of WAL, write amplification, merge backlog, and reader tails. Basic durable writes and recovery belong in step 3, not here.

Each persistence milestone needs tests for insert/update/delete, abort, concurrent snapshots, HOT updates, VACUUM and ctid reuse, TRUNCATE, REINDEX, expression/partial indexes, restart/crash recovery, and replication. A physical tuple location can be reused; it is not an eternal document identity. Keep heap visibility checks until the replacement visibility path is proven. Do not accept count shortcuts based solely on physical postings when dead/invisible tuples or overlapping segments can change the answer.

## What “good” means

First, identical match sets and documented scoring/tie behavior against the unmodified reference, including mutations and snapshots. Then, less work: absent/selective queries should stop scaling with total heap size; ranked queries should stop rebuilding corpus statistics; all-visible counts should eventually avoid tuple-by-tuple heap visits. Track both query latency and the memory/storage/write costs that purchase the improvement.

Compare each revision with unmodified Lead on the same fixture, hardware, build mode, and cache state. Next run the public realistic traces across count/top-k, conjunction/disjunction/phrase, fitting/exceeding memory, and read-only/concurrent-update workloads. Report achieved updates per second alongside read latency, not just the requested write rate.

Only claim proximity to TIN after a directly comparable TIN run or a carefully qualified reproduction of the published environment. Blog throughput is a workload-specific reference, not a universal target. The immediate actionable milestone is the baseline/oracle plus a selective-query experiment; “near TIN” remains an open research outcome.
