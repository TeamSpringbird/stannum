# Local release-build count and visibility experiment

100,000 documents from the existing Wikipedia prefix, PostgreSQL 18.6 on the local ARM Mac. Nine alternating repetitions per mode and query; server-side EXPLAIN ANALYZE timings. Release artifact hash matched the installed extension before measurement.

These are single-client local diagnostics on a shared machine, not throughput or a TIN comparison. Background OS CPU activity was observed. Each measured query uses a fresh backend and a prepared custom plan. No format or automatic strategy changes were made.

The widest query improved 1.87–1.90x in the two vacuumed phases, 1.17x after
mutations and 1.74x after another VACUUM. Default and bitmap paths both fetched
83,122 heap tuples for that query after mutations; both returned to zero after
VACUUM. Sparse `ford parts` was slower with forced bitmaps in every phase.
This supports selective strategy work, not a blanket switch. Small timings vary
on the shared host; for example the fastest query's baseline median moved from
0.081 to 0.062 ms between vacuumed repeats. No statistical significance is claimed.

| Phase | Query | Default ms | Bitmap ms | Default / bitmap | Default heap fetches | Bitmap heap fetches |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| vacuumed | 302:disjunction | 6.666 | 3.556 | 1.87x | 0 | 0 |
| vacuumed | 88:disjunction | 2.359 | 1.527 | 1.54x | 0 | 0 |
| vacuumed | 301:disjunction | 1.734 | 1.393 | 1.24x | 0 | 0 |
| vacuumed | 222:disjunction | 1.359 | 1.022 | 1.33x | 0 | 0 |
| vacuumed | 197:disjunction | 0.134 | 0.176 | 0.76x | 0 | 0 |
| vacuumed | 251:disjunction | 0.081 | 0.082 | 0.99x | 0 | 0 |
| vacuumed-repeat | 302:disjunction | 6.622 | 3.479 | 1.90x | 0 | 0 |
| vacuumed-repeat | 88:disjunction | 2.348 | 1.508 | 1.56x | 0 | 0 |
| vacuumed-repeat | 301:disjunction | 1.535 | 1.292 | 1.19x | 0 | 0 |
| vacuumed-repeat | 222:disjunction | 1.178 | 0.874 | 1.35x | 0 | 0 |
| vacuumed-repeat | 197:disjunction | 0.112 | 0.150 | 0.75x | 0 | 0 |
| vacuumed-repeat | 251:disjunction | 0.062 | 0.061 | 1.02x | 0 | 0 |
| mutated | 302:disjunction | 22.993 | 19.706 | 1.17x | 83122 | 83122 |
| mutated | 88:disjunction | 15.549 | 13.838 | 1.12x | 73858 | 73858 |
| mutated | 301:disjunction | 11.698 | 11.363 | 1.03x | 53230 | 53230 |
| mutated | 222:disjunction | 13.004 | 11.604 | 1.12x | 66520 | 66520 |
| mutated | 197:disjunction | 1.794 | 1.939 | 0.93x | 1876 | 1876 |
| mutated | 251:disjunction | 0.150 | 0.155 | 0.97x | 35 | 35 |
| revacuumed | 302:disjunction | 9.192 | 5.296 | 1.74x | 0 | 0 |
| revacuumed | 88:disjunction | 4.141 | 2.754 | 1.50x | 0 | 0 |
| revacuumed | 301:disjunction | 2.744 | 2.009 | 1.37x | 0 | 0 |
| revacuumed | 222:disjunction | 2.349 | 1.471 | 1.60x | 0 | 0 |
| revacuumed | 197:disjunction | 0.190 | 0.280 | 0.68x | 0 | 0 |
| revacuumed | 251:disjunction | 0.106 | 0.102 | 1.04x | 0 | 0 |

A ratio above 1 favors bitmaps; below 1 favors default counting. Full count agreement and independent lexical membership checks on 1,000 reference rows passed in each phase.

Mutations update payloads for 5% of original rows, append whitespace to indexed text for 10%, and delete every 101st row (990 rows). Autovacuum is disabled so phases stay controlled. These mutations change both visibility and segment/dead-posting state; the experiment does not isolate visibility cost alone. After VACUUM, surviving data and index layout differ from the initial baseline.

The first diagnostic pass accidentally used the previously installed debug library. It is retained under local-count-visibility-100k-r2 with an explicit performance-unusable caveat and is excluded here. The runner now requires a matching release-artifact hash.

Raw plans, counts, settings, visibility snapshots and logs are retained in benchmarks/results/local-count-visibility-100k-release/. The temporary PostgreSQL cluster was stopped and removed. Compact measurements and medians/ranges are in local-count-visibility.json.

Reproduce using benchmarks/count_local.py --input HEADERLESS_WIKIPEDIA_CSV --release-library RELEASE_DYLIB --output NEW_DIRECTORY. PostgreSQL binaries must be in PATH. The runner owns a temporary cluster and holds the shared installation lock.
