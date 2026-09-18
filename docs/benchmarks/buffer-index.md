# Reader cost of the write buffer and small folds

The [merge-budget experiment](merge-budget.md) folded the write buffer at 512
documents or 1 MiB instead of 16,384 documents or 4 MiB, which cut the worst
insert from about 430 ms to about 150 ms but cost readers about 1.7% p99 and
2.6% throughput in its isolation run, and the [integrated comparison](ranked-integration.md)
showed about 4% p99 and 7% throughput against the previous build. Every
backend also builds its own in-memory index of the buffer. This note profiles
where that reader time goes, records what was changed to recover it, and
revisits the fold caps with measurements.

## Protocol and limits

- PostgreSQL 18.6 on the shared local pgrx server (localhost:28818), release
  builds of `9b0bf47` (baseline) and `9b0bf47+buffer-index` (this change).
  Both binaries were built from the same commit tree and copied into the
  shared install at the start of every window; each window records the
  installed SHA256, because other agents install their own builds between
  windows on this machine.
- Verified Wikipedia 100,000-document corpus at
  `$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-100000`.
- **Controlled states** for profiling: the corpus built into 4 segments, then
  7,000 rows inserted from `benchmark_pool` and 2,000 deleted followed by
  `VACUUM (INDEX_CLEANUP ON)`, under the new caps (`new`: 12 immutable
  segments plus a 130-document buffer) or the old caps (`old`: 7 segments plus a
  491-document buffer). Reads are the harness's ten count and ten ranked
  shapes through `pgbench -c 2`, 45-second windows, baseline and patched
  binaries alternated within one locked window per state, no writers.
- **Profiles**: `sample` (macOS) of one reader backend for 30–40 seconds
  while `pgbench -c 1` loops over a mix of shapes; inclusive counts per
  function from the call graph.
- **Fresh connections**: eight new `psql` connections per case, each running
  `EXPLAIN ANALYZE` of the `history` ranked query three times; planning plus
  execution time of the first, second and third statement. The buffer index
  and the segment readers are built during planning (the selectivity estimate
  reads the index), so execution time alone hides the cost.
- **Mutation windows**: `python3 benchmarks/run.py run --profile mutation`,
  600-second timed windows, 30-second warmup, two readers, 50 scheduled
  mutations/second with equal insert/delete/update weights, seed 1729, checks
  every 30 seconds, scheduled `VACUUM (INDEX_CLEANUP ON)` every 60 seconds,
  custom scans on, one window per configuration, each in a fresh
  `stannum_bench_*` database dropped afterwards.
- A machine-wide lock covers every install, test and measurement window, but
  the machine is shared with other agents' builds and tests. Bursts of
  contention are visible in the raw logs (one probe measured the same
  0.9 ms statement at 11 ms while a foreign `rustc` ran at 98% CPU). Paired
  and alternated windows limit that; single 600-second windows do not, and
  none of the numbers below are confidence intervals.

## Where reader time goes

Per-query latency on the controlled states with the baseline binary
(`pgbench` per-statement times, ms):

| Shape | `old` (7 segments, 491 buffered) p50 / p99 | `new` (12 segments, 130 buffered) p50 / p99 |
| --- | ---: | ---: |
| count, all ten shapes | 0.244 / 2.506 | 0.282 / 2.545 |
| ranked, all ten shapes | 0.720 / 4.899 | 0.785 / 4.997 |
| `history` count / ranked | 0.741 / 0.848 | 0.751 / 0.871 |
| `quasar` count / ranked | 0.128 / 0.425 | 0.161 / 0.472 |
| `"united states"` count / ranked | 2.378 / 4.720 | 2.356 / 4.663 |
| reads/s | 2,391 | 2,285 |

Five more small segments cost each count query roughly 8 µs per segment and
each ranked query roughly 12 µs per segment, independent of how much work
the query itself does: the cheap shapes (`quasar`, the misses) lose 25–35%,
the expensive phrase loses nothing. The buffer, three to four times larger in
the `old` state, is not visible at this granularity. The per-segment setup,
not the buffer, is where the small-fold reader cost lives.

A sampled profile of the `history` ranked query on the `new` state (2,355
samples inside the custom scan, baseline binary) splits as:

| Inclusive share | Where |
| ---: | --- |
| 64% | `gather`: the block-max walk and scoring itself |
| 20% | `Index::term`: dictionary lookups (`Dictionary::get`, `Walker::step`) |
| 17% | `scorer_for_scan`: building the statement's scorer, including `SourceReader::new` per source, dead-list decoding to `BTreeSet`s and a second round of term lookups |
| 9% | `clause_estimate`: the planner's selectivity estimate, a third round of term lookups plus a query parse |
| 4% | `parse_tinql_to_query` (three parses per statement: estimate, scan, scorer) |

A term is looked up in every source about three times per statement, and
each lookup walks up to 64 prefix-compressed dictionary entries after a
binary search over block heads; with a dozen sources that is a few hundred
walks per statement. The selectivity memo is cleared at every executor
start, so planning repeats the lookups too. Dead lists were decoded into a
`BTreeSet` per source per statement.

The write buffer's own cost is small by comparison. Turning encoded forward
records into the in-memory index costs (release build, `segment/tests/buffer_cost.rs`,
whitespace-tokenized Wikipedia records of about 3.3 KiB):

| Records | Bytes | Before | After |
| ---: | ---: | ---: | ---: |
| 512 | 1.69 MB | 21.3 ms | 17.9 ms |
| 1,460 | 4.23 MB | 54.7 ms | 46.2 ms |
| 4,096 | 11.18 MB | 144.9 ms | 110.1 ms |

Decoding the records is now more than half of it. An existing backend pays
this once per record as it arrives (about 35 µs per document, or about
1 ms/s of CPU per reader at 33 new records/s), and again for the whole
buffer after VACUUM rewrites it; a fresh connection pays it for the whole
buffer on its first query. A fold does not cost anything here: it empties
the buffer, so the next index starts empty.

## What changed

- **Dictionary lookups are memoized per cached segment** (`postgres/src/storage/mod.rs`,
  `MemoizedSegment`): the reader cache keeps, per index identity and
  generation, a map from term to entry-or-absence, at most 4,096 terms per
  segment. Segments are immutable, so the memo is valid for as long as the
  generation is in the directory, and it goes away with the reader. All
  three rounds of lookups per statement, and every later statement, hit it.
- **Dead lists are decoded once per backend and dead run**: the cached
  segment keeps the `BTreeSet` next to the dead-list bytes, and the view
  hands it to the scorer instead of the scorer decoding it per statement.
- **`MutableIndex` stores positions flat per term** (`segment/src/index.rs`):
  one vector of positions and one of occurrence descriptors per term instead
  of one allocation per term occurrence, with an Fx hasher for the term maps.
  Encoding per term on first use is unchanged.
- The buffer index stays per backend; see the architecture note for why
  sharing it was not worth a shared-memory dependency at these sizes.

## Results

### Read-only, controlled states, baseline and patched alternated

`pgbench -c 2`, 45-second windows, all twenty shapes; per-statement latency in ms.

| State | Build | count p50 / p99 | ranked p50 / p99 | reads/s | Fresh connection: first / second / third statement |
| --- | --- | ---: | ---: | ---: | ---: |
| `new` (12 segments, 130 buffered) | baseline | 0.280 / 2.542 | 0.787 / 5.062 | 2,264 | 5.70 / 0.75 / 0.74 |
| `new` | patched | 0.167 / 2.455 | 0.554 / 4.875 | 2,629 | 5.12 / 0.63 / 0.62 |
| `old` (7 segments, 491 buffered) | baseline | 0.244 / 2.506 | 0.713 / 4.958 | 2,386 | 18.66 / 0.76 / 0.74 |
| `old` | patched | 0.164 / 2.438 | 0.545 / 4.834 | 2,644 | 16.39 / 0.66 / 0.64 |

The patched build removes 40% of the median count latency and 30% of the
median ranked latency in the `new` state and 16% more throughput; the p99s,
dominated by the phrase shapes' payload work, move 3–4%. The five extra
segments of the `new` state, worth 15% of median latency on the baseline,
are worth 2% on the patched build: the per-segment setup that made small
folds expensive is gone, and the patched `new` state is faster than the
baseline `old` state on every column.

The fresh-connection column is planning plus execution of the first
statement on a new connection, which builds the segment readers (three to
twelve of them), the buffer index and the planner's estimate: 5 ms with a
130-document buffer and 12 segments, 16–19 ms with a 491-document buffer and 7
segments. The second statement is within 0.02 ms of the third. On the shared
machine the same probe measured 44 ms and 128 ms once while a foreign build
ran; the paired numbers above were taken back to back.

### Profile, `history` ranked, `new` state

Inclusive share of samples inside the custom scan (one reader, `pgbench -c 1`,
30 s): baseline 15,113 samples at 1,039 statements/s; patched 15,429 samples at
1,284 statements/s.

| Where | Baseline | Patched |
| --- | ---: | ---: |
| block-max walk and scoring (`gather` minus setup) | 64% | 88% |
| dictionary lookups (`Index::term`) | 20% | below 1% |
| scorer construction (`scorer_for_scan`) | 13% | 1.7% |
| planner estimate (`clause_estimate`) | 8% | 3.4% |

What remains is the query's own work: seeking postings, decoding blocks and
scoring candidates, whose cost does not depend on the number of segments
(`Scored Candidates: 2531` either way), plus one query parse per phase.

### Mutation windows: fold caps with the patched build

Writer values are execution latency (pgbench schedule lag subtracted), p99 /
maximum over the whole run including drain. Reader p99 combines all queries
of the shape. One window each.

| Run | Caps (docs / bytes) | Insert p99 / max | Delete p99 / max | Update p99 / max |
| --- | ---: | ---: | ---: | ---: |
| Baseline `9b0bf47` | 512 / 1 MiB | 1.873 / 117.182 | 1.471 / 19.094 | 2.391 / 88.110 |
| Patched | 512 / 1 MiB | 1.343 / 72.815 | 0.972 / 2.608 | 1.988 / 165.083 |
| Patched | 2,048 / 4 MiB | 1.438 / 283.500 | 0.969 / 3.080 | 1.842 / 270.251 |
| Patched | 4,096 / 8 MiB | 1.436 / 515.255 | 0.968 / 3.374 | 1.782 / 599.288 |

| Run | Caps | Count p50 / p99 | Ranked p50 / p99 | Reads/s | Segments min–max / final | Max buffered docs | Check rounds |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline | 512 / 1 MiB | 0.439 / 3.303 | 0.990 / 6.581 | 1,778.8 | 4–20 / 14 | 509 | 20 |
| Patched | 512 / 1 MiB | 0.330 / 3.097 | 0.775 / 6.038 | 2,089.7 | 4–20 / 14 | 504 | 20 |
| Patched | 2,048 / 4 MiB | 0.323 / 3.069 | 0.769 / 5.992 | 2,126.2 | 4–11 / 6 | 2,008 | 20 |
| Patched | 4,096 / 8 MiB | 0.323 / 3.114 | 0.768 / 6.038 | 2,115.2 | 4–8 / 8 | 4,028 | 20 |

Every window passed all twenty periodic oracle rounds and the final
post-traffic round; the binary hash was unchanged across each window.

At the default caps the patched build serves 17.5% more reads per second
than the baseline, with count p99 6% and ranked p99 8% lower and medians 25%
and 22% lower, against the same directory shape (4–20 segments, the same
folds and VACUUM merges at the same times). That is more than the 4%/7%
reader cost the merge-budget change was measured to have introduced. Insert
and delete p99 fell too (the writer shares the machine with the readers).
The single 165 ms update sits in the 480-second bucket where VACUUM merged
the directory from 20 segments to 10, as the baseline's 1.1-second VACUUM did;
its lag column shows the same value, so it is a stall behind the merge, not a
slow statement.

Raising the caps no longer buys readers anything measurable: 2,048 documents
gained 1.7% reads/s and 4,096 lost it again, with the p99s within 1.5% of the
default's. Worst writes, on the other hand, scale with the buffer: 283/270 ms
at 2,048 documents and 515/599 ms at 4,096, versus 73/165 ms at 512, because
a fold builds a segment from the whole buffer under the meta lock. The
fresh-connection cost also scales with the buffer (about 11 ms per MiB).

**Defaults stay at 512 documents and 1 MiB.** The reader cost that motivated
revisiting them came from per-segment query setup, which is now memoized, and
the remaining per-segment cost (about 2% of median latency for five extra
small segments) is far below the write-stall cost of a larger buffer.

Raw artifacts: `/tmp/stannum-bi-results/mut-before-512`, `mut-after-512`,
`mut-after-2048`, `mut-after-4096` (manifests, per-statement logs, plans,
oracle rounds, layout samples, VACUUM output, timelines), the paired
read-only logs `p1-*`, and the `*.sample` profiles.

## Verification

The measured binary (`442be435…`) was built from this branch before it was
merged with `stannum/main` (`d968a6e`, the LSG3 segment format) and the
standby WAL-horizon work (`a55a678`); the merge keeps that work's LSN
validation in `read_buffer_range`, `buffer_index` and `view` and adds the
memoized segments and dead sets inside its retry loop. Verification ran on
the merged tree:

- `cargo fmt --all --check`; PG18 and PG17 workspace/all-target clippy with
  `pg_test`, warnings denied.
- Non-extension workspace tests: 438 passed, including the new
  `out_of_order_records_keep_each_occurrence_with_its_own_positions`.
- `cargo pgrx test pg18`: 93 passed, including
  `memoized_term_lookups_stay_exact_across_folds_merges_reindex_and_drop`
  (folds, tiered merges, an update, REINDEX, DROP/CREATE INDEX, custom scan
  against bitmap path, and a memoized absence that later folds fill) and
  `buffer_index_extends_incrementally_and_restarts_on_epoch_and_identity_changes`
  (append by another statement, VACUUM's buffer rewrite, a fold and REINDEX,
  each checked through `storage::cache_probe`).
- `postgres/tests/postings_lifecycle.py`: passed, 42 index verifications, 15
  concurrent reader checks, 333 standby snapshot checks with no wrong answers.
- `script/reference-oracle` against Lead: 235/235 query/state pairs agree.
