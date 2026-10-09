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

Checked against TIN 1.0.3's and TIN 1.0.4's recorded answers (258 cases;
SKIP is a case without a recorded answer):

| Status | TIN 1.0.3 | TIN 1.0.4 | Meaning |
| --- | ---: | ---: | --- |
| PASS | 183 | 217 | Every capture equals TIN's answer |
| DIFF | 12 | 12 | Both raise an ERROR with the same SQLSTATE; the message wording differs |
| IMPROVED | 5 | 5 | TIN refuses the query with an ERROR; Stannum answers it |
| GAP | 9 | 13 | Stannum lacks what the case exercises |
| LIMITED | 3 | 2 | Stannum refuses with a limit ERROR where TIN answers |
| NEWER | 4 | 4 | Stannum follows a later TIN change that Lead has copied |
| SKIP | 42 | 5 | |
| FAIL | 0 | 0 | |

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
  boosts (`a a` scores as `a^2`), and so does a word written more than once
  in a phrase or span (`"to be or not to be"`); a boost on an operand inside
  a span weighs that operand alone (`ipa^3 NEAR/5 hoppy`). `score()` and `full_score()` over `==>`
  clauses on several indexed columns of one table sum one score per column,
  in clause order.
- **Which clauses score.** A row is scored by the `==>` clauses on its own
  relation in the WHERE clause, a flattened subquery or CTE, or an inner
  join's ON clause, whose search text may come from the joined row. Clauses
  under `NOT` and clauses on other relations contribute nothing. Several
  clauses on one column score as one ORed query whatever their search texts
  are (constants or parameters, under custom or generic plans); each text is
  parsed on its own, so a text `==>` rejects raises `==>`'s error, and an
  empty text adds nothing. A row that no search admits (the `id = 4` of
  `body ==> 'gems' OR id = 4`) scores NULL. A partial index scores only when
  the WHERE clause implies its predicate; with no index to score a relation's
  searches, `score()`, `full_score()` and `max_score()` raise TIN's
  `cannot compute scores for this query` (SQLSTATE 0A000) with a `No
  matching stannum index for: ...` detail.
- **Highlighting.** `highlight()` and `highlight_ansi()` without a query take
  it from a `==>` clause anywhere in the query's join tree, including a CTE or
  subquery the planner flattens, but not under `NOT`; with no clause to bind
  they return the text unmarked. Several clauses mark every term of each,
  parameters included, and their texts are parsed one by one as for scoring.
- **Index options.** All sixteen documented options are accepted with TIN's
  domains, `stemmer` (TIN 1.0.4) included; see [index options](#index-options)
  and [stemming](#stemming).
- **Errors.** An invalid query raises `invalid ==> query at byte N in
  "QUERY": ...` with TIN's SQLSTATE, except a query past a size limit
  (below). `target_segment_count`,
  `max_mutable_segment_size` and `max_merged_segment_size` values outside
  TIN's domains are rejected with SQLSTATE 22023.
- **Query size.** Stannum answers every query size TIN answers (3,000 words,
  a 3,000-term `OR` chain, 1,000 nested parentheses) and more (up to 10,000
  terms). Where TIN 1.0.3 crashes the server (10,000 terms, 5,000 nesting
  levels), Stannum raises an ERROR: a query is limited to 1,000 nesting levels
  and 10,000 terms, and every recursive pass checks PostgreSQL's stack depth.
  `AT LEAST` inside a proximity operator is limited to 10,000 combinations.
  These errors carry SQLSTATE 54001 (`statement_too_complex`), not TIN's
  XX000.

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
| `catalog.S-07` | `stannum.promote()` folds a write buffer of any size, so after it a term in every row is elided by `score()`; TIN's `promote()` consumes only a sealed write segment and leaves a 20-row buffer mutable, where elision does not count it. |

## Other known differences

- **Negated span relations** (`catalog.S-11`). The right side of `NOT
  ENCLOSES`, `NOT ENCLOSED BY` and `NOT OVERLAPPING` adds no scoring term,
  as with `AND NOT`. TIN 1.0.3 and 1.0.4 score it; Lead b8018ac copies TIN's
  later fix, which Stannum follows (a `newer` divergence).
- **NEAR with a shared word** (`span.minimal_interval.4` to `.6`). Each
  operand of NEAR takes its own occurrence, so `"a b" NEAR/2 b` matches
  "a b b c d" through the second `b`. TIN 1.0.3 and 1.0.4 match none of
  these rows; Lead b8018ac copies TIN's later boldi-vigna fix, and Lead
  e3ed2f4 answers as Stannum does (a `newer` divergence).
- **A changed stemmer** takes effect at `REINDEX`, like every tokenizer
  option in Stannum: queries and stored terms are always analyzed alike.
  TIN 1.0.4 stems query terms as soon as `ALTER INDEX ... SET (stemmer)`
  runs and stored terms only after `REINDEX`, so between the two its
  inflected queries miss rows (`stemming.ddl.alter_reindex`).
- **Error wording.** Syntax errors name what was expected in the words of
  Stannum's recursive-descent parser, not TIN's grammar rules; the SQLSTATE
  matches. These are the DIFF cases.
- **`max_score()`** over several indexed columns reports the first column's
  best score, while `score()` sums the columns. TIN (and Lead) report the
  highest summed score among the rows that match every column the WHERE
  clause requires.
- **Index choice.** Where several stannum indexes cover a scored column,
  scoring uses the one `==>` binds to: the oldest by OID whose predicate the
  WHERE clause implies. Lead prefers the newest qualifying partial index,
  then the newest full one. Only the WHERE clause proves a partial index's
  predicate, and an outer join's ON clause binds no scoring, where Lead also
  uses the ON clause on an outer join's nullable side.
- **Case folding** lowercases Unicode scalar values rather than applying full
  Unicode case folding (`ß` stays `ß`). TIN's documentation does not specify
  its algorithm; accent folding and word boundaries are likewise unspecified
  there.
- **Maintenance** runs in a background worker only when the library is
  preloaded (`shared_preload_libraries = 'stannum'`); otherwise inserting
  backends and VACUUM do it, as when the worker pool is exhausted. Workers
  apply the server's settings and the index's options, not an inserting
  session's `SET`s. VACUUM keeps doing its own merges and rewrites. See
  [maintenance workers](architecture/maintenance-workers.md).
  `initial_segment_count` is accepted and ignored with a warning.
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
| `score_stop_words` | comma-separated text; unset | Analyzed terms omitted from default scoring | Same; matching unchanged, full scoring ignores the list; on a stemmed index, written as stems |
| `stemmer` | `ar`, `da`, `de`, `el`, `en`, `es`, `fi`, `fr`, `hu`, `it`, `nl`, `no`, `pt`, `ro`, `ru`, `sv`, `ta`, `tr`; unset | Snowball stemming of indexed and query terms | Same; persisted at build with the other tokenizer settings |
| `initial_segment_count` | integer `1..4096` | Build partitions | Accepted and ignored, with a warning |
| `target_segment_count` | integer `1..4096` | Maintenance target | Soft directory bound in place of `stannum.max_segments` |
| `max_mutable_segment_size` | integer `>= 131072` bytes; `4194304` | Write segment size before it is sealed and promoted | The size at which the write buffer is sealed, in place of `stannum.write_buffer_bytes` (also 4 MiB); `stannum.write_buffer_docs` (12,288) still applies |
| `max_merged_segment_size` | integer `>= 100` MB; `2000` | Merge size ceiling | Most input megabytes one merge takes, within the 3 GiB a run can record |
| `dead_percent_threshold` | real `0..1`; `0.5` | Dead fraction that triggers a rewrite | VACUUM rewrites a segment at this dead fraction |

The storage options are reloptions read when maintenance runs, so
`ALTER INDEX ... SET` takes effect without a rebuild; unset, the
[storage settings](architecture/segmented-storage.md#writing-an-index) apply.
None of them enforces a memory limit or starts workers. Tokenizer options
change stored terms and need `REINDEX`, as TIN's reference states. TIN
documents no language selection beyond the stemmer and no indexing-time stop
words, and Stannum adds none. TIN's server and session
[settings](https://planetscale.com/docs/postgres/search/reference/settings) are
not index options and are not accepted as reloptions.

## Stemming

`WITH (stemmer = '<code>')` stems every analyzed term with the Snowball
stemmer of that language (rust-stemmers 1.2.0, as Lead), at index time and
at query time: `run`, `runs` and `running` are stored and searched as `run`,
while `runner` and `ran` stay apart. The pipeline lowercases, normalizes to
NFC, stems, and then folds accents if `accent_folding = fold`, so `résumés`
stems to `résumé` and folds to `resume`. Phrases, proximity and positional
filters work on stems; positions are the unstemmed text's. Wildcard, fuzzy,
regular-expression and range literals are not stemmed and match the stored
stems as written (`runn*` matches `runner`, not `running`). Scores are
BM25 over stems; `score_inspect` lists stems, and `score_stop_words` is
compared with stems. Highlighting marks the source words whose stems
match.

An unknown code raises XX000 `unknown stemmer language code: <code>` (the
code is case-sensitive and must be one of the eighteen), and a stemmer with
`case_folding = preserve` raises XX000 `stemming requires case_folding =
fold`, at `CREATE INDEX` and `ALTER INDEX` alike, as TIN 1.0.4 does.
`stannum.tokenize` and `ql_parse` take a trailing `stemmer` argument (NULL
stems nothing). `highlight` and `highlight_ansi` take TIN 1.0.4's
`tokenizer` through `stemmer` arguments after `query`: a call that sets none
of them analyzes with the bound index's settings, as before, and one that
sets any analyzes with exactly those settings, each unset one at its
default. `==> ANY(...)` and `ALL(...)` bind to the index's settings too.

The stemmer is stored with the tokenizer settings in the index's meta page,
in bits that indexes built before stemming hold as zero, so those indexes
read as unstemmed and the on-disk format is unchanged.

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

- Score bits are not compared. Lead before 615e9ce counted a document with
  no tokens at index build time in its corpus size, which shifted every IDF
  in the last bits; TIN and Stannum do not, nor does Lead since.
- Expansion shapes (wildcards, regular expressions, ranges) compare match
  sets and highlights only, because Lead scores their matches as zero where
  TIN scores the expanded terms.

A highlight difference can be excused only with an explicit reason in
`REFERENCE_UNHIGHLIGHTED`, and score exclusions never suppress highlight
checks. The same oracle runs against PlanetScale TIN by hand
(`benchmarks/oracle.py --right-engine tin`, with the connection in a libpq
environment file outside the repository); there score bits, including
`max_score`, are compared exactly.
