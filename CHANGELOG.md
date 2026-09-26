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

- A PostgreSQL 17 and 18 index access method, `stannum`, with TINQL matching
  through `==>`, BM25 ranking (`stannum.score`, `full_score`, `max_score`,
  `score_inspect`), highlighting (`stannum.highlight`, `highlight_ansi`),
  segmented indexes and exact counts under concurrent writes.
- Segment format `STN3`. A term's documents are stored once, as ordinals into
  the segment's document table, with each member's term-frequency bucket
  beside it and a score bound per 65,536-document chunk and per
  1,024-document sub-block, so scoring never reads positions. Positions have
  their own stream, read only by positional queries. The document table maps
  ordinals to heap locations and back through a page table and a two-byte
  offset per document, and a one-byte length class per document lets a
  ranked walk bound a candidate before reading its length. Dead lists are
  ordinal streams. On the 150 million row Stack Exchange corpus the index is
  47 GB, against 246.5 GB for the earlier `LSG5` format.
- Counts of Boolean term queries fold the ordinal streams a chunk at a time
  instead of visiting each match: the 302 published Wikipedia count queries
  sum to 39 ms instead of 3,767 ms in the replay harness.
  `stannum.count_fold = off` selects the page-mask and scalar strategies.
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
  dictionaries and streams directly through a validated, interruptible API
  ([ADR 0001](docs/adr/0001-preserve-posting-order-before-changing-encoding.md)).
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
  declared divergence cannot hide a regression. Against TIN 1.0.3: 170 PASS,
  12 DIFF (error wording), 5 IMPROVED, 2 GAP, 0 FAIL.
- Limits on query size: 1,000 nesting levels, 10,000 terms and 2,000 levels
  of span nesting, each an ERROR naming the byte offset. Every recursive pass
  over a query also calls PostgreSQL's `check_stack_depth`, so a smaller stack
  ends in "stack depth limit exceeded".
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
- A ranked walk reads ordinal chunks, position spans and its candidates'
  length and class pages in place from pinned shared-buffer pages instead of
  copying them per backend.
- A backend's private memory no longer grows with what a query reads: cursors
  own bounded buffers, shared through a least-recently-used cache of
  `stannum.read_cache_mb` (64 MiB) per backend; the segment readers' headers,
  dictionary samples and page tables are bounded by `stannum.reader_cache_mb`
  (384 MiB). A view releases the meta page before it loads segment readers.
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

- Readers for the development formats `LSG1` to `LSG5`, `STN1` and `STN2`.
  Indexes in them must be rebuilt with `REINDEX`.
- The `stannum.rank_by_ordinal` setting: ranking over the ordinal streams is
  the only ranked path.
