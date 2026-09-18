# Concurrent write contention

`benchmarks/contention.py` complements the single-writer
[foreground attribution probe](foreground-writes.md) with concurrent writers,
readers, wait sampling, and a same-statement correctness oracle.

```sh
python3 benchmarks/contention.py \
  --seconds 20 --writers 2 --readers 2 \
  --write-buffer-docs 32 --repeat 200 --max-merge-docs 1024 \
  --sample-ms 50 --artifact /path/to/installed/stannum.so \
  --output benchmarks/results/concurrent-write-contention/baseline
```

Use a dedicated local PostgreSQL server and hold the installation lock on a
shared development machine. The named library must be the server's installed
library; hashing a local file does not identify a remote server's loaded code.
The output directory must be new. The probe creates and drops its own database.

Two independent pgbench processes run single-row INSERTs and selective
`sum(id)` searches. Each INSERT gets a unique sequence ID; its body is derived
from that ID. Raw pgbench transaction logs retain client latency, including
commit and round trips. No per-insert EXPLAIN or directory inspection runs in
the timed writer. The fixture disables autovacuum so foreground maintenance
can accumulate; this deliberately stresses folds and merges rather than
representing a tuned production workload. JIT is disabled for reproducibility.

The correctness observer compares indexed selective and broad results with
independent heap/ID predicates within one statement snapshot. Multiset
comparison detects missing, extra, and duplicate rows. Only checks fully
contained within the observed overlap of reader/writer processes count as
concurrent; a run without such a check fails. Final checks reconcile successful
logged INSERTs with heap growth and index document totals, and run the deep
index verifier. Transaction failures and incomplete logs fail the run.

A separate observer samples active backend wait events and directory generations.
`BufferContent` observations do not identify a specific page, establish lock
hold time, or prove a causal link to a particular INSERT. Directory transitions
identify intervals with publication/retirement, not which writer performed a
fold or merge. Monitor query time is retained because inspection itself takes
locks and can delay sampling. Short stalls may be missed.

This is a closed-loop, growing-corpus workload. A faster writer leaves more rows
for readers to scan, so throughput and latency differences are not isolated
measurements of lock-hold reduction. Monitor/oracle overhead is included. Keep
all settings fixed, alternate build order, retain failed runs, and report each
run's final row count alongside tails before drawing performance conclusions.

Artifacts include effective settings, the harness checkout identity, installed
library hash, SQL, plans, raw transaction logs, wait samples, oracle results,
and summaries. The harness checkout is not mislabeled as the installed binary's
source revision.
