# Wide ranked OR under concurrent traffic

PR #52's grouped pivot optimization is compared with the frozen PR #51 build.
All twelve windows use fresh copies of the same 16,384-document synthetic fixture,
four Docker CPUs and 4 GiB memory. Build order is baseline/candidate/candidate/baseline,
with one reader, four readers, and four readers plus 50 offered UPDATEs/second per build.
Each window lasts 20 seconds and equally weights ranked OR widths 2, 32 and 128.
Preflight EXPLAIN confirms block-max pruning for every measured query.

Values below average the two per-build runs; p95 values are means of per-run p95s,
not pooled percentiles. Full counts, tails, writer lag and validation are in
[the machine-readable results](wide-contention-results.json).

| Readers / offered writes/s | Read QPS before → after | OR128 p95 ms before → after | Completed writes/s before → after |
|---|---|---|---|
| 1 / 0 | 16.6 → 39.7 | 213.1 → 66.5 | — |
| 4 / 0 | 67.6 → 149.0 | 238.6 → 72.9 | — |
| 4 / 50 | 63.6 → 145.7 | 214.9 → 74.4 | 51.2 → 49.8 |

The two-term control is not uniformly faster: with four readers and writes, its
mean per-run p95 increases from 2.18 to 2.72 ms (about 25%). The 32-term case falls
from 25.80 to 18.41 ms. Higher overall throughput does not guarantee lower tails
for every query in the mix; these short runs do not establish a cause for that
control change. The final baseline single-reader OR128 p95 was noisier than its
first repeat; both observations are retained.

## Validation and the initial rejected oracle

All 108 case validations pass: 72 exact pre/post score-multiset and membership checks,
plus 36 live membership/cardinality/finite-score checks. No pgbench transactions fail.
These do not prove exact ranked ordering during writes. The live check adds load
and uses a repeatable-read snapshot; each report retains its duration. Each query
has p99 reported only when that query/window has at least 1,000 samples; most do not.

The initial campaign stopped when separate score statements disagreed during baseline
writes. A [controlled probe](score-snapshot-probe.json) keeps exactly the same 1,024
visible documents and body hash in a repeatable-read transaction, commits updates from
another session, and observes a changed score hash on the same SQL. With writes stopped,
exact ranked checks pass. This is consistent with the documented live, dead-inclusive
index statistics; MVCC row visibility does not freeze those statistics. The harness
now checks exact scores before/after traffic and membership/result validity during it.
The rejected run remains in the raw evidence. Extend the [cursor fuzzer](../testing.md)
to wide terms to validate exact ranking while writes continue against a captured scorer.

## Scope and reproduction

These are closed-loop, local-client timings including transport and connection startup,
not server-only timings and not TIN/GIN comparisons. Results concern a controlled
synthetic mix, not a capacity ceiling or a universal speedup. Random query/write
schedules differ between runs. Windows are short, warm, update-only and do not
cover sustained insert/delete/VACUUM traffic, large real corpora or memory pressure.

Use the [concurrent query-shape mode](query-shapes.md#concurrent-wide-query-diagnostic)
with `--rows 16384 --readers 1|4 --seconds 20 --write-rate 0|50`.
The campaign records immutable image IDs, source hashes, SQL, plans, initial fixture
hashes and all pgbench logs. Raw local archive: `benchmarks/results/wide-contention-evidence.tar.gz`.
Archive SHA-256: `9e0a44109cf289778e4eca4a641ae5068596924455778b8f24753e0339264ee2`.

See the [prioritized follow-up work](priorities.md).
