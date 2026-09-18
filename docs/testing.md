# Testing the ranked scan under concurrency

The ranked (top-k) scan keeps state across the life of a scan: the scorer it
built, the rows it pruned to, and the write-buffer index it retained. Two bugs
in one day (rows re-emitted after a deletion, fixed in `45d2696`; a retained
document scored with another document's length after a buffer refresh, fixed
in `9d3a7a0`) were caught only by the sustained-mutation benchmark, whose
checks are not an oracle. `postgres/tests/ranked_fuzz.py` is the oracle: a
randomized concurrency fuzzer for this class of bug.

## What the fuzzer does

It starts a throwaway cluster (install stannum first; standard library plus
`psql`, autovacuum off so every VACUUM is a scheduled step) and builds a small
corpus from a dozen templates, so exact score ties abound, in a table with a
random fillfactor. Several **writer sessions** then run random operations:
inserts, deletes, non-HOT updates (new body), HOT updates (a non-indexed
column), VACUUM, delete-VACUUM-insert cycles that reuse heap line pointers,
rolled-back inserts, multi-statement transactions, `keep`-table churn for
joins, and per-session tunables (`stannum.write_buffer_docs`,
`write_buffer_bytes`, `merge_tier_factor`, `max_segments`, `max_merge_docs`,
`build_segment_docs`) drawn from small values so folds, tiered merges and
deferred merges happen constantly. REINDEX and CREATE INDEX CONCURRENTLY run
only between reader transactions, so a lock wait cannot stall the scheduler.

**Reader sessions** run episodes. An episode opens a transaction (mostly
REPEATABLE READ, sometimes READ COMMITTED), pins its snapshot, lets writers
churn, and then, in one quiescent batch with no writer statement in flight:

1. runs the query through the unpruned path (`stannum.enable_custom_scan = off`,
   `ORDER BY score DESC, ctid`) for every matching row;
2. runs a regex sequential scan with the same predicate (`\mterm\M` per term,
   combined as the query combines them) and checks the match sets are equal;
3. reads the heap's HOT chains through `pageinspect`, since the scan orders a
   HOT-updated row by the root it posted while the executor projects the
   member's `ctid`;
4. runs the custom-scan query with `LIMIT`/`OFFSET`, or declares a cursor and
   fetches its first rows.

Statistics (document counts, lengths, frequencies) include buffered and
dead documents, so scores drift with every write; the quiescent batch is what
makes bit-for-bit comparison valid. A cursor then keeps fetching while writers
churn, sometimes with the fetch and a write overlapping in time, and its
whole output must equal the oracle's prefix: the scan retained its scorer, so
its scores must not move. A second cursor in the same transaction (same or
another query, its own oracle) checks that two scans do not share state.

Queries are single terms, `OR`, `AND` (two or three terms), boosts, phrases,
`AND NOT`, prefixes, `AT LEAST`, and `(a OR b) AND c`, scored by `full_score`
or `score`, with limits around the 128-posting block boundary and above the
4,096-row pruning cap, offsets, joins that read past k (`enable_hashjoin` and
`enable_mergejoin` off so the scan's order reaches the top; when a Sort still
sits above the scan, ties are compared tolerantly), and filters applied above
the scan.

Every result is checked for unique ids, finite scores, descending scores with
ties in the posting's heap order, membership in the regex match set, and
equality with the oracle's slice (ids, ctids and score text with
`extra_float_digits = 3`, which round-trips float4 exactly).

On a failure the fuzzer writes `failure.json` and `repro.sql` into its
artifact directory: the seed and arguments, the failing comparison, and every
statement issued, in order, labelled by session. Re-running with the same
`--seed`, `--seconds`, `--writers`, `--readers` and `--corpus` issues the same
statements; `--stop-at N` stops after episode N.

```
python3 postgres/tests/ranked_fuzz.py --seed 7 --seconds 600
python3 postgres/tests/ranked_fuzz.py --smoke     # fixed seeds and REGRESSIONS
```

`--smoke` runs one fixed seed and the `REGRESSIONS` list in the script (each
a short configuration that once failed or pins a bug class) in under two
minutes; CI runs it after the lifecycle checks.

## Bug classes it found

- **HOT-updated rows scored zero.** The index posts the root of a HOT chain;
  the executor projects the visible member's `ctid`; `score_bound_indexed`
  looked that location up in the index, found nothing, and returned zero on
  both paths. The pruned scan then ordered such a row by its real score while
  reporting zero. `IndexScorer::score` now resolves a location absent from
  every source to its chain root (`heap_get_root_tuples` under a share lock).
  Test: `hot_updated_rows_keep_their_score_on_both_ranked_paths`.
- **Scans on one query shared a scorer.** The scan handed its scorer to the
  score functions through a slot keyed by a backend-wide statement counter,
  so a later scan (or an unpruned query) on the same query replaced it, and a
  cursor's remaining rows were projected with statistics that writes in
  between had changed, out of step with the order it ranked them in. Scans
  now publish their scorer under their own identity with the score of every
  row they ranked and record the row they emitted last, with the statement
  number and an emission stamp; a score call in that statement for exactly
  that location takes the scan's score, newest emission first (any other
  location, such as an unpruned scan's row while a cursor is open on the
  same query, or a row a paused cursor emitted in an earlier statement, is
  scored by the statement's own scorer), and the entry is dropped when the
  scan ends, including after an error. The fuzzer's first three attempts at
  this fix (newest scan wins; most recent emitter wins; exact location
  without the statement scope) each failed the same seed within three
  seconds, which is the point of running it.
  Test: `concurrent_cursors_on_one_query_keep_their_own_scores`.

The earlier two bugs remain covered by
`a_completed_ranked_scan_does_not_repeat_the_rows_it_emitted` and
`buffered_scoring_keeps_document_lengths_when_heap_space_is_reused`, and by
the fuzzer's cursor episodes, which exercise both interleavings continuously.
