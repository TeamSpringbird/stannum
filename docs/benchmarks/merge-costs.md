# Remaining merge costs

Foreground merges read immutable runs, reconstruct forward records, ingest
those records into a new segment builder, encode the result and write its
WAL-backed runs. These steps currently share the metadata lock. Moving fold
construction outside that lock does not move merge construction or run writes.

## Grouped-record ingestion

The old `SegmentBuilder::add_record` expanded already-grouped forward records
into individual tokens, sorted tokens by position, then regrouped them by term.
The new path copies canonical term groups directly after checking the TID,
term/position ordering and cross-term position uniqueness. A bounded bitmap
checks uniqueness for dense positions. Sparse or noncanonical public records
use the previous normalization path. Document lengths are still recomputed
from actual tokens; malformed records cannot partially modify the builder.

This optimization is confined to the segment codec and benefits both folds and
merges. It does not change storage publication, WAL ordering, reclamation, the
file format or the amount of segment output. Avoiding sort/regroup work does
not establish that CPU dominates every observed foreground stall.

Differential tests compare the complete output bytes against the old token
path for all three supported formats, including document lengths, positions
and score bounds. Additional tests cover duplicate positions across terms,
empty groups, duplicate/unsorted term groups, sparse positions, invalid TIDs,
duplicate TIDs and mismatched caller-supplied document lengths. A property test
compares acceptance, errors and output for generated records.

An optional microprobe alternates old/new ingestion paths for 512 documents at
40 and 400 tokens each, discards two warmup rounds and reports ten samples per
path. It excludes segment encoding, run reads, writes and WAL; its result is
not database throughput or end-to-end insert latency:

```sh
cargo test -p segment --release grouped_record_ingestion_microprobe -- --ignored --nocapture
```

A local ARM64 release-mode run measured median ingestion time for 512 documents
at 0.443 ms versus 0.094 ms for 40 tokens/document, and 3.253 ms versus 0.439 ms
for 400 tokens/document. These synthetic two-term records isolate removed
sorting/regrouping work; representative database measurements remain necessary.

## Repeatable database probe

`benchmarks/merge_costs.py` wraps the existing single-writer
`foreground_writes.py` attribution probe. Each budget gets identical input in
a fresh disposable database. The default three rounds rotate the order of
budgets 0, 256 and 2048 so each occupies every position once. A budget of 256
can merge eight 32-document runs; 2048 also permits a later merge of eight
256-document runs when the cumulative per-insert budget allows it. The default
4,161-document fixture with a 32-document buffer reaches the 128-segment hard
directory bound even when optional merges are disabled. Zero budget therefore
does not mean zero merge work.

```sh
python3 benchmarks/merge_costs.py \
  --docs 4161 --repeat 20 --write-buffer-docs 32 \
  --budgets 0 256 2048 --rounds 3 \
  --artifact /path/to/installed/stannum.so \
  --output benchmarks/results/merge-costs/baseline
```

Run on a dedicated server; hold the extension installation lock for the whole
campaign on shared machines. Run the same command on the changed library with
a new output directory, then repeat with `--repeat 200` for longer documents.
The wrapper rejects mixed build hashes, fixtures, server settings, incomplete
rounds and failed correctness checks. Raw SQL, JSON EXPLAIN output and per-insert
samples remain in each window directory. Windows preserve individual group
latencies; campaign totals are medians of per-window totals, not a pooled p99.

The probe records complete INSERT execution time, WAL bytes, shared buffer
reads, visible retired document counts and final segment count. These counters
do **not** separate CPU construction from input reading, segment writing or
WAL latency. Retired document counts omit intermediate runs in cascaded merges.
Changing the budget changes segment layout and maintenance backlog, so compare
the same budget/fixture between builds before attributing a gain to code.

The probe uses warm index inspection between inserts, one writer, no readers,
and EXPLAIN execution times that exclude commit/fsync. Use the concurrent
contention harness for lock-wait and reader-impact validation. Writing segment
runs outside the metadata lock still needs a separate reservation/reclamation
protocol; this optimization does not make that publication change safe.
