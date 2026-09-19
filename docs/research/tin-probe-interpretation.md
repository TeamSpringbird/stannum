# TIN probes and interpretation

Researched 2026-09-19 against first-party documentation. This is an experiment
proposal, not a record of executed tests. Existing observations are in the
[plan catalog](../benchmarks/tin-plan-catalog.md) and
[machine comparison](../benchmarks/tin-machine-comparison.md).

## Most useful discovery: measure the alternative plans

TIN documents session-level controls for forcing alternative strategies. Use
short-lived probe sessions and retain the automatic-plan result alongside each
forced result. Verify availability on the installed extension version before
running the matrix. A setting name accepted as an arbitrary custom GUC is not
proof that an extension implements it.

| Setting | Probe |
| --- | --- |
| `tin.debug_force_topk = topk / exhaustive` | Measure bounded versus exhaustive ranking at the same LIMIT |
| `tin.debug_force_conjunction_mode = generic / pushdown / noadaptive` | Separate mixed-predicate strategies and adaptive verification |
| `tin.debug_disable_index_probe = on` | Measure the value of the btree-assisted path |
| `tin.debug_disable_count_pushdown = on` | Compare specialized counting with aggregation over a scan |
| `tin.debug_force_visibility = streaming / sorted` | Isolate count visibility-check ordering |
| `tin.debug_force_parallel = on` | Inspect a parallel alternative, if one exists |
| `tin.track_page_reuse_stats = on` | Obtain distinct versus repeated index-page reads |

For multiple TIN indexes, `debug_force_boolean_family` accepts `fused` or
`factored`, and `debug_force_multi_index_drive` accepts `sparse` or `stripe`.
These documented debugging controls preserve results and still reject unsafe
query shapes. [TIN settings](https://planetscale.com/docs/postgres/search/reference/settings)

**Proposed experiment:** locate one automatic-plan crossover, then measure both
alternatives on either side, alternating run order. A switch is evidence of a
planner choice; a measured crossover establishes whether that choice pays off
on this fixture and machine. Record exceptions rather than silently dropping
unsupported configurations. Run correctness comparisons before timing, allowing
different tied rows at the LIMIT boundary.

## Fixture and build controls

TIN's initial segment count defaults to available parallelism and influences
query parallelism. It can be fixed with `initial_segment_count`; maintenance
converges toward `target_segment_count`. Mutable folding defaults include a
4 MiB threshold and a 16,384-document trigger. The dead-row rewrite threshold
defaults to 0.5. Preserve index reloptions with every result.
[Index options](https://planetscale.com/docs/postgres/search/reference/indexes)

Build memory is another confounder: TIN divides `maintenance_work_mem` between
workers, and insufficient memory can produce fragmentation that persists until
reindexing. The docs describe roughly 1 GiB per build worker as preferable, not
a universal allocation guarantee. `effective_io_concurrency` controls readahead
hints; a positive value can help storage reads but adds overhead when everything
is in shared buffers. Query parallelism is also capped by Postgres worker
settings. [Operational guidance](https://planetscale.com/docs/postgres/search/operations)

**Proposed experiment:** first compare normal defaults, which reflect the user
experience. If behavior differs, repeat a small representative subset with the
same segment count, session build budget, query worker limit, and `work_mem`.
Do not describe identical input rows as identical physical indexes. Use actual
relation sizes and observed reads to characterize the working set; repetitive
text can compress too well to create intended memory pressure.

## Query probes ranked by expected value

### 1. Indexed and unindexed filter crossover

TIN documents btree point/range predicates narrowing text search. Unindexed
predicates instead filter text candidates, potentially requiring deeper ranking
to fill a LIMIT. It also supports SQL OR between a text predicate and primary
key lookup. [Recommended SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes)

Sweep 0.1%, 1%, 5%, 10%, 20%, and 50% selectivity with LIMIT 1, 10, 100, and
1,000. Compare clustered ID ranges with scattered tenant predicates at equal
cardinality: the same number of matching TIDs can occupy very different numbers
of heap pages. Add positively and negatively correlated text/filter populations.
The hypothesis is that candidate-page locality and intersection cardinality
matter beyond independent predicate selectivities. Force alternative strategies
at the most informative points instead of expanding every Cartesian product.

### 2. Position and dictionary expansion cost

TINQL supports phrases, one-word phrase gaps, phrase tolerance, ordered
`THEN/N`, unordered `NEAR/N`, and document-position windows. `/N` counts extra
words, with zero meaning adjacency. Wildcards include prefix and suffix forms;
fuzzy syntax supports an explicit stable-prefix length. Regex matching uses
normalized dictionary terms and does not itself fold the pattern.
[TINQL reference](https://planetscale.com/docs/postgres/search/tinql)

Use paired queries such as `alpha AND beta`, `"alpha beta"`, `"alpha _ beta"`,
`"alpha beta"~2`, `alpha THEN/2 beta`, and `alpha NEAR/2 beta`. Place the same
terms early versus late in long documents and increase their occurrence count.
Separately increase the number of dictionary terms matched by `prefix*`,
`*suffix`, and fuzzy queries while holding matching document count approximately
constant. The hypotheses are delayed positional reads and dictionary expansion
cost. Compare position-page counters, planning time, and execution time; do not
infer an exact positional encoding.

### 3. Prepared executions and outer-parameter rescans

Postgres auto mode initially executes five custom plans, then compares their
average estimated cost with a generic plan. Force-custom and force-generic modes
provide controls. DDL or updated planner statistics can cause replanning.
[Postgres PREPARE](https://www.postgresql.org/docs/18/sql-prepare.html)

Alternate rare and common terms and selective/unselective filters in one
persistent prepared statement. Repeat with reversed first-five history and a
parameterized LIMIT. Preserve per-execution plans and prepared-plan counters.
Do not issue ANALYZE or DDL mid-sequence. TIN also documents a LATERAL query whose
search text comes from the outer row: this is useful for checking repeated
binding and top-k per outer value.
[Recommended SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes)

### 4. Churn, visibility, and scoring

TIN uses page-oriented bitmaps, CTID postings, and liveness information. Its
announcement describes count shortcuts for disjoint page sets and visibility-map
intersections; mutable segments are folded and immutable segments merged in the
background. These are published architectural claims, not details recovered
from EXPLAIN. [TIN announcement](https://planetscale.com/blog/introducing-tin)

Run count and full-score queries before updates, immediately after updates and
deletes, after VACUUM, and after maintenance settles. Keep a small independently
verifiable fixture for membership checks. TIN's live results obey snapshot
visibility, but BM25 corpus statistics can include dead entries until VACUUM
and segment rewriting remove them. Therefore score changes across maintenance
phases need not be a correctness failure.
[Scoring and visibility](https://planetscale.com/docs/postgres/search/scoring)

Measure transient cost with bounded mutation batches and record background
maintenance overlap. A stable file size is not proof that maintenance did
nothing: freed TIN space is reused internally, while shrinking the relation
requires REINDEX. [Limitations](https://planetscale.com/docs/postgres/search/reference/limitations)

### 5. Dense-term scoring boundary

The scoring docs describe dense-term elision, explicit boosts pinning terms,
`full_score` retaining all terms, and `score_inspect` exposing the selected term
set. Inspect terms around the boundary and compare score, full-score, and an
explicit `^1.0` boost. The existing catalog observed retention at 10% and
elision at 10.1%; the docs describe 10% or more. Preserve this version-specific
discrepancy rather than changing recorded observations to match prose.
[Scoring reference](https://planetscale.com/docs/postgres/search/scoring),
[observed catalog](../benchmarks/tin-plan-catalog.md)

## What the counters can and cannot prove

Use `EXPLAIN (ANALYZE, BUFFERS, SETTINGS, TIMING OFF, FORMAT JSON)` for diagnostic
queries. TIMING OFF still measures whole-statement execution while reducing
per-node timing overhead. EXPLAIN does not send normal result rows to the client;
its execution time excludes result network transfer. PostgreSQL 18's `MEMORY`
option describes **planning memory**, not total executor RSS. Buffer counts can
include repeated access and parent-node totals include descendants; do not sum
all nodes into an I/O total. Planner cost units are not milliseconds.
[Postgres EXPLAIN](https://www.postgresql.org/docs/18/sql-explain.html)

The searched public TIN documentation did not define every `Predicted Work` or
`Page Touches` field. Retain raw JSON and treat predicted values as estimates.
Counter-name interpretations are hypotheses until documented or independently
validated. Distinct page reuse counters improve interpretation but still do not
establish physical storage I/O: operating-system caching sits below shared
buffers. A count node returning one row reports its aggregate output, not the
number of matched documents. An unchanged visible plan does not prove unchanged
internal execution or rule out adaptive behavior.

For concurrency, record end-to-end latency separately from server execution,
use persistent connections, and run 1/2/4/8-client phases sequentially. Reserve
instrumented EXPLAIN probes for representative points; instrumentation is not a
substitute for an ordinary query throughput run. Do not label the first run
"cold cache" without evidence: building the index itself has already touched
data, and reconnecting does not evict server caches.

## Follow-up: actual layout, progress, and completion

The public TIN function reference documents `tin.fsck(index, heapcheck => true)`
as a read-only structural and heap/TID check. An empty result indicates success;
ownership is required. It neither repairs nor inventories the index. That page
does **not** document a segment-list function, a segment-count function, or a
maintenance-drain function. No public definition of every work counter was found
in the documentation reviewed. These are research limits, not proof that the
installed extension lacks additional APIs.
[TIN SQL functions](https://planetscale.com/docs/postgres/search/reference/functions)

Inspect installed signatures and comments before considering any undocumented
call. This proposed read-only inventory does not execute TIN functions:

```sql
SELECT p.oid::regprocedure::text AS signature,
       p.prokind,
       pg_get_function_result(p.oid) AS result_type,
       obj_description(p.oid, 'pg_proc') AS description
FROM pg_proc AS p
JOIN pg_namespace AS n ON n.oid = p.pronamespace
WHERE n.nspname = 'tin'
ORDER BY signature;
```

The catalog provides functions/procedures and their kinds; the information
functions expose reconstructed signatures/results and object comments. A
declared volatility is not sufficient proof that an undocumented function is
safe or has a particular maintenance scope.
[pg_proc](https://www.postgresql.org/docs/18/catalog-pg-proc.html),
[system information functions](https://www.postgresql.org/docs/18/functions-info.html)

For build progress, sample from a second connection:

```sql
SELECT pid, command, phase, index_relid,
       blocks_total, blocks_done, tuples_total, tuples_done
FROM pg_stat_progress_create_index
WHERE relid = 'owned_probe.documents'::regclass;
```

This standard view covers CREATE INDEX and REINDEX. Nonconcurrent CREATE INDEX
uses `index_relid = 0`, hence the table filter. Detailed counters during index
building depend on the access method's instrumentation; zero totals are not a
usable percentage. Neither this view nor reloptions is an inventory of current
TIN segments. [Postgres progress reporting](https://www.postgresql.org/docs/18/progress-reporting.html)

The documented maintenance sequence distinguishes VACUUM's dead-entry marking
from later background rewriting. There is no documented terminal drain command
in the reviewed pages. [Operational guidance](https://planetscale.com/docs/postgres/search/operations)
Use `VACUUM (ANALYZE) owned_probe.documents` followed by bounded observation;
label it "after vacuum and observed settling," not "all maintenance completed."
For a separate rebuilt endpoint, use
`REINDEX INDEX owned_probe.documents_body_tin`; REINDEX is the documented repair
and shrink operation. It changes physical layout and must not be presented as
equivalent to a completed incremental-maintenance pass.
[TIN SQL functions](https://planetscale.com/docs/postgres/search/reference/functions),
[limitations](https://planetscale.com/docs/postgres/search/reference/limitations)
