# TIN 1.0.3: observed behavior compared with the public documentation

A report for PlanetScale's TIN team from the Stannum project, 2026-09-26.

## 1. Summary

**Who and what.** The Stannum project measured PlanetScale TIN, extension
`tin` 1.0.3, on a PlanetScale Postgres 18.6 database in us-east-1 on
2026-09-26.

**How.** We wrote a conformance suite of 189 cases. It is open source and
engine-agnostic. The cases come from PlanetScale's public documentation, the
TINQL book in the public Lead repository, and our earlier measurements. We ran
the suite live against TIN and recorded every answer: ids, counts, rankings,
float4 score bits, highlight text, and error SQLSTATEs and messages. Every
recorded file carries a `source` header naming the engine, the extension
version, the server version, the host region, the time and the suite commit.
The recording ran at 2026-09-26 15:18 ET from suite commit `64b7f8e`. The
three query-size cases that crash the server were not run again; their answers
were imported from an earlier probe of the same server that day (§2). We
recorded answers only. We did not change any value afterwards.

**Reading this report.** Each claim cites a case id, such as `catalog.F-02`,
and TIN's recorded answer, or a documentation page. Where the recorded answers
say nothing, we ask a question instead of stating a behavior. Our reading of
the documentation may be what is wrong in places, and we say so where it
applies. Scores appear as float4 bit patterns (`encode(float4send(x),'hex')`),
with the decimal value in parentheses where it helps.

| Area | Finding | Severity |
|---|---|---|
| Query size | A 10,000-word query, a 10,000-term `OR` chain or 5,000 levels of parentheses crashes the backend, and PostgreSQL reinitializes (§2) | High: denial of service |
| Positional filters | `IN WORDS X TO Y` counts from 1. The docs say positions are 0-based (§3.1) | Medium: silent wrong results |
| Phrases and spans | `"hotel _ hotel"` misses a document the literal reading matches (§3.7). `"a b" NEAR/2 b` matches nothing where `THEN/2` matches three documents (§4.2) | Medium: silent missing matches |
| Scoring | A term in exactly 10% of documents is not elided (§3.8). A term in every row of a 20-row table scores above zero under `tin.score` (§3.9) | Low |
| Plan dependence | Scoring fails with `tin.enable_custom_scan = off` (§4.1). So do queries on an index with non-default tokenization (§3.12) | Low |
| Syntax versus docs | `*` inside a span is rejected, but the docs use it in an example. Commas work inside `[...]`. `wi-fi~1` does not error. `IN`/`AT`/`ALL` are reserved outside their context. Relations do not chain (§3.2 to §3.6) | Low |
| Highlighting | `ENCLOSED BY` marks the outer span (§3.11). The BEFORE prose disagrees with its own example, and TIN follows the example (§3.10) | Low |
| Errors | Query syntax errors, invalid regexes and bad function arguments all raise SQLSTATE `XX000` (internal_error) (§5) | Medium for client error handling |
| Surface | Functions such as `segment_info`, `promote`, `merge`, `fsm_rebuild` and `ql_parse` are present but not documented (§6) | Informational |

## 2. Server crash on large queries (highest severity)

**We are sharing this privately. We suggest tracking it in a non-public
channel until a fix ships.**

### Observed

Case ids are `query_size.*`. The answers come from `benchmarks/tin_behavior_probe.py`
at `29a520e` and an equivalent ad-hoc probe for the nesting cases. Both ran
against the same TIN 1.0.3 server on 2026-09-26 and were imported into the
suite unchanged. The corpus has 4 rows, and the term `a` appears in none of
them, so every correct count is 0.

| Query shape | Answered (count 0) | Crashed the server |
|---|---|---|
| `repeat('a ', N)`: N plain words, implicit AND | N = 1,000 and 3,000 (`query_size.words.1000`, `.3000`) | N = 10,000 (`query_size.words.10000`) |
| `a OR a OR ...` with N terms | N = 1,000 and 3,000 (`query_size.or_chain.1000`, `.3000`) | N = 10,000 (`query_size.or_chain.10000`) |
| `a` inside N levels of parentheses | N = 100 and 1,000 (`query_size.nested.100`, `.1000`) | N = 5,000 (`query_size.nested.5000`) |

`catalog.Q-20` confirms the accepted sizes against a document of 3,000 distinct
words. A 3,000-word AND, a 3,000-term OR and 1,000 levels of nesting each
returned `[1]`.

For each crash, the recorded detail reads: *"the connection dropped with no
error, an idle second connection dropped too and statistics were reset
(PostgreSQL reinitialized after a backend crash)"*. The crash therefore
affected every session on the server, not just the one that sent the query.

### Reproduction

**Run this only on a disposable server. Every statement below restarts all
backends.**

```sql
CREATE EXTENSION IF NOT EXISTS tin;
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'alpha x beta gamma'), (2, 'alpha beta gamma'),
                     (3, 'beta gamma alpha'), (4, 'alpha x y beta gamma');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;

-- Accepted: returns 0
SELECT count(*) FROM t WHERE body ==> repeat('a ', 3000);
SELECT count(*) FROM t WHERE body ==> array_to_string(array_fill('a'::text, ARRAY[3000]), ' OR ');
SELECT count(*) FROM t WHERE body ==> (repeat('(', 1000) || 'a' || repeat(')', 1000));

-- Each of these crashed the backend; PostgreSQL reinitialized
SELECT count(*) FROM t WHERE body ==> repeat('a ', 10000);
SELECT count(*) FROM t WHERE body ==> array_to_string(array_fill('a'::text, ARRAY[10000]), ' OR ');
SELECT count(*) FROM t WHERE body ==> (repeat('(', 5000) || 'a' || repeat(')', 5000));
```

The parentheses around the whole right-hand side of the last query matter.
`==>` and `||` are both user-defined operators, so they have the same
precedence and group from the left. Without the parentheses, PostgreSQL
parses `(body ==> repeat(...)) || ...` and raises a type error before TIN
sees the query.

### Likely cause

This is a hypothesis; we have not seen TIN's code. TIN's parse errors name the
same grammar rules as the public Lead grammar
(`tinql/src/parser/pest_parser/grammar.pest`), for example `kw_TO`,
`fuzzy_suffix`, `and_sep`, `rel_op` and `kw_LEAST` (see `catalog.Q-06`,
`catalog.Q-11` and `catalog.Q-15` in §5). That suggests a parser derived from
Lead's:

- In that grammar, nesting is recursive: `grouped = { "(" ~ or_expr ~ ")" }`.
  Each level of parentheses costs several stack frames.
- Flat chains parse iteratively, but they are then folded into left-deep binary
  trees. Recursive lowering, evaluation or `Drop` of those trees goes one frame
  deeper per term.

A Rust stack overflow in a backend process is a SIGSEGV, and the postmaster
answers it by restarting every backend. Stannum derives its parser from Lead's
and crashed in the same way before it added limits. A 30,000-word query
overflowed an 8 MiB stack inside the parser (`docs/tin-behavior.md`).

### Impact

Any role that can run a TIN search can restart the whole server with one
statement, and every other session is disconnected. `catalog.I-06` shows
`==>` evaluating as a plain filter on a table with no TIN index. It may
therefore be reachable without any TIN index at all, but we have not tested
that (question 1).

### Suggested mitigations

- Parse flat `AND`/`OR`/implicit-AND chains into n-ary nodes rather than
  left-deep binary trees, and walk the trees iteratively. Watch recursive
  `Drop` as well.
- Enforce explicit limits on query bytes, term count and nesting depth. Raise
  a clean `ERROR` when a limit is exceeded, for example SQLSTATE `54001`
  (statement_too_complex) or `54000` (program_limit_exceeded). Any limit
  should still accept the sizes TIN 1.0.3 answers today (3,000 terms and
  1,000 levels).
- Call PostgreSQL's `check_stack_depth()` at the recursion points in parsing,
  lowering and evaluation, so that `max_stack_depth` applies. Alternatively,
  grow the stack explicitly.

## 3. Documentation versus behavior

All reproductions assume `CREATE EXTENSION tin` has been run. They use
`SET enable_seqscan = off`, as the suite does, so the planner uses the TIN
index on tiny tables. Each one starts with `DROP TABLE IF EXISTS t`, so they
can run one after another in a single session.

This table maps the 14 conflicts in our behavior catalog (§8 there) to the
subsections below:

| Catalog §8 conflict | Where | TIN 1.0.3 follows |
|---|---|---|
| 1 `IN WORDS X TO Y` numbering | 3.1 | Lead's TINQL book (1-based), not the product docs |
| 2 Commas in `[...]` | 3.2 | Commas act as separators |
| 3 Fuzzy on a hyphenated term | 3.3 | No error; builds a phrase |
| 4 Scores across columns | 3.16 | Product docs (summed) |
| 5 BEFORE highlighting | 3.10 | The docs' example, not the docs' prose |
| 6 Exact 10% dense boundary | 3.8 | Keeps a term at exactly 10% |
| 7 Expansion and NOT scoring | 3.15 | Docs are silent |
| 8 Statistics lifecycle | 3.16 | Product docs |
| 9 `*` inside spans | 3.4 | Rejects the docs' example |
| 10 `IN`/`AT`/`ALL` outside context | 3.5 | Reserved everywhere |
| 11 Tokenizer binding | 3.12 | Binds to the index; refuses where it cannot |
| 12 Segment option domains | 3.16 | Product docs |
| 13 `tin.tokenize` signature | 3.14 | Named options work |
| 14 `score` with `full_score` on one relation | 3.16 | Product docs (rejected) |

### 3.1 `IN WORDS X TO Y` counts positions from 1

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Positional filters: "Token positions are **0-based** (the first token is
  position `0`)". The table describes `IN WORDS X TO Y` as an inclusive 0-based
  range `[X, Y]`.
- **Observed.** `catalog.F-02` on the document `a b c d e`:
  - `a IN WORDS 0 TO 0` → `[]`
  - `c IN WORDS 2 TO 2` → `[]`
  - `a IN WORDS 1 TO 1` → `[1]`
- **Contrast.** `IN FIRST N WORDS` is 0-based as documented (`catalog.F-01`):
  `c IN FIRST 2 WORDS` → `[]` and `c IN FIRST 3 WORDS` → `[1]`. The Lead TINQL
  book (`positional-filters.md`) describes `IN WORDS` as counting from 1, which
  matches what TIN does.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'a b c d e');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> 'a IN WORDS 0 TO 0';  -- observed: 0 rows (docs: 1)
SELECT id FROM t WHERE body ==> 'c IN WORDS 2 TO 2';  -- observed: 0 rows (docs: 1)
SELECT id FROM t WHERE body ==> 'a IN WORDS 1 TO 1';  -- observed: 1     (docs: 0 rows)
```

**Impact.** Every `IN WORDS` window is shifted by one position relative to the
docs, and no error signals it. **Question 2**: which numbering is intended?
Changing the behavior would change results for existing queries, so a
documentation fix may be the safer choice.

### 3.2 Commas inside `[...]` act as separators

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Alternatives: `[a b c]` is an "OR of whitespace-separated alts (not commas)".
- **Observed.** `catalog.A-02`, on documents 1 `beer`, 2 `wine`, 3 `beer wine`
  and 4 `water`:
  - `[beer,wine]` → `[1, 2, 3]`
  - `[beer, wine]` → `[1, 2, 3]`
- **Why this shows commas separate.** If the comma were part of the term, the
  analyzer would split `beer,wine` into the phrase `"beer wine"`, which
  matches only document 3.
- **Commas between digits.** A comma between digits stays inside the term:
  `[47,000 48,000]` → `[1, 2]` (`catalog.A-03`).

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'beer'), (2, 'wine'), (3, 'beer wine'), (4, 'water');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> '[beer,wine]';  -- observed: 1, 2, 3
```

**Impact.** Low. We may be misreading "not commas": it could mean that commas
are not *required*. **Question 3.**

### 3.3 `wi-fi~N` does not raise an error

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Rules: "`wi-fi~2` errors because `wi-fi` is two words." The
  [Indexes](https://planetscale.com/docs/postgres/search/reference/indexes)
  page also says fuzzy matching requires a single token.
- **Observed.** `catalog.E-05`, on documents 1 `wi fx` and 2 `wi fi`:
  `wi-fi~1` → `[1, 2]`, with no error. The result is consistent with the phrase
  `"wi fi~1"`, with the fuzzy distance applied to the last word, which is what
  Lead does.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'wi fx'), (2, 'wi fi');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> 'wi-fi~1';  -- observed: 1, 2 (docs: ERROR)
```

**Impact.** Low. The behavior seems useful, and only the docs may need to
change. **Question 4.**

### 3.4 `*` inside a span or positional context is rejected, including the docs' own example

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Span relations: the example for `A NOT ENCLOSES B` is `* NOT ENCLOSES spam`.
- **Observed.** `catalog.R-06`, SQLSTATE `XX000`:
  - `* NOT ENCLOSES spam` → `invalid ==> query in "* NOT ENCLOSES spam": MatchAll (*) is not valid inside a span/positional context`
  - `* IN FIRST 3 WORDS` → the same message

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'spam eggs'), (2, 'eggs');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> '* NOT ENCLOSES spam';
-- observed: ERROR XX000 ... MatchAll (*) is not valid inside a span/positional context
```

**Impact.** Low, but a user who copies the documented example gets an error.
**Question 5.**

### 3.5 `IN`, `AT` and `ALL` are reserved outside their context

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Keyword index: "`IN` is special only before `FIRST` / `LAST` / `MIDDLE` /
  `WORDS`." The same passage says `AT` is special only before `LEAST`, and
  `ALL` only before `OF`.
- **Observed.** `catalog.Q-11`, on the document `in at all first of by`:
  - `IN` → `XX000 invalid ==> query at byte 2 in "IN": expected expected fuzzy_suffix (at byte 2), found unexpected input`
  - `AT` and `ALL` fail in the same way
  - `FIRST`, `OF` and `BY` each → `[1]`

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'in at all first of by');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> 'IN';     -- observed: ERROR XX000
SELECT id FROM t WHERE body ==> 'FIRST';  -- observed: 1
```

**Impact.** Low. Quoting (`"IN"`) is the documented workaround, and
`tin.maybe_quote` provides it. **Question 6.**

### 3.6 Relations do not chain

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Precedence: "Operators at the same level are left-associative." Relation
  operators are listed as one precedence level.
- **Observed.** `catalog.Q-19`: `a BEFORE b BEFORE c` →
  `XX000 invalid ==> query at byte 18 in "a BEFORE b BEFORE c": expected expected kw_TO (at byte 18), found unexpected input`.
  Lead's grammar allows at most one relation per `rel_expr`.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'a b c');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> 'a BEFORE b BEFORE c';  -- observed: ERROR XX000
```

**Impact.** Low. The workaround is `(a BEFORE b) BEFORE c`, but the error
message mentions `kw_TO`, which is unrelated to the problem. **Question 7.**

### 3.7 A phrase gap between repeated terms misses a match

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Phrases: "`_` = one any-word gap".
- **Observed.** `span.minimal_interval.1` and `.2`. The document ids are
  1 `x hotel hotel hotel zed y`, 2 `hotel x hotel zed` and 3 `hotel hotel zed`.

  | Query | Observed | Literal reading of the docs |
  |---|---|---|
  | `"hotel _ hotel zed"` | `[2]` | `[1, 2]`: positions 1, (2), 3, 4 in document 1 |
  | `"hotel _ hotel"` | `[2]` | `[1, 2]`: positions 1, (2), 3 in document 1 |
  | `"hotel hotel zed"` | `[1, 3]` | `[1, 3]` |

- **Plan independence.** The suite ran these cases with `tin.enable_custom_scan`
  on and off, and the matches were identical.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'x hotel hotel hotel zed y'), (2, 'hotel x hotel zed'),
                     (3, 'hotel hotel zed');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> '"hotel _ hotel zed"';  -- observed: 2
SELECT id FROM t WHERE body ==> '"hotel _ hotel"';      -- observed: 2
```

**Impact.** Medium: a silent false negative. It appears when the gap word is
itself another occurrence of a phrase term. Stannum's evaluator, which derives
from Lead's minimal-interval algebra, returns the same answers. The cause is
therefore probably the shared minimal-interval semantics, where the nearest
next occurrence is taken and the exact-gap check then fails, rather than
anything specific to TIN. **Question 8.**

### 3.8 A term in exactly 10% of documents is not elided

- **Documented.** [Scoring](https://planetscale.com/docs/postgres/search/scoring):
  "At the default ratio, a term found in 10% or more of the documents
  contributes nothing."
- **Observed.** `catalog.S-05` uses 100 documents, where `nine` appears in
  documents 1–9, `ten` in 1–10 and `eleven` in 1–11:
  - `tin.score` for `nine`: nonzero for every match. Document 1 is `3fedffaa`
    (1.859).
  - `ten` (df = 10 = 10%): nonzero for every match. Document 1 is `3fe3ec04`
    (1.781).
  - `eleven` (df = 11): `00000000` for every match.
  - `eleven^1` and `eleven` with `dense_ratio => 1.1` are nonzero, as
    documented.
- **`score_inspect` agrees** (`catalog.S-06`). At the default ratio it lists
  `nine` and `ten` but not `eleven`. At `dense_ratio => 0.25` it lists all
  three.
- **A possible explanation.** Our catalog (row S9) suggests an f32 → f64
  artifact. `0.1f32` widened to f64 is 0.10000000149…, so df = 0.1·N falls
  just below the threshold.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t SELECT n, 'padding ' || repeat('extra ', n % 4)
  || CASE WHEN n <= 9 THEN 'nine ' ELSE '' END
  || CASE WHEN n <= 10 THEN 'ten ' ELSE '' END
  || CASE WHEN n <= 11 THEN 'eleven ' ELSE '' END
  || CASE WHEN n = 1 THEN 'alpha beta gamma' ELSE 'beta alpha' END
FROM generate_series(1, 100) n;
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id, encode(float4send(tin.score(ctid)), 'hex') FROM t WHERE body ==> 'ten' ORDER BY id;
-- observed: all 10 rows nonzero (id 1: 3fe3ec04); docs: 0.0
SELECT id, encode(float4send(tin.score(ctid)), 'hex') FROM t WHERE body ==> 'eleven' ORDER BY id;
-- observed: all 11 rows 00000000
SELECT term, weight FROM tin.score_inspect('t_idx'::regclass, 'nine OR ten OR eleven') ORDER BY term;
-- observed: nine 1, ten 1
```

**Impact.** Low. Only the boundary is affected. **Question 9.**

### 3.9 A term in every row of a 20-row table still scores under `tin.score`

- **Documented.** [Scoring](https://planetscale.com/docs/postgres/search/scoring):
  "On a table of only a few rows, every term is dense, and every score is
  `0.0`."
- **Observed.** `catalog.S-07`: the index was built on an empty table, then 20
  rows `w k1` … `w k20` were inserted.
  - Before promotion: `tin.score(ctid)` for `w` is `3cc5683a` (0.0241) on
    every row, and `tin.full_score` gives the same value.
  - `SELECT tin.promote('t_idx'::regclass) IS NOT NULL` returned `true`.
  - After promotion: both functions still return `3cc5683a` on every row.
- **A possible explanation.** Lead computes elision from immutable-segment
  statistics only. That would explain the result before promotion. We cannot
  tell from our data why it persists after `tin.promote`.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
CREATE INDEX t_idx ON t USING tin (body);
INSERT INTO t SELECT n, 'w k' || n FROM generate_series(1, 20) n;
ANALYZE t;
SET enable_seqscan = off;
SELECT id, encode(float4send(tin.score(ctid)), 'hex') FROM t WHERE body ==> 'w' ORDER BY id;
-- observed: 3cc5683a on all 20 rows (docs: 0.0)
SELECT tin.promote('t_idx'::regclass) IS NOT NULL;  -- observed: true
SELECT id, encode(float4send(tin.score(ctid)), 'hex') FROM t WHERE body ==> 'w' ORDER BY id;
-- observed: 3cc5683a on all 20 rows
```

**Impact.** Low. Rankings on freshly written data follow different elision
rules from the documented ones. **Question 10.**

### 3.10 BEFORE highlighting: the docs' prose and example disagree, and TIN follows the example

- **Documented.** [Highlighting](https://planetscale.com/docs/postgres/search/highlighting):
  "For `a BEFORE b`, only the `b` occurrences that satisfy the relation are
  wrapped." The example on the same page shows
  `tin.highlight('b a b', query => 'a BEFORE b')` → `b <b>a</b> <b>b</b>`,
  which also wraps `a`.
- **Observed.** `catalog.H-02` → `b <b>a</b> <b>b</b>`, the same as the
  example.

```sql
SELECT tin.highlight('b a b', query => 'a BEFORE b');  -- observed: b <b>a</b> <b>b</b>
```

**Impact.** Documentation only. **Question 11**: should the prose say "the
`a` spans and the `b` occurrences that witness them"?

### 3.11 `ENCLOSED BY` highlights the outer span

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql),
  Span relations: "`ENCLOSES` returns the outer span and `ENCLOSED BY` returns
  the inner one."
  [Highlighting](https://planetscale.com/docs/postgres/search/highlighting)
  says highlighting "marks exactly the text that produced the match".
- **Observed.** `catalog.R-02`, on the document `a b c`:
  - `b ENCLOSED BY (a NEAR/3 c)` → `<b>a b c</b>`
  - `(a NEAR/3 c) ENCLOSES b` → `<b>a b c</b>` (also `catalog.H-13`)

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'a b c');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT tin.highlight(body) FROM t WHERE body ==> 'b ENCLOSED BY (a NEAR/3 c)';
-- observed: <b>a b c</b>   (our reading of the docs: a <b>b</b> c)
```

**Impact.** Low. Our reading may be wrong: "returns" might refer only to span
composition, not to highlighting. **Question 12.**

### 3.12 Non-default tokenization: two undocumented refusals

- **Documented.** [Indexes](https://planetscale.com/docs/postgres/search/reference/indexes):
  "Indexing and query analysis use the same pipeline, so `==>` searches agree
  with what the index stored."
  [Highlighting](https://planetscale.com/docs/postgres/search/highlighting):
  when `query` is omitted, the `==>` predicate on the same column is used.
  Neither page mentions a restriction for non-default tokenization.
- **Observed** on an index `WITH (case_folding = 'preserve')`:
  - `catalog.K-12`: `Apple` → `[2]` with the custom scan. With
    `tin.enable_custom_scan = off` it fails:
    `XX000 queries targeting a tin index with non-default tokenization require a usable tin custom scan path`.
  - `catalog.H-14`: implicit `tin.highlight(body)` fails even with the custom
    scan on:
    `XX000 implicit tin.highlight() is not supported with non-default index tokenization; pass the query explicitly or omit highlighting`.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'apple'), (2, 'Apple');
CREATE INDEX t_idx ON t USING tin (body) WITH (case_folding = 'preserve');
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> 'Apple' ORDER BY id;  -- observed: 2
SELECT tin.highlight(body) FROM t WHERE body ==> 'Apple';
-- observed: ERROR XX000 implicit tin.highlight() is not supported with non-default index tokenization; ...
SET tin.enable_custom_scan = off;
SELECT id FROM t WHERE body ==> 'Apple' ORDER BY id;
-- observed: ERROR XX000 queries targeting a tin index with non-default tokenization require a usable tin custom scan path
```

**Impact.** Low. Refusing is safer than answering with the wrong tokenizer,
but users only find these limits by hitting them. **Question 13**: could both
be listed on the Limitations page?

### 3.13 The error text for scoring outside a scan differs from the documented text

- **Documented.** [Scoring](https://planetscale.com/docs/postgres/search/scoring):
  outside a TIN scan, the call raises "requires a tin index scan and cannot be
  used in this query context".
- **Observed.** `catalog.S-16`: `SELECT tin.score(ctid) FROM t ORDER BY id LIMIT 1`
  → `XX000 synthetic-column queries require a usable tin custom scan path`.
  `catalog.S-22` returns the same text with the custom scan off (§4.1).
  `UPDATE ... WHERE body ==> 'alpha' RETURNING tin.score(ctid)` works
  (`catalog.S-16`, `[[1, "403c25f3"]]`).

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'alpha gamma');
INSERT INTO t SELECT 1000 + n, 'pad' || n FROM generate_series(1, 90) n;
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT tin.score(ctid) FROM t ORDER BY id LIMIT 1;
-- observed: ERROR XX000 synthetic-column queries require a usable tin custom scan path
```

**Impact.** Low. Users who search for the documented message will not find
this one. **Question 14.**

### 3.14 `tin.tokenize`: the signature line shows one argument, and named options work

- **Documented.** [Functions](https://planetscale.com/docs/postgres/search/reference/functions):
  the signature is given as `tin.tokenize(text) → setof text`. The prose says
  the function takes optional named arguments mirroring the index options
  (`tokenizer`, `case_folding`, `accent_folding`, `long_tokens`,
  `max_token_bytes`, `graphemes`, `position_gaps`).
- **Observed.** Named arguments work on 1.0.3:
  - `graphemes => 'emoji' | 'retain' | 'discard'` (`catalog.K-05`)
  - `tokenizer => 'whitespace'` (`catalog.K-08`)
  - `long_tokens => ...` and `max_token_bytes => 3` (`catalog.K-10`)
  - `tokenizer => 'icu'` → error (`catalog.K-13`)
- **Not recorded.** Our notes say the installed function has eight positional
  parameters, which would match the one-line signature plus seven options.
  The suite did not record `pg_proc`, so we list the exact signature as
  question 15.

```sql
SELECT array_agg(t) FROM tin.tokenize('Hello, World! wi-fi', tokenizer => 'whitespace') t;
-- observed tokens: 'hello,', 'world!', 'wi-fi'
```

**Impact.** Documentation only.

### 3.15 Where the docs are silent and Lead's reference implementation differs

These are not discrepancies. We record them so the team can confirm the
current behavior is intended.

**Expansions are scored.**
- `catalog.S-09`: `appl*` gives `4084117c` (4.127) to both `apple` and
  `apply`. `appl*^2` gives exactly twice that (`4104117c`).
- `catalog.S-10`: `MATCHES a.*`, `ant TO bee` and `ant~1` each score their
  matches at `4084117c`, the same as a plain term with df = 1.

**Boolean NOT is not scored, but a negative span relation is.** In
`catalog.S-11`, `score_inspect`:
- `a AND NOT b` → `[a 1.0]`
- `a NOT OVERLAPPING b` → `[a 1.0, b 1.0]`

**Repeated query terms add up** (`catalog.S-12`, `score_inspect` weights):
- `a a` → `a 2.0`
- `a OR a^2` → `a 3.0`
- `(a^2)^3` → `a 6.0`

Lead's reference implementation removes the duplicate in `a a` (weight 1.0),
and so does Stannum. This affects every user query that repeats a word.
**Question 16.**

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'a b');
INSERT INTO t SELECT 1000 + n, 'pad' || n FROM generate_series(1, 90) n;
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SELECT term, weight FROM tin.score_inspect('t_idx'::regclass, 'a a');           -- observed: a 2
SELECT term, weight FROM tin.score_inspect('t_idx'::regclass, 'a AND NOT b');   -- observed: a 1
SELECT term, weight FROM tin.score_inspect('t_idx'::regclass, 'a NOT OVERLAPPING b');  -- observed: a 1, b 1
```

### 3.16 Catalog conflicts where TIN matches the product docs

| Topic | Evidence | Observed |
|---|---|---|
| Scores sum across columns ([SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes): "`tin.score(ctid)` adds up the relevance from every column that matched.") | `catalog.S-18` | `name ==> 'fuji' OR notes ==> 'citrus'`: the row matching both scores `40e820d6` (7.254). Each single-column row scores `406820d6` (3.627). With `fuji^1.5`: `41111486` (9.068) = 5.441 + 3.627. Lead's reference scores only the first column; TIN follows the docs |
| `score` and `full_score` on one relation are rejected | `catalog.S-15` | `XX000 tin.score() and tin.full_score() cannot be combined on one scanned relation; use one scoring function per relation (tin.max_score() adapts to either)` |
| Deleted rows stay in the statistics until a rewrite ([Scoring](https://planetscale.com/docs/postgres/search/scoring), Visibility) | `catalog.S-20` | Deleting ids 2 and 3 leaves the bits of ids 1, 4 and 5 unchanged (`3f8711c4`, `3f4b534b`, `3e85a079`), and so does VACUUM. After REINDEX they become `3f70a451`, `3ef0a451`, `3ef0a451` |
| Option domains ([Indexes](https://planetscale.com/docs/postgres/search/reference/indexes)) | `catalog.I-02` | Every documented endpoint was accepted and every value one step outside was rejected (`22023`). This covers `initial_segment_count` and `target_segment_count` 1..4096, `max_mutable_segment_size` ≥ 131072 and `max_merged_segment_size` ≥ 100 |
| `score_stop_words` match analyzed terms exactly | `catalog.S-23` | `'The'` does not stop `the` (document 2 scores `406a2513`). `'the'` does (`00000000`) |
| `ALTER INDEX ... SET (k1 = ...)` takes effect without a rebuild | `catalog.S-23` | Document 1 goes from `40c2cea9` to `40c8ac65` |
| `max_score` | `catalog.S-17` | `score/max_score` = `3f800000` (1.0) for the top row. `max_score` alone = `406979d8` = `max(full_score)` |

## 4. Behavior inconsistencies within TIN

### 4.1 Scores require the custom scan (`catalog.S-22`, the only `variants_disagree`)

- **Documented.** [Overview](https://planetscale.com/docs/postgres/search):
  "The same row gets the same score however the query is executed."
  [Settings](https://planetscale.com/docs/postgres/search/reference/settings)
  describes `tin.enable_custom_scan = off` as switching to generic index
  scanning.
- **Observed.** The query is `a OR b`, on documents 1 `a`, 2 `a b` and 3 `b`
  plus 90 pad rows. The suite ran the same scoring query under two settings.
  TIN's two answers differ:

  | Settings | `full_score` bits | `score` bits |
  |---|---|---|
  | `enable_seqscan = off`, `tin.enable_custom_scan = on` | 1 `40692497`, 2 `40a5c296`, 3 `40692497` | the same |
  | `enable_seqscan = off`, `tin.enable_custom_scan = off` | `XX000 synthetic-column queries require a usable tin custom scan path` | the same error |

- **Scores never disagreed.** With the custom scan off, TIN refused rather
  than returning different values.
- **Matching is plan-independent.** The match cases that the suite runs under
  both settings (`span.*` and `span.minimal_interval.*`) gave identical ids
  and counts.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (1, 'a'), (2, 'a b'), (3, 'b');
INSERT INTO t SELECT 1000 + n, 'pad' || n FROM generate_series(1, 90) n;
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SET tin.enable_custom_scan = on;
SELECT id, encode(float4send(tin.full_score(ctid)), 'hex') FROM t WHERE body ==> 'a OR b' ORDER BY id;
-- observed: 1 40692497 | 2 40a5c296 | 3 40692497
SET tin.enable_custom_scan = off;
SELECT id, encode(float4send(tin.full_score(ctid)), 'hex') FROM t WHERE body ==> 'a OR b' ORDER BY id;
-- observed: ERROR XX000 synthetic-column queries require a usable tin custom scan path
```

**Question 17**: is this refusal intended? If so, could the Settings page say
that turning the custom scan off disables scoring?

### 4.2 `NEAR/N` matches less than `THEN/N`

- **Documented.** [TINQL](https://planetscale.com/docs/postgres/search/tinql):
  `THEN/N` is "Ordered proximity", `NEAR/N` is "Either order", and N is "the
  maximum number of extra words allowed between the operands". So every
  `A THEN/N B` match should also be an `A NEAR/N B` match.
- **Observed.** `span.minimal_interval.4` to `.7`, on documents 4
  `a b b c d`, 5 `a b c b d` and 6 `a b x x b`:

  | Query | Observed |
  |---|---|
  | `"a b" THEN/2 b` | `[4, 5, 6]` |
  | `"a b" NEAR/2 b` | `[]` |
  | `"a b" NEAR/1 b` | `[]` |
  | `b NEAR/2 "a b"` | `[]` |

- **Why the NEAR answers look wrong.** Document 4 has the phrase at positions
  0–1 and a separate `b` at position 2. The case description expected that the
  phrase's own `b` cannot also serve as the other operand, but a second `b`
  exists in all three documents.
- **Context.** Matches were identical with the custom scan on and off. Stannum,
  which derives from Lead, returns the same answers.

```sql
DROP TABLE IF EXISTS t;
CREATE TABLE t (id int PRIMARY KEY, body text);
INSERT INTO t VALUES (4, 'a b b c d'), (5, 'a b c b d'), (6, 'a b x x b');
CREATE INDEX t_idx ON t USING tin (body);
ANALYZE t;
SET enable_seqscan = off;
SELECT id FROM t WHERE body ==> '"a b" THEN/2 b' ORDER BY id;  -- observed: 4, 5, 6
SELECT id FROM t WHERE body ==> '"a b" NEAR/2 b' ORDER BY id;  -- observed: 0 rows
```

**Impact.** Medium: a silent false negative when a proximity operand shares a
term with the other operand. **Question 8** covers this together with §3.7.

### 4.3 Invalid or unbound queries: `==>` raises, `tin.highlight` stays silent

`catalog.H-08`:
- `tin.highlight('a b', '<b>', '</b>', 'a AND')` returns `a b` unchanged, with
  no error. The same query in `==>` raises `XX000` (`catalog.Q-15`,
  `dangling_and`).
- `SELECT tin.highlight('x')`, with no query and no predicate to bind, returns
  `x`. By contrast, `tin.score` with no TIN scan raises (§3.13).

```sql
SELECT tin.highlight('a b', '<b>', '</b>', 'a AND');  -- observed: a b
SELECT tin.highlight('x');                             -- observed: x
-- catalog.Q-15, on a one-row table with a tin index:
--   SELECT id FROM t WHERE body ==> 'apple AND'  -- observed: ERROR XX000 invalid ==> query at byte 9 ...
```

**Question 18**: is silently returning the input intended? It can hide a bug
in how an application builds its queries.

## 5. Errors and SQLSTATEs

The only SQLSTATE the docs name is `40001`, on replicas
([Limitations](https://planetscale.com/docs/postgres/search/reference/limitations)).
We found no error reference page. The table lists the SQLSTATEs TIN 1.0.3
raised:

| SQLSTATE | Condition | Cases |
|---|---|---|
| `XX000` internal_error | Every TINQL syntax error: `NOT peel`, bare `IN`/`AT`/`ALL`, `""`, `"   "`, `[]`, `apple AND`, `(apple`, `apple)`, `apple ^2`, `a BEFORE b BEFORE c`, `a THEN b`, `a IN MIDDLE 5 WORDS`, `"craft beer`, `beer OR` | `catalog.Q-06`, `Q-11`, `Q-14`, `Q-15`, `Q-19`, `X-03`, `F-07`, `smoke.error.unterminated_phrase`, `smoke.error.missing_operand` |
| `XX000` | Semantic query errors: `*` in a span, a wildcard or empty range bound | `catalog.R-06`, `E-10` |
| `XX000` | Invalid or unsupported regex: `MATCHES (`, `MATCHES (a)\1` | `catalog.E-07` |
| `XX000` | Bad scoring arguments: `term_add` together with `term_replace`, differing `dense_ratio`, a row-dependent `k1`, `k1 => -1`, `dense_ratio => -0.1`, `score` plus `full_score`, scoring without a scan | `catalog.S-13`, `S-14`, `S-24`, `S-15`, `S-16`, `S-22` |
| `XX000` | Bad function arguments: `highlight_ansi` `wrap_to` of 0 or -1, `tokenize` with `max_token_bytes => 3` or `tokenizer => 'icu'` | `catalog.H-10`, `K-10`, `K-13` |
| `XX000` | Plan or feature refusals | `catalog.K-12`, `H-14` |
| `22023` invalid_parameter_value | Index options out of range or with an unknown value, e.g. `value 1.1 out of bounds for option "b"`, `invalid value for enum option "tokenizer": icu` | `catalog.I-02`, `S-24` (`ddl_b_1_5`) |
| `0A000` feature_not_supported | `USING tin (id, body)`: `access method "tin" does not support multicolumn indexes` | `catalog.I-01` |
| `42704` undefined_object | `USING tin (id)`: `data type integer has no default operator class for access method "tin"` | `catalog.I-01` |
| `55P02` cant_change_runtime_param | `SET tin.maintenance_jobs_per_db = 1`: `parameter "tin.maintenance_jobs_per_db" cannot be changed now` | `catalog.I-07` |

**The same bound gets a different SQLSTATE depending on where it is checked:**
- `max_token_bytes = 3` raises `22023` as an index option (`catalog.I-02`) but
  `XX000` in `tin.tokenize` (`catalog.K-10`).
- `b = 1.5` raises `22023` at DDL (`catalog.S-24`), while `k1 => -1` at run
  time raises `XX000 failed to plan search cursor: BM25 k1 must be in [0, 10000] and b must be in [0, 1]`.

**Message formats observed:**

```text
invalid ==> query at byte 9 in "apple AND": expected expected base or kw_NOT (at byte 9), found unexpected input
invalid ==> query at byte 6 in "apple ^2": expected '^' adjacent to expression (no space) (at byte 6), found whitespace before '^'
invalid ==> query in "MATCHES (": invalid regex "(": regex parse error:
    \A(?:()\z
      ^
error: unclosed group
invalid ==> query in "... TO z": range bound "..." sub-tokenizes into no tokens
tin.score() dense_ratio must be finite and >= 0
wrap_to must be positive
```

- Parse errors take the form `invalid ==> query at byte N in "<query>": <detail> (at byte N)`.
- Errors found after parsing have no offset: `invalid ==> query in "<query>": <detail>`.
- Messages name internal grammar rules (`kw_TO`, `fuzzy_suffix`, `and_sep`,
  `rel_op`), and the word "expected" appears twice.
- `NOT peel` reports `expected kw_TO`, because `NOT` is read as the first
  bound of a range.
- The regex message exposes the anchored form TIN compiles
  (`\A(?:...)\z`), which also tells users that backreferences are
  unsupported.
- Each message repeats the whole query text. We do not know how that behaves
  for very long queries.

**Question 19**: are stable, documented codes planned? For example `42601`
(syntax_error) for TINQL syntax, `2201B` (invalid_regular_expression) for
regexes, and `22023` for invalid function arguments. Applications currently
cannot tell a user's typo from an internal fault without parsing message
text.

## 6. Undocumented surface

**Already documented, and not part of this section.** The
[Functions](https://planetscale.com/docs/postgres/search/reference/functions)
page does document `tin.fsck(regclass, boolean)`, `tin.score_inspect`, and
`tin.maybe_quote`. The
[Settings](https://planetscale.com/docs/postgres/search/reference/settings)
page documents the eight `tin.debug_*` settings as session-level
(`SET`). It adds that they "never change query results, and a query shape
that would be incorrect is still refused". So debug settings being
user-settable is documented, and the suite used
`tin.debug_disable_count_pushdown = on` under `SET LOCAL` on 1.0.3
(`catalog.N-02`: 50 matches with and without pushdown).

We could not find these functions in the docs:

| Function | 1.0.3 evidence | Shape recorded on TIN 1.0.2 on 2026-09-17 (earlier notes, not part of the suite) |
|---|---|---|
| `tin.promote` | `catalog.S-07`: `tin.promote('t_idx'::regclass) IS NOT NULL` → `true` | `promote(index, extent_cap_bytes)` → consumed_controls, linked_segments, docs_promoted, terms_added |
| `tin.segment_info` | none | `segment_info(index)` → ordinal, kind, root_block, docs, dead_docs, sum_doc_lengths, npostings, total_pages, source_state, origin, sequence |
| `tin.merge` | none | `merge(index, target_segment_count, high_water_multiplier, max_fan_in, force)` → considered, retired_only, merged, linked, output_docs, output_postings, replayed_kills, no_op_reason |
| `tin.fsm_rebuild` | none | `fsm_rebuild(index)` → start_nblocks, marked_blocks, swept_blocks, swept_runs |
| `tin.ql_parse` | none | Callable as `tin.ql_parse('history')` in earlier experiments; full signature not recorded |
| Operator support: `tin_text_cmpfunc_indexed`, `tin_text_term_array_cmpfunc_indexed`, `tin_text_restrict`, `tin_text_join` | none | Present; planner and operator support |

The eight-argument `tin.tokenize` is covered in §3.14.

**Question 20**: which of these are supported interfaces? Specifically: may
operators call `promote`, `merge` and `fsm_rebuild` during maintenance, and
may monitoring rely on the columns of `segment_info`? What privileges does
each require? Is `ql_parse` output (TIN's parse tree) a stable format?

## 7. Behavior the docs leave unspecified

These are recorded answers. Please confirm which are intended and can be
relied on.

**NULLs and empty input**

| Behavior | Case | Recorded answer |
|---|---|---|
| `NULL::text ==> 'a'`; `'a' ==> NULL::text` | `catalog.N-05` | `NULL`; `NULL` |
| `WHERE body ==> NULL` | `catalog.N-05` | no rows, no error |
| `==> ANY(ARRAY['apple', NULL])`; `==> ANY('{}'::text[])` | `catalog.N-04` | `[1]` (NULL element ignored); `[]` |
| `*` on documents `apple`, `''`, `'...'`, NULL | `catalog.Q-12` | `[1]`, count 1: token-less and NULL documents do not match `*` |
| Queries `''`, `'   '`, `'!!!'` | `catalog.Q-13` | count 0, no error (documented) |
| `"   "` (a phrase of blanks) | `catalog.Q-14` | `XX000 ... empty phrase` |
| `"..."` (a phrase that analyzes to nothing) | `catalog.P-11` | count 0, no error |
| `tin.tokenize(NULL)`; `tin.highlight(NULL, query => 'a')` | `catalog.K-13`; `catalog.H-08` | 0 rows; `NULL` |
| Token-less documents in N and avgdl | `catalog.S-21` vs `S-04` | Adding five `''` rows leaves every score bit unchanged, so they are not counted |

**Ordering, ties and paging**

| Behavior | Case | Recorded answer |
|---|---|---|
| Ten equal scores, `ORDER BY full_score DESC LIMIT 3` | `catalog.T-02` | ids {1, 2, 3}. This is the set; the suite orders ties by id itself |
| `LIMIT 5` equals the head of the full ordering | `catalog.T-01` | `[20, 10, 5, 11, 12]` = the first 5 of `[20, 10, 5, 11, 12, 3, 6, 13, 2, 14, ...]` |
| `ORDER BY full_score DESC, id LIMIT 3 OFFSET 3` | `catalog.T-03` | `[11, 12, 3]`, the exact slice |
| A filter on an unindexed column, top 5 excluded | `catalog.T-04` | `[3, 6, 13]`; top-k still fills k |
| Scores one ulp apart (`k1 = 0`), `LIMIT 1` | `bm25.k1.0`, `catalog.T-05` | Row 2 (`3d3e8bc6`), not row 1 (`3d3e8bc5`): the exhaustive top |
| Two `==>` on the same column vs one query | `catalog.S-19` | `body ==> 'a' AND body ==> 'b'` = `body ==> 'a b'` = `40bc25f3` |

**Operators**

| Behavior | Case | Recorded answer |
|---|---|---|
| `AT LEAST 5 OF [a b]` | `catalog.A-07` | `[]`, no error |
| `AT LEAST 0 OF [a b]` on `a`, `a b`, `a b c` | `catalog.A-07` | `[1, 2, 3]`. Every document contains `a`, so this does not show whether a document with no element matches (question 21) |
| `AT LEAST N% OF [a b c]` at 50, 34 and 33 | `catalog.A-05` | Needs 2, 2 and 1: rounds up |
| `t IN FIRST 25%` / `20%`, t at position 2 of 10 | `catalog.F-04` | Match / no match: the window rounds up (3 and 2) |
| `m IN MIDDLE 50%` on 10 tokens, m at 3 vs 2 | `catalog.F-05` | Only position 3 matches |
| `IN FIRST 0 WORDS`; `IN WORDS 5 TO 2` | `catalog.F-07` | `[]`; `[]`, no error |
| `a NEAR/2 a` on `a` and `a x a` | `catalog.X-06` | `[2]`: one occurrence does not pair with itself |
| `a NEAR/5 b WITHIN 2` vs `(a NEAR/5 b) WITHIN 2` | `catalog.X-05` | `[1, 2]` vs `[1]`: WITHIN binds to `b`, consistent with the precedence table |
| `teh~1` / `teh~2` on `the` | `catalog.E-04` | `[]` / `[1]`: Levenshtein, a transposition costs 2 |
| `MATCHES Apple.*` / `apple.*` / `ppl` | `catalog.E-06` | `[]` / `[1, 2]` / `[]`: anchored, matched against folded terms |
| `a TO b` on `ä`, `b`, `9`, `aé` | `catalog.E-09` | `[1, 2, 4]`: byte order of folded terms |
| `cat TO bee` (reversed) | `catalog.E-08` | `[]`, no error |
| `"foo_bar"` / `"foo\_bar"` / `foo_bar` on `foo_bar`, `foo x bar`, `foo bar` | `catalog.P-08` | `[2]` / `[1]` / `[1]`: an unescaped `_` inside a phrase is a gap, even inside a word |

**Tokenization**, the default pipeline via `tin.tokenize`

| Input | Case | Tokens |
|---|---|---|
| `3.14 can't wi-fi example.com e-mail U.S.A. 1,000 foo_bar @user #tag` | `catalog.K-01` | `3.14`, `can't`, `wi`, `fi`, `example.com`, `e`, `mail`, `u.s.a`, `1,000`, `foo_bar`, `user`, `tag` |
| `Straße STRASSE İstanbul ΣΊΣΥΦΟΣ` | `catalog.K-02` | `straße`, `strasse`, `istanbul`, `σισυφοσ`: lowercasing, not full case folding; no final sigma |
| `Jalapeño ﬁne Ｆｕｌｌ ① x²` | `catalog.K-03` | `jalapeno`, `ﬁne`, `ｆｕｌｌ`, `①`, `x`, `²`: no compatibility (NFKC) folding; the ligature, fullwidth and circled forms stay distinct |
| `東京タワー ひらがな 한국어` | `catalog.K-06` | `東`, `京`, `タワー`, `ひ`, `ら`, `か`, `な`, `한국어` |
| `I ❤️ 😀 👩‍💻 🇺🇸` | `catalog.K-05` | emoji and retain modes: `i`, `❤`, `😀`, `👩‍💻`, `🇺🇸` (variation selector dropped); discard mode: `i` |
| `repeat('a', 300)` | `catalog.K-10` | split: 256 + 44 bytes; truncate: 256; discard: no tokens |

**Tokenization observations**
- **Voiced kana fold to unvoiced.** In `ひらがな`, `が` became `か` (K-06). NFD
  splits off the dakuten, and accent folding strips it. This merges different
  Japanese words.
- **Unaccented and NFD queries match** (`catalog.K-04`). `cafe` and an NFD
  `café` both match an NFC `café`.
- **Kanji match per character** (`catalog.K-07`). `"東京"` and `東` both match
  `東京タワー`.
- **Question 22**: is dakuten and handakuten folding intended under
  `accent_folding = fold`?

**Term-frequency quantization**
- `catalog.S-02`: tf 3 and tf 4, in documents of the same length, score the
  same (`404d5022`).
- `catalog.S-03`: tf 4 and tf 5 differ (`404d5022` vs `4086d86c`).
- This matches Lead's 16 buckets, with representatives 1, 2, 3, 5, 10, 20,
  41, …
- As a result, rankings are not monotonic in tf. In `catalog.T-01`, tf 20
  ranks above tf 10, 5, 11 and 12, which rank above tf 19.

**Dense-term elision**
- `catalog.S-08`: `the` is in 20 of 110 documents.
  - `tin.score` for `the AND midnight` = `4056a9ee` (3.354).
  - `tin.full_score` = `40957424` (4.670).
- `dense_ratio > 1` disables elision (`catalog.S-05`).
- **Question 23**: is the 16-bucket quantization a stable part of the scoring
  contract? Could the docs mention it? It explains ties that users will
  notice.

**Highlighting**

| Behavior | Case | Recorded answer |
|---|---|---|
| Document text is not HTML-escaped | `catalog.H-07` | `<script><b>apple</b></script> & "x"` |
| `$QUERY_PART` for a phrase | `catalog.H-06` | `PHRASE(&quot;fuji apple&quot;); apple` |
| `$QUERY_LABEL` | `catalog.H-06` | `apple`, `qpart-3d`, `cafe`, `apple phrase-fuji-apple` |
| `$QUERY_PART` for `email*` | `catalog.H-05` | `MATCHES email.*` |
| Phrase with punctuation | `catalog.H-03` | `<b>Fuji, apple</b>!` |
| Separate terms vs overlapping matches | `catalog.H-04` | `<b>fuji</b> <b>apple</b>` vs `<b>fuji apple</b>` |
| Implicit query through `UPDATE ... RETURNING`, a CTE, a subquery | `catalog.H-12` | All `<b>urgent</b> x` |
| `highlight_ansi` on `red apple`, query `red` | `catalog.H-09` | `\e[1;31mred\e[0m apple` (hex `1b5b313b33316d7265641b5b306d206170706c65`). With query `apple`: `red \e[1;34mapple\e[0m`. Phrase `"fuji apple"`: `\e[97;104mfuji apple\e[0m` |
| `highlight_ansi(body, 10)` | `catalog.H-10` | `one two\nthree four\nfive \e[1;34mapple\e[0m` |

**Question 24**: is the missing escaping of document text intended? If so,
could the Highlighting page warn that output inserted into HTML must be
escaped first? Separately, is `PHRASE(...)` the intended form of
`$QUERY_PART` for phrases?

## 8. Open questions

1. (§2) Does `SELECT 'x' ==> repeat('a ', 10000)`, with no table or index,
   crash too? Which roles can reach the parser?
2. (§3.1) Is `IN WORDS X TO Y` meant to be 0-based, as the product docs say,
   or 1-based, as TIN and Lead's book do?
3. (§3.2) Are commas meant to separate alternatives inside `[...]`?
4. (§3.3) Should `wi-fi~2` error, or is the phrase-with-fuzzy-last-word
   behavior the intended one?
5. (§3.4) Should `* NOT ENCLOSES spam` work, or should the example change?
6. (§3.5) Should `IN`, `AT` and `ALL` be plain terms outside their context?
7. (§3.6) Should relations chain left-associatively, or is one relation per
   expression intended?
8. (§3.7, §4.2) Should `"hotel _ hotel"` match `x hotel hotel hotel`? Should
   `"a b" NEAR/2 b` match everything `"a b" THEN/2 b` matches?
9. (§3.8) Is a term at exactly `dense_ratio × N` dense, as the docs say, or
   not, as TIN does?
10. (§3.9) Which statistics drive elision for rows in the mutable segment and
    after `tin.promote`?
11. (§3.10) Is the BEFORE example the intended behavior, so that the prose
    needs to change?
12. (§3.11) Should `ENCLOSED BY` highlight only the inner span?
13. (§3.12) Can the two non-default-tokenization refusals be documented?
14. (§3.13) Which error text for scoring outside a scan is current?
15. (§3.14) What is the installed signature of `tin.tokenize`, and are
    positional calls supported?
16. (§3.15) Is it intended that a repeated query term (`a a`) doubles its
    weight?
17. (§4.1) Is refusing to score with `tin.enable_custom_scan = off` intended?
18. (§4.3) Should `tin.highlight` raise on an invalid explicit query, or when
    it has nothing to bind to?
19. (§5) Are stable SQLSTATEs (`42601`, `2201B`, `22023`) planned for user
    errors?
20. (§6) Which of `promote`, `merge`, `fsm_rebuild`, `segment_info` and
    `ql_parse` are supported, with what privileges and stability?
21. (§7) Does `AT LEAST 0 OF [...]` match documents that contain none of the
    elements?
22. (§7) Is dakuten and handakuten folding intended?
23. (§7) Is term-frequency bucketing part of the scoring contract?
24. (§7) Is unescaped document text in `tin.highlight` intended? Is
    `PHRASE(...)` the intended `$QUERY_PART` for phrases?
25. (§7) Are the recorded choices for ties under `LIMIT` (lowest ids here)
    and for token-less documents under `*` (no match) guaranteed, or
    incidental?

## 9. About the suite

The Stannum project maintains the suite. It is a self-contained directory, so
it can live on its own: a Python runner, declarative YAML cases grouped by
area, and recorded answers per engine version. The runner connects through a
connection string it reads only from an environment variable. It creates a
scratch schema for each run, builds each corpus once, and runs every capture
in its own transaction, which it rolls back. It drops the schema at the end.

To detect crashes, the runner keeps an idle sentinel connection open. If the
sentinel dies, the server reinitialized.

A case looks like this:

```yaml
corpora:
  cat_f01:
    rows:
    - [1, a b c d e]
cases:
- id: catalog.F-02
  description: 'IN WORDS X TO Y: 0-based (product docs) or 1-based (Lead)?'
  source: docs/tin-behavior-catalog.md §9 F-02; behaviour §1.7 F2; §8 conflict 1
  priority: conflict
  corpus: cat_f01
  settings: {enable_seqscan: 'off'}
  capture:
  - ids: {query: a IN WORDS 0 TO 0, as: a_0_0}
  - ids: {query: c IN WORDS 2 TO 2, as: c_2_2}
  - ids: {query: a IN WORDS 1 TO 1, as: a_1_1}
```

**Capture kinds:** ids, count, top-k ranking, float4 score bits, highlight
text, arbitrary SQL values, SQLSTATE and message, and multi-step or
multi-session scripts. A case can also require identical answers under
several setting variants, such as the custom scan on and off. When they
differ, the answer is recorded as `variants_disagree`, with every answer.

**Record mode** writes `expected/<engine>-<version>/<area>.json`. Each file
starts with a `source` header. TIN 1.0.3's header reads:

```json
{
 "engine": "tin",
 "extension_version": "1.0.3",
 "server_version": "PostgreSQL 18.6 (Debian 18.6-1.pgdg12+2) on aarch64-unknown-linux-gnu, ...",
 "host": "PlanetScale Postgres 18.6, us-east-1",
 "date": "2026-09-26T19:18:38Z",
 "suite_commit": "64b7f8e52b9f40be77c6a1a34a5ea695d4866cf8",
 "runner_version": "1"
}
```

**Check mode** replays a recorded directory against another engine or
version. It reports each case as PASS, DIFF (same SQLSTATE, different message
text), FAIL or SKIP, and compares answers exactly. Recorded answers are never
edited. A new release gets a new directory, so the difference between
`tin-1.0.3` and a future `tin-1.0.4` is itself a record of what changed.

We would be glad to share the suite with the TIN team, and to run it against
future TIN versions and send the diff. Where to publish the suite is still
being decided.

## Appendix: all 189 cases and TIN 1.0.3's recorded outcome

The table was generated from `conformance/expected/tin-1.0.3/*.json` at suite
commit `cb42794`.

- "answered": every capture returned rows.
- "error": every capture raised an ERROR.
- Mixed cases give the error count out of all captures.
- The three crashing cases were recorded by the earlier probe and are skipped
  whenever the suite runs against TIN 1.0.3 (they are tagged
  `crashes: [tin-1.0.3]`).

**Totals:** 160 answered; 15 error; 10 mixed; 1 variants disagree; 3 crashed
the server.

| Case | Outcome |
|---|---|
| `bm25.k1.0` | answered |
| `bm25.k1.0_001` | answered |
| `bm25.k1.0_01` | answered |
| `bm25.k1.1_2` | answered |
| `catalog.A-01` | answered |
| `catalog.A-02` | answered |
| `catalog.A-03` | answered |
| `catalog.A-04` | answered |
| `catalog.A-05` | answered |
| `catalog.A-06` | answered |
| `catalog.A-07` | answered |
| `catalog.A-08` | answered |
| `catalog.N-01` | answered |
| `catalog.N-02` | answered |
| `catalog.N-03` | answered |
| `catalog.N-04` | answered |
| `catalog.N-05` | answered |
| `catalog.N-06` | answered |
| `catalog.N-07` | answered |
| `catalog.I-01` | error 0A000, 42704 (2 of 2 captures) |
| `catalog.I-02` | answered; error 22023 in 20 of 49 captures |
| `catalog.I-03` | answered |
| `catalog.I-04` | answered |
| `catalog.I-05` | answered |
| `catalog.I-06` | answered |
| `catalog.I-07` | error 55P02 |
| `catalog.E-01` | answered |
| `catalog.E-02` | answered |
| `catalog.E-03` | answered |
| `catalog.E-04` | answered |
| `catalog.E-05` | answered |
| `catalog.E-06` | answered |
| `catalog.E-07` | error XX000 (2 of 2 captures) |
| `catalog.E-08` | answered |
| `catalog.E-09` | answered |
| `catalog.E-10` | error XX000 (2 of 2 captures) |
| `catalog.H-01` | answered |
| `catalog.H-02` | answered |
| `catalog.H-03` | answered |
| `catalog.H-04` | answered |
| `catalog.H-05` | answered |
| `catalog.H-06` | answered |
| `catalog.H-07` | answered |
| `catalog.H-08` | answered |
| `catalog.H-09` | answered |
| `catalog.H-10` | answered; error XX000 in 2 of 3 captures |
| `catalog.H-11` | answered |
| `catalog.H-12` | answered |
| `catalog.H-13` | answered |
| `catalog.H-14` | error XX000 |
| `catalog.P-01` | answered |
| `catalog.P-02` | answered |
| `catalog.P-03` | answered |
| `catalog.P-04` | answered |
| `catalog.P-05` | answered |
| `catalog.P-06` | answered |
| `catalog.P-07` | answered |
| `catalog.P-08` | answered |
| `catalog.P-09` | answered |
| `catalog.P-10` | answered |
| `catalog.P-11` | answered |
| `catalog.F-01` | answered |
| `catalog.F-02` | answered |
| `catalog.F-03` | answered |
| `catalog.F-04` | answered |
| `catalog.F-05` | answered |
| `catalog.F-06` | answered |
| `catalog.F-07` | answered; error XX000 in 1 of 3 captures |
| `catalog.F-08` | answered |
| `catalog.X-01` | answered |
| `catalog.X-02` | answered |
| `catalog.X-03` | error XX000 |
| `catalog.X-04` | answered |
| `catalog.X-05` | answered |
| `catalog.X-06` | answered |
| `catalog.R-01` | answered |
| `catalog.R-02` | answered |
| `catalog.R-03` | answered |
| `catalog.R-04` | answered |
| `catalog.R-05` | answered |
| `catalog.R-06` | error XX000 (2 of 2 captures) |
| `catalog.S-01` | answered |
| `catalog.S-02` | answered |
| `catalog.S-03` | answered |
| `catalog.S-04` | answered |
| `catalog.S-05` | answered |
| `catalog.S-06` | answered |
| `catalog.S-07` | answered |
| `catalog.S-08` | answered |
| `catalog.S-09` | answered |
| `catalog.S-10` | answered |
| `catalog.S-11` | answered |
| `catalog.S-12` | answered |
| `catalog.S-13` | answered; error XX000 in 1 of 3 captures |
| `catalog.S-14` | answered; error XX000 in 2 of 3 captures |
| `catalog.S-15` | error XX000 |
| `catalog.S-16` | answered; error XX000 in 1 of 2 captures |
| `catalog.S-17` | answered |
| `catalog.S-18` | answered |
| `catalog.S-19` | answered |
| `catalog.S-20` | answered |
| `catalog.S-21` | answered |
| `catalog.S-22` | variants disagree: answered with the custom scan on, error XX000 with it off |
| `catalog.S-23` | answered |
| `catalog.S-24` | error 22023, XX000 (3 of 3 captures) |
| `catalog.Q-01` | answered |
| `catalog.Q-02` | answered |
| `catalog.Q-03` | answered |
| `catalog.Q-04` | answered |
| `catalog.Q-05` | answered |
| `catalog.Q-06` | error XX000 |
| `catalog.Q-07` | answered |
| `catalog.Q-08` | answered |
| `catalog.Q-09` | answered |
| `catalog.Q-10` | answered |
| `catalog.Q-11` | answered; error XX000 in 3 of 6 captures |
| `catalog.Q-12` | answered |
| `catalog.Q-13` | answered |
| `catalog.Q-14` | error XX000 (3 of 3 captures) |
| `catalog.Q-15` | error XX000 (4 of 4 captures) |
| `catalog.Q-16` | answered |
| `catalog.Q-17` | answered |
| `catalog.Q-18` | answered |
| `catalog.Q-19` | error XX000 |
| `catalog.Q-20` | answered |
| `catalog.K-01` | answered |
| `catalog.K-02` | answered |
| `catalog.K-03` | answered |
| `catalog.K-04` | answered |
| `catalog.K-05` | answered |
| `catalog.K-06` | answered |
| `catalog.K-07` | answered |
| `catalog.K-08` | answered |
| `catalog.K-09` | answered |
| `catalog.K-10` | answered; error XX000 in 1 of 4 captures |
| `catalog.K-11` | answered |
| `catalog.K-12` | answered; error XX000 in 1 of 2 captures |
| `catalog.K-13` | answered; error XX000 in 1 of 2 captures |
| `catalog.T-01` | answered |
| `catalog.T-02` | answered |
| `catalog.T-03` | answered |
| `catalog.T-04` | answered |
| `catalog.T-05` | answered |
| `catalog.T-06` | answered |
| `query_size.words.1000` | answered |
| `query_size.words.3000` | answered |
| `query_size.words.10000` | server crashed (earlier probe; skipped as crashing) |
| `query_size.or_chain.1000` | answered |
| `query_size.or_chain.3000` | answered |
| `query_size.or_chain.10000` | server crashed (earlier probe; skipped as crashing) |
| `query_size.nested.100` | answered |
| `query_size.nested.1000` | answered |
| `query_size.nested.5000` | server crashed (earlier probe; skipped as crashing) |
| `smoke.term.single` | answered |
| `smoke.term.case_folding` | answered |
| `smoke.term.no_match` | answered |
| `smoke.and.implicit` | answered |
| `smoke.and.explicit` | answered |
| `smoke.or.terms` | answered |
| `smoke.or.precedence` | answered |
| `smoke.not.and_not` | answered |
| `smoke.group.parentheses` | answered |
| `smoke.phrase.adjacent` | answered |
| `smoke.phrase.reversed` | answered |
| `smoke.prefix.trailing` | answered |
| `smoke.near.either_order` | answered |
| `smoke.boost.zero_still_matches` | answered |
| `smoke.boost.reorders` | answered |
| `smoke.count.or_all` | answered |
| `smoke.ranked.term_frequency` | answered |
| `smoke.ranked.limit` | answered |
| `smoke.scores.bits` | answered |
| `smoke.highlight.default` | answered |
| `smoke.highlight.markers` | answered |
| `smoke.error.unterminated_phrase` | error XX000 |
| `smoke.error.missing_operand` | error XX000 |
| `span.minimal_interval.1` | answered |
| `span.minimal_interval.2` | answered |
| `span.minimal_interval.3` | answered |
| `span.minimal_interval.4` | answered |
| `span.minimal_interval.5` | answered |
| `span.minimal_interval.6` | answered |
| `span.minimal_interval.7` | answered |
| `span.then_phrase_operand.1` | answered |
| `span.then_phrase_operand.2` | answered |
| `span.then_phrase_operand.3` | answered |
| `span.then_phrase_operand.4` | answered |
| `span.then_terms.1` | answered |
| `span.near_phrase_operand.1` | answered |
