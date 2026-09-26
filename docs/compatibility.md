# Compatibility with TIN

Stannum implements the SQL interface of PlanetScale TIN: the `==>` operator,
the TINQL query language, the index access method with TIN's index options,
and the scoring and highlighting functions. The reference is PlanetScale TIN
1.0.3 on PostgreSQL 18.6, whose answers are recorded by the
[conformance suite](../conformance/README.md), and PlanetScale's open-source
Lead, which CI runs beside Stannum on every push.

This page lists what matches, where Stannum knowingly differs, and how that is
checked. The [TIN behavior catalog](tin-behavior-catalog.md) lists every
documented and measured TIN behavior with its source.

## Names and installation

Stannum's extension, library, access method and SQL schema are `stannum`,
where TIN's and Lead's are `tin`: `tin.score` is `stannum.score`, and TIN's
settings are `stannum.*` settings. The operator is `==>` in both. Because both
extensions define `==>` in `pg_catalog`, Stannum and TIN (or Lead) need
separate databases. There is no in-place migration: create a fresh database,
load the data, and build indexes with `USING stannum`.

## Conformance summary

Checked against TIN 1.0.3's recorded answers:

| Status | Cases | Meaning |
| --- | ---: | --- |
| PASS | 170 | Every capture equals TIN's answer |
| DIFF | 12 | Both raise an ERROR with the same SQLSTATE; the message wording differs |
| IMPROVED | 5 | TIN refuses the query with an ERROR; Stannum answers it |
| GAP | 2 | Stannum lacks what the case exercises |
| FAIL | 0 | |

Improvements and gaps are declared in
[`conformance/divergences/stannum.yaml`](../conformance/divergences/stannum.yaml).
A declared case fails if anything other than the listed captures differs, so a
declaration cannot hide a regression.

## What matches TIN 1.0.3

- **Query language.** Terms, Boolean operators, phrases with slop and gaps,
  alternatives, `AT LEAST` and `ALL OF`, proximity, span relations, positional
  filters, expansions (wildcards, regular expressions, ranges, fuzzy terms)
  and boosts, as described in the [query language guide](query-language/introduction.md).
- **Scoring.** BM25 scores are bit-identical to TIN's, including at `k1 = 0`,
  and the pruned top k is the exhaustive top k. A term repeated in a flat `AND` or `OR` chain adds its
  boosts (`a a` scores as `a^2`). `score()` and `full_score()` over `==>`
  clauses on several indexed columns of one table sum one score per column,
  in clause order.
- **Highlighting.** `highlight()` and `highlight_ansi()` without a query take
  it from a `==>` clause anywhere in the query's join tree, including a CTE or
  subquery the planner flattens; with no clause to bind they return the text
  unmarked.
- **Index options.** All fifteen documented options are accepted with TIN's
  domains; see [index options](#index-options).
- **Errors.** An invalid query raises `invalid ==> query at byte N in
  "QUERY": ...` with TIN's SQLSTATE. `target_segment_count`,
  `max_mutable_segment_size` and `max_merged_segment_size` values outside
  TIN's domains are rejected with SQLSTATE 22023.
- **Query size.** Stannum answers every query size TIN answers (3,000 words,
  a 3,000-term `OR` chain, 1,000 nested parentheses) and more (up to 10,000
  terms). Where TIN 1.0.3 crashes the server (10,000 terms, 5,000 nesting
  levels), Stannum raises an ERROR: a query is limited to 1,000 nesting levels
  and 10,000 terms, and every recursive pass checks PostgreSQL's stack depth.

## Documented improvements

TIN 1.0.3 refuses these with an ERROR; Stannum answers them.

| Case | TIN 1.0.3 | Stannum |
| --- | --- | --- |
| `catalog.S-14` | Refuses differing `dense_ratio` arguments and a row-dependent `k1` | Scores each call with its own arguments |
| `catalog.S-15` | Refuses `score()` and `full_score()` on one relation | Returns both |
| `catalog.S-22` | Refuses to score with its custom scan disabled | Returns the same scores with `stannum.enable_custom_scan` on or off |
| `catalog.K-12` | Refuses a query on a non-default-tokenization index without its custom scan | Answers with the index's tokenizer either way |
| `catalog.H-14` | Refuses an implicit highlight on a non-default-tokenization index | Highlights with the index's tokenizer |

Stannum binds matching and highlighting to the index's persisted tokenizer
settings, including sequential-scan and bitmap rechecks, which is what makes
the last two possible.

## Documented gaps

| Case | Difference |
| --- | --- |
| `catalog.I-07` | Stannum runs no background maintenance workers and has no `maintenance_jobs_per_db` setting, so a session `SET` of it is not refused. |
| `catalog.S-07` | Stannum has no `promote()` function. Inserts fold the write buffer into segments and VACUUM merges them. |

## Other known differences

- **Error wording.** Syntax errors name what was expected in the words of
  Stannum's recursive-descent parser, not TIN's grammar rules; the SQLSTATE
  matches. These are the DIFF cases.
- **`max_score()`** over several indexed columns reports the first column's
  best score, while `score()` sums the columns.
- **Case folding** lowercases Unicode scalar values rather than applying full
  Unicode case folding (`ß` stays `ß`). TIN's documentation does not specify
  its algorithm; accent folding and word boundaries are likewise unspecified
  there.
- **Maintenance** runs in inserting backends and VACUUM rather than
  background workers, so `initial_segment_count` is accepted and ignored with
  a warning.
- **Unbound `==>`.** Where no query is planned around the operator (a
  partial-index predicate, a CHECK constraint, a generated column) or the
  document is not an indexed column, `==>` uses the default tokenizer
  settings. See the storage guide's
  [current limits](architecture/segmented-storage.md#current-limits).

## Index options

All options on PlanetScale's
[index reference](https://planetscale.com/docs/postgres/search/reference/indexes)
are accepted. Stannum's registration is in
[`options.rs`](../postgres/src/options.rs); tokenization is represented by
[`TokenizerPipelineSpec`](../tokenizer/src/spec.rs).

| Option | TIN values; default | Meaning | Stannum |
| --- | --- | --- | --- |
| `tokenizer` | `unicode`, `whitespace`; `unicode` | Word boundaries or whitespace fields | Same |
| `case_folding` | `fold`, `preserve`; `fold` | Case-insensitive or original-case terms | Same; lowercases Unicode scalars |
| `accent_folding` | `fold`, `preserve`; `fold` | Remove or retain accents | Same; canonical decomposition, combining marks removed, recomposed |
| `long_tokens` | `split`, `truncate`, `discard`; `split` | Terms beyond the byte ceiling after folding | Same; chunks prefer grapheme boundaries |
| `max_token_bytes` | integer `4..2692`; `256` | UTF-8 analyzed-term byte ceiling | Same |
| `graphemes` | `emoji`, `retain`, `discard`; `emoji` | Standalone emoji and symbol clusters | Same |
| `position_gaps` | `preserve`, `collapse`; `preserve` | Positions consumed by removed tokens | Same; discarded long tokens leave gaps only in preserve mode |
| `k1` | real `0..10000`; `1.2` | BM25 saturation | Same; query-time, no rebuild |
| `b` | real `0..1`; `0.75` | BM25 length normalization | Same; query-time, no rebuild |
| `score_stop_words` | comma-separated text; unset | Analyzed terms omitted from default scoring | Same; matching unchanged, full scoring ignores the list |
| `initial_segment_count` | integer `1..4096` | Build partitions | Accepted and ignored, with a warning |
| `target_segment_count` | integer `1..4096` | Maintenance target | Soft directory bound in place of `stannum.max_segments` |
| `max_mutable_segment_size` | integer `>= 131072` bytes; `4194304` | Write buffer size before promotion | Fold size in place of `stannum.write_buffer_bytes`; `stannum.write_buffer_docs` still applies |
| `max_merged_segment_size` | integer `>= 100` MB; `2000` | Merge size ceiling | Most input megabytes one merge takes, within the 3 GiB a run can record |
| `dead_percent_threshold` | real `0..1`; `0.5` | Dead fraction that triggers a rewrite | VACUUM rewrites a segment at this dead fraction |

The storage options are reloptions read when maintenance runs, so
`ALTER INDEX ... SET` takes effect without a rebuild; unset, the
[storage settings](architecture/segmented-storage.md#writing-an-index) apply.
None of them enforces a memory limit or starts workers. Tokenizer options
change stored terms and need `REINDEX`, as TIN's reference states. TIN
documents no stemming, language selection or indexing-time stop words, and
Stannum adds none. TIN's server and session
[settings](https://planetscale.com/docs/postgres/search/reference/settings) are
not index options and are not accepted as reloptions.

## Reference oracle against Lead

`script/reference-oracle`, run in CI as "Reference oracle against upstream
Lead", checks out PlanetScale's Lead (`main` unless `LEAD_REF` pins a
revision), builds it under its own extension name `tin` into the same server
as Stannum, and runs `benchmarks/oracle.py` on both. The oracle covers 47
TINQL shapes (terms, Boolean forms, phrases, gaps, slop, proximity, span
relations, positional filters, expansions, `AT LEAST`, boosts, and
tokenizer-sensitive accents, numerics, apostrophes, hyphens, URL hosts and
emoji) across five mutation states: after the build, after deletes, after
VACUUM, after inserts into the write buffer, and after `REINDEX`.

Match sets, the rank order of full and dense scores, and the exact HTML and
ANSI highlighted strings must agree, highlights through the one-argument
functions so implicit binding is exercised. Two known deviations of Lead are
allowed for:

- Score bits are not compared. Lead counts a document with no tokens at index
  build time in its corpus size, which shifts every IDF in the last bits; TIN
  and Stannum do not.
- Expansion shapes (wildcards, regular expressions, ranges) compare match
  sets and highlights only, because Lead scores their matches as zero where
  TIN scores the expanded terms.

A highlight difference can be excused only with an explicit reason in
`REFERENCE_UNHIGHLIGHTED`, and score exclusions never suppress highlight
checks. The same oracle runs against PlanetScale TIN by hand
(`benchmarks/oracle.py --right-engine tin`, with the connection in a libpq
environment file outside the repository); there score bits, including
`max_score`, are compared exactly.
