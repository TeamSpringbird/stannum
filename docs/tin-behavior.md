# TIN's behaviour on the cases a review of Stannum found

A review of Stannum on 2026-09-26 (head `ba9c049`) found paths that crash
the server or return wrong results. Where TIN, PlanetScale's engine that
Stannum's query language and parser derive from, can be observed doing the
same thing, its behaviour was measured so Stannum can match it where it is
right and do better where it is not. Measured on 2026-09-26 against a
PlanetScale PostgreSQL 18.6 database with TIN 1.0.3, using
`benchmarks/tin_behavior_probe.py` (scratch schema `stannum_probe`,
dropped afterwards).

| Case | TIN 1.0.3 | Stannum `ba9c049` | Decision |
|---|---|---|---|
| `alpha THEN/1 "beta gamma"`, a span with a phrase operand after the first position | correct: rows 1, 2 | misses row 1 (plain `WHERE` and ranked) | match TIN (`fix/ranked-correctness`) |
| `ORDER BY full_score(ctid, k1 => 0) LIMIT 1`, scores equal to one ulp | correct: the exhaustive top row | returns a row one ulp lower | match TIN (`fix/ranked-correctness`) |
| 10,000-word query, 10,000-term `OR` chain, 5,000 nested parentheses | crashes the server: every backend is restarted | crashes the server (stack overflow in the parser) | do better: a clean ERROR (`fix/query-depth`) |
| user-settable debug settings | `tin.debug_force_*` are user-settable and choose plans | `stannum.debug_seed_score` is user-settable and can change results | a user setting may change the plan, never the result; seeding becomes superuser-only |

## Spans with phrase operands

Documents: 1 "alpha x beta gamma", 2 "alpha beta gamma", 3 "beta gamma
alpha", 4 "alpha x y beta gamma". TIN's matches (and ranked order under
`full_score`), identical with its custom scan on and off:

| Query | Matches | Ranked |
|---|---|---|
| `alpha THEN/1 "beta gamma"` | 1, 2 | 2, 1 |
| `alpha THEN/2 "beta gamma"` | 1, 2, 4 | 2, 1, 4 |
| `"alpha x" THEN/2 "beta gamma"` | 1, 4 | 1, 4 |
| `alpha THEN/1 beta` | 1, 2 | 2, 1 |
| `"beta gamma" THEN/1 alpha` | 3 | 3 |
| `alpha NEAR/1 "beta gamma"` | 1, 2, 3 | 2, 3, 1 |

These agree with tinql's reference evaluator. Stannum's `PhrasePlan`
(`boldi-vigna/src/phrase_plan.rs`, since `71a1cf1`/`d9068b1`) recorded the
pair gaps of a nested ordered span out of order and dropped rows 1 and 4.

## BM25 with k1 = 0

Ten rows: row 1 "w w w", rows 2 to 10 "w". TIN's `full_score(ctid, 0, 0.75)`
is 0.046520013 for row 1 and 0.046520017 for row 2, bit-identical to
Stannum's: with k1 = 0 the score is `fl(fl(m·tf)·1) / fl(tf)`, which rounds
one ulp lower for some higher term frequencies. TIN's pruned `LIMIT 1`
returns row 2, the exhaustive top. At k1 = 0.001, 0.01 and 1.2 row 1 is the
top and TIN returns it. Stannum bounded a candidate by its top frequency
bucket, assuming the score never falls as the bucket rises.

## Query size and nesting

TIN accepted 3,000 plain words, a 3,000-term `OR` chain and 1,000 levels of
nesting. At 10,000 words, 10,000 `OR` terms and 5,000 levels the connection
dropped with no error; an idle second connection dropped with it and the
database's statistics were reset, which is PostgreSQL reinitializing after a
backend crash. Stannum crashes the same way (a 30,000-word query overflows
an 8 MiB stack in the parser). Stannum's limits must accept everything TIN
accepts (at least 3,000 terms and 1,000 levels) and answer anything larger
with an ERROR. The probe's `--crash-probe` repeats this measurement; it
restarts the server it runs against.

## Not measurable against TIN

VACUUM freeing a merge's unpublished pages, the pending list freed before
the meta page is written, and counts racing VACUUM are properties of
Stannum's storage and are fixed and tested locally with race points
(`docs/testing.md`).
