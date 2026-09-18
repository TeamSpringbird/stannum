# An open-source PostgreSQL search engine

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../../README.md) and [BENCHMARKS](../benchmarks/README.md).

Status: proposed implementation plan, 2026-09-17. This supersedes “near TIN” as the objective in the initial investigation. No competitive performance claim has been established.

## Product target

Build an independently usable, openly developed PostgreSQL search extension whose public implementation includes the fast execution path. The user-selected first competitive target is mixed Boolean/phrase search, BM25 top-k, and exact counts with ongoing writes.

“Better than alternatives” must describe a measured workload and resource envelope. Compare with built-in GIN for filtering/counts and with ParadeDB `pg_search` and `pg_textsearch` for compatible ranked queries. Both extension projects already provide open-source search; openness alone is not the differentiator. See their primary descriptions: [ParadeDB](https://www.paradedb.com/learn/search-in-postgresql/bm25), [pg_textsearch](https://github.com/timescale/pg_textsearch).

Pin competitor versions when runs begin, use their supported tuning, and publish setup and query translations. Do not treat unsupported queries as infinite latency or compare different matching/scoring semantics as equivalent. GIN ranking is not a BM25 oracle. Verify relevance separately from retrieval speed; returning fewer or different results is not a performance win.

Proposed competitive gate: at a fixed reader/writer workload and memory budget, seek at least 2x throughput over the fastest compatible alternative while meeting the same p99 latency ceiling and achieved write rate. This is an engineering target, not an expectation backed by measurements. Publish individual scenario results, timeouts, memory, index size, build time, WAL, and write throughput; qualify every claim to the scenarios it passes. A useful first release need not win every workload.

## Architecture to pursue

Retain the parser, tokenizer, positional solver, scoring policy, and SQL compatibility surface. Build retrieval behind a small interface with two distinct responsibilities:

* A search module compiles supported query expressions and produces an ordered stream of candidate tuple locations from a pinned index view. Its interface explicitly distinguishes an empty result from an unsupported operation that requires fallback. It hides posting encodings and set-operation execution.
* A PostgreSQL adapter owns buffer access, locks, WAL, tuple lifecycle, snapshots, and conversion to executor results. A candidate is not a visible match. Initially PostgreSQL heap rechecks remain responsible for exact matching and visibility.

Do not add a generic storage framework or a forest of traits before an actual alternative needs the seam. Pure posting/query code can accept bounded input blocks and be tested without a server; durable lifecycle behavior must be tested through PostgreSQL.

Use native tuple locations for postings, ordered by heap block and offset. Begin with a scalar implementation and explicit format versions. Dense bitmap encodings and sparse sorted representations should be selected from measured density/size costs. Do not commit to one bitmap per rare term or unbounded materialization of every query result.

Keep term matching and heap rechecking under the same tokenizer contract. The current predicate uses the default pipeline while scoring reads index options; establish actual behavior for nondefault options before an index starts pruning candidates. Incorrect tokenization agreement can silently remove valid matches.

## Deliverable sequence and exit conditions

| Stage | Deliverable | Exit condition |
| --- | --- | --- |
| 0: trustworthy measurements | Public correctness runner and repeatable Lead baseline, followed by competitor adapters | Identical fixture/query inputs, saved raw results, explainable plans; optimized builds for performance |
| 1: selective retrieval | Durable term dictionary and term-to-tuple postings for terms, AND, and OR; conservative fallback for unsupported expressions | Rare/missing terms avoid full-heap work; matches equal reference; insert/abort/update/VACUUM/restart tests pass |
| 2: useful ranking | Stored document lengths and term frequencies, aggregate statistics, bounded top-k result heap | No corpus reconstruction per query; scores match agreed policy; ranking under SQL filters and visibility is correct |
| 3: positional execution | Stored positions and phrase/span candidate evaluation | Correct phrase/span results and materially fewer heap rechecks on representative traces |
| 4: competitive execution | Adaptive compression, block skipping, visibility-aware counts, safe ranked pruning | Beats alternatives in declared scenarios at equal resource/write constraints |
| 5: sustained operation | Bounded write buffering, background maintenance if needed, segment merging and operational packaging | Long-running mutation/recovery workloads remain correct with bounded memory and maintenance backlog |

Durability is part of stage 1. Stage 5 improves its efficiency; it does not introduce correctness after the fact. Stage 2 can initially score all matching candidates while retaining only k results; block-max/WAND pruning is a subsequent optimization, not a prerequisite to removing Lead's whole-corpus reconstruction.

The first coding slice should be an independently testable posting-set/query evaluator plus its PostgreSQL persistence design. Then wire one term end to end through build, insert, scan, and vacuum. An in-memory posting benchmark alone is not completion of stage 1. Avoid shipping “fast until a write happens” or “requires a rebuild after restart” as a functioning index.

## Decisions required before durable code

1. Specify the minimal on-disk dictionary/posting layout, page allocation/reclamation, lock order, and atomic publication. Compare a simple page-backed mutable design with immutable segments plus a durable write buffer. Choose based on implementation/recovery cost and measured write behavior, not resemblance to TIN.
2. Specify how scans pin a consistent structural view while writers and VACUUM operate. Tuple visibility remains separate from index structural consistency. HOT chains and reused tuple locations must follow PostgreSQL's index contract.
3. Define candidate algebra: approximations may add false positives, never false negatives. An unsupported OR branch forces a conservative union; approximate NOT cannot be implemented by naive complement. Unsupported matching remains available through the reference evaluator.
4. Define statistics semantics under snapshots and concurrent writes. Exact snapshot-wide BM25 statistics have a different cost from safe tuple visibility; do not conflate them. Preserve established behavior or document an intentional change and test ranking quality.
5. Preserve a way to run unmodified Lead as the correctness reference in a separate instance. Freeze golden fixtures as well, so simultaneous evaluator changes cannot make both sides agree on a new bug.

## Open development

Keep the fast path, correctness fixtures, benchmark adapters, workload generation, and build instructions in the repository. Existing source declares AGPL-3.0-only; retain current notices and licensing metadata during implementation. A new product name and packaging identity can be selected before public distribution, without blocking retrieval experiments. There is no need to depend on access to private TIN code for the project to succeed.

The first success is a reliable selective index. The next is ranked queries without query-time corpus reconstruction. Only then will vectorization and advanced skipping have an appropriate engine to accelerate.

## Initial validation

The installed Rust 1.96.0, cargo-pgrx 0.19.1, and Homebrew PostgreSQL 18 toolchain successfully ran `cargo test --locked -p tinql -p tokenizer -p boldi-vigna` and `cargo pgrx test pg18 --package tin --no-default-features --features pg18`. The PostgreSQL extension suite reported 32 passing tests. This establishes a working correctness baseline; it does not establish query performance or complete storage-engine lifecycle coverage. No retrieval implementation has changed yet.

## First retrieval implementation

The in-memory inverted-segment core is now implemented in
`tinql/src/runtime/retrieval.rs`, with six passing targeted tests. See
[its contract and remaining PostgreSQL work](selective-retrieval.md). This begins
stage 1; the SQL access method and its persistence callbacks are still unchanged.
The active Wikipedia baseline continues using its pinned pre-change image.

## Durable single-term slice

New logged indexes now write WAL-protected fingerprint postings and prune single-term
bitmap scans. VACUUM removes dead postings; other queries, legacy zero-page indexes,
unlogged/temporary indexes, and recovery-mode reads retain the reference fallback.
See [format, validation and remaining limits](durable-postings.md). Stage 1 remains
in progress: persisted Boolean/phrase candidates are implemented; efficient page reuse and ranking remain pending.

## Segmented storage

The fingerprint format above was replaced by segmented storage (LDP2): a write
buffer of per-document records folded into immutable segments with a term
dictionary, positions and lengths, exact bitmaps for every query form, and
page reclamation. See [segmented-storage.md](../architecture/segmented-storage.md). Stages 1
and 3 of the table are now implemented; stage 2 (persisted ranking statistics)
is next.
