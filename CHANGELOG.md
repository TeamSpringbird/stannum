# Changelog

All notable changes to Stannum are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Entries describe
the state of the code; where a later change replaced an earlier one, only the
result is listed.

## [Unreleased]

The first Stannum release baseline, `0.1.0-dev` (`stannum.version()` returns
`0.1.0`), versioned independently of Lead.

### Added

- Queries read segments in TIN's shape in place from pinned shared buffers
  rather than copying what they read into per-backend chunks: at 150M rows
  the copies overflowed each backend's cache and every query copied some
  15,000 pages again. `stannum.native_in_place` (on) switches back for
  comparison; `EXPLAIN ANALYZE` reports native reads by kind (bytes copied,
  pages pinned, bytes stitched across page boundaries) and buffer accesses
  by phase.

- A PostgreSQL 17 and 18 index access method, `stannum`, with TINQL matching
  through `==>`, BM25 ranking (`stannum.score`, `full_score`, `max_score`,
  `score_inspect`), highlighting (`stannum.highlight`, `highlight_ansi`),
  segmented indexes and exact counts under concurrent writes.
- Segment format `TNS1`, in TIN's shape: a term's postings are a set of heap
  ctids over a grid of 256-page groups (Elias-Fano lists, grid bitmaps or
  per-page containers, the smallest), with a footer of per-block impact
  frontiers, a TF tail of bucket nibbles at each block's width, positions in
  their own stream, a document set and a DL sidecar of exact lengths; see
  [the TIN-shape guide](docs/architecture/tin-shape.md). Index pages are
  layout version 6; an index written earlier must be rebuilt (`REINDEX`).
  Ranked scans (every style, tiebreak keys and filtered walks included),
  bitmap and plain index scans, counts and `score()` of single rows read
  segments natively, a 256-page group at a time, with VACUUM's dead list as
  each segment's liveness; wildcards, regexes, ranges and fuzzy terms are
  expanded against the segment's term map first. Only queries that still do
  not lower (spans that do not need every word, position filters, `AT
  LEAST` of more than one) read a segment's terms translated into ordinal
  streams, as the write buffer's are. Merges keep the group containers of
  groups only one input holds. Dead lists are ordinal streams of dead ranks.
- Counts of Boolean term queries fold the ordinal streams a chunk at a time
  instead of visiting each match: the 302 published Wikipedia count queries
  sum to 39 ms instead of 3,767 ms in the replay harness.
  `stannum.count_fold = off` selects the page-mask and scalar strategies.
  Where the query lowers to it (terms, `AND`, `OR`, `NOT`, phrases and spans
  of plain terms) a segment is counted over its ctid sets instead, a
  256-page group at a time, trusting the visibility map per heap page
  (`stannum.count_native`).
- Pruned ranked retrieval over the ordinal streams (block-max WAND with
  per-chunk and per-sub-block bounds), with the same rows, scores and tie
  order as exhaustive scoring. It covers flat conjunctions and disjunctions,
  phrases (walked as the conjunction of their words, with positions read only
  for candidates that would enter the top k), and combinations of terms and
  phrases under `AND`, `OR`, `AT LEAST` and `AND NOT`. Dense terms that
  `stannum.score` elides join the walk as filters, and a query whose terms are
  all elided takes its top k in heap order. A ranked conjunction first
  evaluates its best-bounded chunks (`stannum.warmup_chunks`,
  `stannum.warmup_min_matches`).
- `stannum.verify_index(index, heap_check)`, which walks a whole index and
  lists every inconsistency with a severity and location, and
  `stannum.segment_info` for inspecting the segment layout.
- Tiered segment merges with per-fold budgets (`stannum.max_merge_docs`), one
  merge per insert outside the metadata lock (`stannum.deferred_merge_docs`),
  and deferred merges, dead lists and rewrites in VACUUM, which holds the
  metadata lock only to publish. Merges combine the inputs' sorted
  dictionaries and streams directly through a validated, interruptible API.
- Background maintenance workers when the library is preloaded
  (`shared_preload_libraries = 'stannum'`): a launcher starts one worker at a
  time for a database with queued work, and a fold queues the merges,
  rewrites and reclamation it leaves behind instead of running them in the
  writing session, which falls back to its inline merges when the worker pool
  or the queue is full. `stannum.index_maintenance_mode` (`background`,
  `foreground`, `manual`) chooses who maintains; `stannum.maintenance_jobs_per_db`
  (a reload setting, as in TIN) shares the worker between databases.
  `stannum.maintenance_status()` and `stannum.maintenance_jobs()` show the
  launcher, the worker and the queue. Hot standbys run no workers. See
  [maintenance workers](docs/architecture/maintenance-workers.md).
- The write buffer is TIN's mutable write segment: at
  `stannum.write_buffer_docs` (12,288) or `write_buffer_bytes` (4 MiB, the
  index's `max_mutable_segment_size`) it is sealed in place, and a sealed
  segment is promoted into an immutable one by a worker, by the inserting
  session after publishing, or in manual mode by `promote()`, VACUUM or a
  third seal. Searches read sealed segments; elision counts immutable
  segments only, as TIN's does.
- `stannum.promote(index, extent_cap_bytes)` promotes the sealed write
  segments now (with a cap, into several segments each), and
  `stannum.merge(index, target_segment_count, high_water_multiplier,
  max_fan_in, force)` merges toward a target; both have TIN's signatures and
  need the `MAINTAIN` privilege. `stannum.segment_info` has TIN's columns
  (`npostings`, `source_state`, `origin`, `sequence` among them) in TIN's
  order, lists sealed segments, and adds `generation`.
- The TIN-named index options `target_segment_count`,
  `max_mutable_segment_size`, `max_merged_segment_size` and
  `dead_percent_threshold` shape maintenance for the index that sets them,
  within TIN's domains (1..4096, at least 131,072 bytes, at least 100 MB);
  other values fail with SQLSTATE 22023. Unset, the `stannum.*` settings
  apply. `initial_segment_count` is accepted and ignored with a warning.
- Hot-standby index reads when the extension is preloaded on the primary and
  standby, through removal-horizon WAL records from a custom resource manager.
- The [TIN conformance suite](conformance/README.md): engine-agnostic,
  declarative cases (the 140 cases of the TIN behavior catalog among them),
  PlanetScale TIN 1.0.3's recorded answers, and a runner that records one
  engine and checks another. Documented divergences report as `IMPROVED`
  (TIN refuses, Stannum answers) or `GAP` (Stannum lacks the feature), and a
  declared divergence cannot hide a regression. Against TIN 1.0.3: 185 PASS,
  13 DIFF (error wording), 5 IMPROVED, 6 GAP, 3 LIMITED, 4 NEWER, 0 FAIL;
  against 1.0.4: 219 PASS, 13 DIFF, 5 IMPROVED, 10 GAP, 2 LIMITED, 4 NEWER,
  0 FAIL.
- Limits on query size: 1,000 nesting levels, 10,000 terms and 2,000 levels
  of span nesting, each an ERROR naming the byte offset. Every recursive pass
  over a query also calls PostgreSQL's `check_stack_depth`, so a smaller stack
  ends in "stack depth limit exceeded". `AT LEAST n OF [k operands]` inside a
  proximity operator, relation or positional filter, which is matched as the
  disjunction of its C(k, n) combinations, is limited to 10,000 combinations
  and to 100,000 operands added by the expansion. A regex or wildcard
  compiles to at most 2 MiB per automaton, and a query's to 256 MiB in all.
  A query past any of these limits fails with SQLSTATE 54001
  (`statement_too_complex`).
- Term expansions answer a cancel or `statement_timeout`: scanning a
  dictionary checks for interrupts every 1,024 entries. Ranking scores every
  term the wildcards, regexes, ranges and fuzzy terms of a query expand to,
  up to `stannum.max_expansion_terms` (default 65,536) in all; past it the
  query fails with SQLSTATE 54000 (`program_limit_exceeded`) rather than
  scoring some of them.
- A versioned schema snapshot with an automatic fresh-install and upgrade
  comparison, an explicit page and segment compatibility policy, and a
  release procedure.
- Benchmark harness: `--save-database` and `--load-database` let one built,
  vacuumed and checked database serve every workload of a campaign;
  `--ranked-validation-queries` samples the exhaustive ranked check;
  `build-image --base` builds a native image for local runs.

### Changed

- TINQL query expressions are parsed by a recursive-descent parser; the pest
  expression grammar remains only as a test-only differential oracle, and
  phrase contents are still parsed with pest. AND and OR chains parse into
  one flat node however long, a bracket level takes about 0.95 KiB of stack
  instead of 4.5 KiB, and `MATCHES` patterns scan in linear time where the
  pest grammar backtracked exponentially over unclosed groups. The `AT LEAST`
  estimate is linear for thresholds near either end.
- An invalid `==>` query raises its error in TIN 1.0.3's form,
  `invalid ==> query at byte N in "QUERY": ...` (without the byte when the
  error names none; a query over 1 KiB is quoted up to 1 KiB). The SQLSTATE is
  unchanged. Syntax errors name what was expected in the descent parser's
  words, not TIN's grammar rules.
- A term repeated in a flat AND or OR chain adds its boosts in scoring and
  `score_inspect`, as in TIN 1.0.3: `a a` scores as `a^2`. Matching is
  unchanged.
- `score()` and `full_score()` over `==>` clauses on several indexed columns
  of one table sum one score per column in clause order, as TIN 1.0.3 does.
  Such a sum is sorted over the matches rather than ranked by the index scan,
  and `max_score()` reports the first column's best score.
- `highlight()` and `highlight_ansi()` without a query take it from a `==>`
  clause anywhere in the query's join tree, so a CTE or subquery the planner
  flattens binds, and with no clause to bind they return the text unmarked,
  as TIN 1.0.3 does.
- A pruned ranked scan that must read past its top k deepens the pruned
  search (to four times the depth, up to 4,096 rows) before it scores every
  match, and checks each row's snapshot visibility as it enters the top k.
  With eight clients ranking disjunctions over 15 million rows beside 1,000
  updates a second, throughput went from 2 to 125 queries a second.
  If the deepest walk still falls short, a filter that is not volatile is
  applied by one more walk as it admits rows instead of scoring every match.
- `ORDER BY score DESC, <other keys> LIMIT k` uses the pruned ranked scan
  under an incremental sort; its top k keeps every row tied with the k-th
  score. At a million Stack Exchange rows the comparison kit's tiebreak
  disjunctions went from 7.7 s (a sequential scan and a sort) to 0.3 ms.
- The planner prices a pruned ranked scan by the candidates its walks score
  rather than by every match, so `... AND id <= K ORDER BY score DESC LIMIT
  10` chooses it over the primary key unless the filter is selective: the
  comparison kit's filtered disjunctions went from 680 ms to 2.4 ms.
- A ranked walk reads ordinal chunks, position spans and its candidates'
  length and class pages in place from pinned shared-buffer pages instead of
  copying them per backend.
- A backend's private memory no longer grows with what a query reads: cursors
  own bounded buffers, shared through a least-recently-used cache of
  `stannum.read_cache_mb` (64 MiB) per backend; the segment readers' headers,
  dictionary samples, page tables and decoded dead lists are bounded by
  `stannum.reader_cache_mb` (384 MiB), and each captured view drops those of
  segment generations a merge retired. A view releases the meta page before it loads segment readers.
- The on-disk directory holds 96 entries and the pending-free list 48.
  `stannum.max_segments` is a soft bound enforced within the insert merge
  budget; only the 96-entry bound forces an unbudgeted merge.
- An index build compacts its directory to the fewest segments the 3 GiB
  merge cap allows, then packs its live runs into the lowest pages and
  truncates the relation.
- A merge takes at most 3 GiB of input, dropping its largest members until it
  fits; writing a segment longer than a run's 32-bit length is an error.
- Inserts free at most `stannum.reclaim_pages` pages of retired runs at a
  time, and each run records its last page, so retiring a run no longer walks
  it under the meta lock.
- Physical index diagnostics check heap permissions and row security;
  catalog-dependent SQL functions are STABLE rather than IMMUTABLE.
- `stannum.debug_seed_score` is superuser-only: a plain role could set it and
  make a ranked query return wrong or no rows.

### Fixed

- A backend decodes a segment's dead list into a bitmap over its ordinals, at
  most a bit per document, instead of a set of heap locations plus a vector of
  ordinals, 16 to 24 bytes per dead document. After VACUUM published 45
  million dead rows of 150 million, eight query backends held about a
  gigabyte each, rebuilt it on every query once it overflowed
  `stannum.reader_cache_mb`, and the server was killed for memory.
- Draining the pending list, and joining a retired run to the pending chain,
  wait until the meta page is written. An error or crash in between left the
  meta page listing runs whose pages were already free, and a later drain
  could free a live segment's page (`62cd0fc`).
- VACUUM records FREE pages the free space map lost. The map is not
  WAL-logged, so after a crash or on a promoted standby such pages were never
  reused and the index grew until `REINDEX` (`5abb6e6`).
- A fold or VACUUM that replaces the write buffer writes the new contents to
  pages the published buffer does not cover. A failure before the meta page
  was written could pair the old meta page with rewritten pages, losing
  buffered rows (`05f6f4c`).
- VACUUM's orphan pass holds the index's maintenance lock, so it no longer
  frees the pages of a deferred merge that is about to be published
  (`0626d91`).
- Counts that do not fold (phrases, `NOT`, prefixes and other expansions)
  read the visibility map after capturing their view and confirm the view is
  still current, so a row VACUUM removed meanwhile is not counted
  (`0f785be`).
- An ordered span with a phrase operand after its first position (for
  example `a THEN/1 "b c"`) tested the wrong word pair and missed rows
  (`290f633`).
- Pruned ranking at `k1` near zero no longer drops the true top row when
  rounding leaves a higher frequency bucket an ulp below a lower one
  (`290f633`).
- A conjunction's warm-up passes give up their pinned pages as each pass
  ends; a later pass could read a chunk from a page no longer pinned
  (`a147da3`).
- A backend that exits during a ranked walk leaves the walk's pins to
  PostgreSQL instead of releasing them a second time, which crashed the
  backend and restarted the server (`e2e3e58`).
- An oversized or deeply nested query (30,000 words, a 30,000-term OR chain,
  5,000 nested parentheses) is an ERROR instead of a stack overflow that
  restarted every session (`23afe78`, `1b580f9`).
- A deleted document is no longer scored by the disjunction walk; its reused
  location could be returned for a row that never matched.
- A dead list replaced by VACUUM in the same pages at the same size is no
  longer served from a reader's cache: each dead list carries a stamp.
- Counts check a heap page's matches under one buffer lock, and restart if
  VACUUM publishes a dead list after their view.
- Per-row scores no longer depend on the order the executor hands rows over
  in (a join scores rows in its own order).
- Packing a built index marks the pages it reuses as used in the free space
  map; stale entries made inserts walk the map under the meta lock for
  seconds at a time.
- VACUUM reclaims pages a crash left unreferenced (`page N` warnings of
  `stannum.verify_index`) instead of requiring `REINDEX`.

### Removed

- Readers for the segment formats of earlier development builds, and the
  setting that chose between their ranked paths. Indexes built by those
  builds must be rebuilt with `REINDEX`.
