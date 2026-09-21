# Candidate binary on a restored 100k baseline

The count-strategy candidate passed a physical-snapshot compatibility rehearsal
using 100,000 Wikipedia documents. Main 750872d built the index; candidate
54c2d9e read and modified a restored copy. Both are release builds. This is native
macOS ARM PostgreSQL 18.6, not a restore of the AWS Linux snapshot.

| Phase | OR-count checks | Rows |
|---|---:|---:|
| Baseline binary | 302 | 100,000 |
| Candidate on restored physical files | 604 | 100,000 |
| Candidate after writes and foreground merges | 604 | 100,041 |
| Candidate after VACUUM | 604 | 100,041 |
| Candidate after server restart | 604 | 100,041 |

All 2,718 checks passed against an independent full-corpus whitespace-token OR
oracle for the normalized published trace. Candidate phases exercise default
selection and forced page bitmaps; plans must show the expected custom count
provider and forced bitmap strategy. Counts match before and after substitution.
Initial row count, visibility map and segment metadata match exactly on restore.
Updates exercise indexed and non-indexed columns, deletes remove about 1%, and
1,024 inserts force small write-buffer flushes and tiered merges. The focused
two-session visibility suite also passed 56 checks across retained/fresh snapshots
and vacuum, using nested AND/OR cases and both count strategies.

The harness creates one private cluster and copies both libraries into its output
folder. Only extension-owned C-function bindings in that disposable database are
redirected to the private runtime path. It verifies those bindings and the runtime
library checksum per phase. The server stops before physical copying and binary
replacement; a new backend never mixes both libraries. Global installed extension
files are untouched. The baseline and candidate SQL interfaces are identical;
this harness does not implement extension SQL migrations or authorize arbitrary
binary swaps. A shared pgrx lock serializes local extension-related activity.

Raw plans, counts, library hashes, copied protocol, clean-shutdown control data,
server log, stopped cluster, and pristine physical snapshot are retained under
`benchmarks/results/local-snapshot-candidate-100k-r2/`. The first exploratory pass
also succeeded; r2 adds explicit per-phase library binding/hash checks. Summary:
[receipt](local-snapshot-candidate.json).

Execution times are one-shot diagnostic EXPLAIN samples on a shared development
machine, not a paired capacity benchmark or evidence of an AWS speedup. This
validates only the tested binary pair, dataset and workload; ranked, fuzzy and
phrase compatibility and additional PRs still need their own checks.
