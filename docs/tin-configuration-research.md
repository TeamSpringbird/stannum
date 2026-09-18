# Tin configuration: evidence for the engine and benchmark design

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../README.md) and [BENCHMARKS](../BENCHMARKS.md).

Researched 2026-09-17 against PlanetScale's live documentation and Lead source at
`3fcf441ac7c3d183de179b1f846ceb0ef83e1358`. Documentation is not a pinned Tin release;
save the actual extension version and effective settings with any future Tin run.
This is documentation/source research, not a measurement of Tin.

The useful distinction is between knobs that change the search contract and knobs
that execute the same contract differently. Our benchmark must preserve the first
and account for the resources and maintenance costs of the second.

## Search semantics already present in Lead

These defaults and ranges are verified in local
[options.rs](../postgres/src/options.rs),
[bm25.rs](../postgres/src/bm25.rs),
[udfs.rs](../postgres/src/udfs.rs), and
[tokenizer/spec.rs](../tokenizer/src/spec.rs).

| Index option | Default | Accepted values | Contract affected |
| --- | --- | --- | --- |
| `tokenizer` | `unicode` | `unicode`, `whitespace` | Token boundaries |
| `case_folding` | `fold` | `fold`, `preserve` | Case matching |
| `accent_folding` | `fold` | `fold`, `preserve` | Accent matching |
| `long_tokens` | `split` | `split`, `truncate`, `discard` | Long-token handling |
| `max_token_bytes` | `256` | `4..2692` | UTF-8 token size |
| `graphemes` | `emoji` | `emoji`, `retain`, `discard` | Standalone graphemes |
| `position_gaps` | `preserve` | `preserve`, `collapse` | Phrase positions |
| `k1` | `1.2` | `0..10000` | Frequency saturation |
| `b` | `0.75` | `0..1` | Length normalization |
| `score_stop_words` | unset | Comma-separated analyzed terms | Scoring exclusions |

Tin documents the same semantic settings. Scoring changes apply without rebuilding;
tokenizer changes require `REINDEX` for existing documents.
[Tin index reference](https://planetscale.com/docs/postgres/search/reference/indexes).

Lead's [score.rs](../postgres/src/score.rs) implements query overrides for `k1`, `b`,
`dense_ratio`, `term_add`, and `term_replace`; its default `dense_ratio` is `0.10`.
These are not interchangeable performance switches. Tin omits terms reaching 10%
document frequency from default scoring. Explicit boosts pin terms; added/replacement
scoring terms are analyzed. `tin.full_score` bypasses both density exclusion and
scoring stop words. Matching is unaffected by density exclusion.
[Tin scoring](https://planetscale.com/docs/postgres/search/scoring).

**Benchmark decision:** use full scoring for an explicit full-BM25 comparison, and
keep default/elided scoring in a separately named scenario. Record analyzer options,
scoring parameters, term edits, and boosts. Disabling stemming alone does not prove
two engines agree on punctuation, accents, phrases, or document lengths. Validate
representative token streams and result sets first.

## Storage and maintenance controls missing from Lead

Documented Tin settings:

| Index option | Default | Domain | Purpose |
| --- | --- | --- | --- |
| `initial_segment_count` | Available parallelism | `1..4096` | Build partitioning |
| `target_segment_count` | Initial count | `1..4096` | Maintenance target |
| `max_mutable_segment_size` | `4194304` bytes | `>=131072` | Write accumulation |
| `max_merged_segment_size` | `2000` MB | `>=100` | Merge size ceiling |
| `dead_percent_threshold` | `0.5` | `0..1` | Dead-entry rewrite trigger |

Mutable data also folds at 16,384 documents. Initial count is build-time;
other changes affect subsequent maintenance. Segment count bounds parallelism.
[Tin index reference](https://planetscale.com/docs/postgres/search/reference/indexes).

Lead accepts `initial_segment_count` but explicitly ignores it, with default `1`
and domain `1..1024`. It registers none of the other four storage options.
The entire registration is in [options.rs](../postgres/src/options.rs).
Do not describe unsupported settings as merely ignored, or reuse Tin DDL blindly.

**Inference:** a useful engine needs separately measurable write buffering,
partitioned search, consolidation, and dead-entry reclamation. These controls show
the tradeoffs exposed by Tin; they do not reveal its complete disk format or imply
we should copy its thresholds. Start with correctness and instrumentation before
making each threshold public API.

## Execution controls expose alternative algorithms

All rows below come from the
[Tin settings reference](https://planetscale.com/docs/postgres/search/reference/settings).
Numerical domains not published there remain unknown.

| GUC (`tin.` prefix) | Default | Values / effect |
| --- | --- | --- |
| `enable_custom_scan` | `on` | Off: generic scans |
| `index_maintenance_mode` | `background` | `background`, `foreground`, `manual` |
| `build_io_concurrency` | `0` | Auto; `1` serializes |
| `rss_baseline` | Server-derived MB | Worker memory estimate |
| `maintenance_jobs_per_db` | `0` | Database scheduling; zero drains |
| `track_page_reuse_stats` | `off` | Page reuse in EXPLAIN |
| `debug_force_boolean_family` | `auto` | `auto`, `fused`, `factored` |
| `debug_force_topk` | `auto` | `auto`, `topk`, `exhaustive` |
| `debug_force_multi_index_drive` | `auto` | `auto`, `sparse`, `stripe` |
| `debug_force_conjunction_mode` | `auto` | `auto`, `generic`, `pushdown`, `noadaptive` |
| `debug_force_visibility` | `auto` | `auto`, `streaming`, `sorted` |
| `debug_disable_count_pushdown` | `off` | `on`, `off` |
| `debug_disable_index_probe` | `off` | `on`, `off` |
| `debug_force_parallel` | `off` | `on`, `off` |

Debug controls preserve results and force planner alternatives. Maintenance job
scheduling rejects session `SET`; use server configuration. Lead registers no such
GUCs in [_PG_init](../postgres/src/lib.rs).

**Inference:** these switches suggest valuable future experiments: compare bounded
ranking against exhaustive scoring; compare rare-term candidate generation against
block-oriented evaluation; measure visibility batching; test filters inside search
against filters after retrieval. A diagnostic forced path should remain a separate
benchmark from the normal cost-based planner. A fast forced path may expose a
planner problem without proving the product is faster in ordinary use.

## Operational behavior to preserve in measurements

Tin uses `maintenance_work_mem` for builds/maintenance; build workers prefer about
1 GB each. Smaller build budgets can leave persistent fragmentation. Worker-pool
exhaustion can move maintenance into writer sessions. `effective_io_concurrency`
controls read-ahead. Query worker caps and planner costs also affect execution.
VACUUM reports dead entries; count shortcuts additionally depend on visibility-map
state. Replica readers need `hot_standby_feedback`.
[Operational guidance](https://planetscale.com/docs/postgres/search/operations).

Tin's scoring statistics include stored dead documents until segment reclamation
or rebuild, even though returned rows obey visibility. Lead's
[build_corpus](../postgres/src/score.rs) instead reads documents through SQL and
reconstructs statistics from that visible corpus. Cross-engine scores during churn
therefore need not agree even when both correctly implement their own contracts.
[Tin scoring visibility](https://planetscale.com/docs/postgres/search/scoring#visibility).

**Benchmark decisions:**

1. Save effective PostgreSQL settings, including build settings, with every run:
   `maintenance_work_mem`, worker limits, `work_mem`, `shared_buffers`, I/O and
   planner costs, WAL settings, and table autovacuum settings. Give each engine the
   same total CPU, memory, and storage budget rather than assuming identical
   per-process knobs imply equal resource use.
2. Separate freshly built/vacuumed runs from sustained write runs. Include inserts,
   updates, and deletes. Report committed write rate, read and write tail latency,
   WAL, index growth, vacuum activity, and maintenance backlog/recovery time where
   observable. Do not disable maintenance and call the result sustained throughput.
3. Check visibility correctness independently of numerical ranking equivalence.
   Use frozen, identically prepared corpora to validate scoring, and document the
   statistics contract for concurrent updates. Do not silently redefine a ranking
   contract to make a comparison pass.
4. Track both one-query parallel latency and throughput with concurrent clients.
   Save execution plans to see whether extra workers or different paths caused a
   result. Pin storage class and distinguish warmed from disk-read workloads.

## Inspection and integrity tools

`tin.tokenize` exposes token analysis. `tin.score_inspect` exposes retained scoring
terms and weights. `tin.fsck(index, heapcheck => true)` adds heap/TID checks to a
read-only structural check; it requires index ownership and does not repair data.
[Tin functions reference](https://planetscale.com/docs/postgres/search/reference/functions).
Lead implements tokenization and score inspection, but no `fsck` in the inspected
[Postgres module tree](../postgres/src/lib.rs).

**Recommendation:** record sampled plans separately from timed traffic and make
engine health observable before optimizing compaction. Our future equivalent
should expose segment counts/bytes, buffered writes, dead entries, queued work,
and completed maintenance. These are proposed Lead metrics, not claims that Tin
publishes SQL functions for all of them. The inspected reference does not document
a segment-statistics API or a command to trigger manual maintenance.

Tin also documents direct ranked, count, and btree-filtered query shapes.
[SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes).
For fairness, record each adapter's actual SQL and plan; a mathematically equivalent
rewrite may prevent one engine's optimized path. Add richer filters after the first
mixed workload is repeatable, without erasing its original baseline.
