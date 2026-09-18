# Dead-list estimates and conjunction bounds

Measured 2026-09-18, PostgreSQL 18 on local pgrx port 28818, Wikipedia 100,000
rows from `LeadBenchmarks/datasets/wikipedia-100000/documents.csv`. Baseline:
`45d2696`. Each query selects ID, orders by `stannum.full_score(ctid) DESC`,
LIMIT 10, with `enable_seqscan=off`. The table has a primary key and a Stannum
body index and was VACUUM ANALYZEd. Each entry below is a warm median of seven
EXPLAIN ANALYZE execution times in one backend after one discarded warm-up.
Two baseline/changed pairs ran under a single PostgreSQL lock, alternating
release libraries against the same database. Library sizes/timestamps were
checked around every timing run. The temporary database was dropped.

| Query | Baseline ms, runs 1 / 2 | Changed ms, runs 1 / 2 | Scored candidates, both |
| --- | ---: | ---: | ---: |
| `war AND history` | 1.427 / 1.273 | 1.324 / 1.301 | 510 |
| `united AND states` | 0.856 / 0.806 | 0.743 / 0.813 | 215 |
| `history AND war AND world` | 1.780 / 1.928 | 1.601 / 1.706 | 290 |
| `telescope AND astronomy` | 0.502 / 0.436 | 0.445 / 0.413 | 45 |
| `war` | 0.761 / 0.742 | 0.690 / 0.735 | 2,834 |
| `telescope` | 0.422 / 0.415 | 0.371 / 0.403 | 148 |
| `war OR history` | 1.862 / 1.866 | 1.954 / 1.835 | 555 |

The strongest consistent improvement is the three-term conjunction. Controls
also vary, so these small local times do not establish a broad speedup.
Scored-candidate counts do not change: the optimization avoids repeated bound
work and may reject candidates before their document length is fetched; the
existing per-document bounds already reject the same eventual non-winners.

The retained implementation computes a shared lower bound on document length
(the maximum shortest length of the conjunction's current posting blocks),
then bounds each bucket at the greater of that length and its own minimum.
This is never looser than the independent sum. The sum follows scoring term
order, preserving floating-point upper-bound and tie behavior. The interval
bound is cached through the earliest block end; when it cannot compete, only
the rarest cursor must seek past that interval before intersection resumes.

An initial version computed fresh block bounds before every intersection. Its
first `war AND history` measurement regressed from 1.359 to 1.889 ms, with
unchanged scored count. It was replaced by intersection-triggered computation
and interval reuse. Block size remains 128; the on-disk bounds layout and LSG2
signature are unchanged. Smaller/two-level blocks were not justified by this
evidence and were not implemented.

## Estimates and visibility limits

Statistics now remove each source's known dead count from N. Terms with at most
1,024 postings count dead intersections exactly, which handles selective
correlated deletion; larger terms use the segment live fraction. Expansions
use the same per-term calculation. Buffer counts are unchanged. Unit tests
cover selective and common terms, multiple sources, buffers, expansions/caps,
and all-dead/empty populations.

The requested before-first-VACUUM guarantee cannot be obtained from dead lists:
only the bulk-delete callback populates them, and DELETE does not update the
index. No heap scans were added at plan time. The pg_test instead separates
VACUUM's bulk-delete and cleanup callbacks, checks half the rare matches dead
with the original segment still present (factor 1.5), then checks cleanup and
a forced rewrite. A real SQL VACUUM cannot run inside a pg_test transaction.
Common-term scaling remains an independence approximation, and stale PostgreSQL
relation statistics can still affect the final row estimate; ANALYZE refreshes
that separate denominator.

Validation includes core workspace tests, PG18 pg_tests (including extended
bit-for-bit top-k comparisons for four more AND shapes and limits
127/128/129/255/256/257), and PG18/PG17 clippy with pg_test and warnings denied.
