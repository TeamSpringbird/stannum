# TIN behavior catalog for a correctness suite

Compiled 2026-09-26. This catalog drives an open-source correctness suite. The
suite runs first against PlanetScale TIN (extension `tin` 1.0.3, PostgreSQL 18.6)
to record its answers, then against Stannum. PlanetScale's documentation is
paraphrased and cited, not copied.

Every behavior carries one of these provenance tags:

- **DOC**: stated in PlanetScale's public product documentation or blog.
- **LEADDOC**: stated in the public Lead repository's TINQL book
  (`tinql/docs/src`), which PlanetScale wrote for its open-source reference.
- **LEAD**: only implied by Lead's code. TIN may differ.
- **MEAS**: measured on hosted TIN earlier and recorded in this repository.
  The TIN version is given.
- **OPEN**: undocumented or contradictory. Measure it on TIN and record the answer.

Lead code citations use this worktree's paths. The `tinql/`, `tokenizer/` and
`boldi-vigna/` crates here match Lead `bd95c7e` (2026-09-19) except for license
headers and `tin`→`stannum` renames. Line numbers can shift by a few lines
against upstream. This repository's `postgres/` crate is Stannum's rewrite. For
that crate, citations name the upstream file, `Lead@bd95c7e:postgres/src/…`.

## Sources

| Key | Source | Date published | Retrieved |
| --- | --- | --- | --- |
| BLOG | [Introducing TIN: full-text search for Postgres](https://planetscale.com/blog/introducing-tin) (Ridge, Reynolds) | 2026-09-16, erratum 2026-09-20 (3,762 queries, not 1,719) | 2026-09-26 |
| CHLOG | [Changelog: TIN: Postgres full-text search](https://planetscale.com/changelog/tin-text-search) | 2026-09-16 | 2026-09-26 |
| LEADBLOG | [Introducing Lead: TIN-compatible full-text search for CI](https://planetscale.com/blog/introducing-lead) | 2026-09-17 | 2026-09-26 |
| ANAT | [Anatomy of a (Postgres) search engine](https://planetscale.com/blog/anatomy-of-a-postgres-search-engine) (general background; no TIN contracts) | 2026-09-22 | 2026-09-26 |
| OV | [TIN: PlanetScale Postgres Search (overview)](https://planetscale.com/docs/postgres/search) | undated | 2026-09-26 |
| GS | [Get started with TIN](https://planetscale.com/docs/postgres/search/get-started) | undated | 2026-09-26 |
| TQ | [TINQL](https://planetscale.com/docs/postgres/search/tinql) | undated | 2026-09-26 |
| SC | [Scoring](https://planetscale.com/docs/postgres/search/scoring) | undated | 2026-09-26 |
| HL | [Highlighting](https://planetscale.com/docs/postgres/search/highlighting) | undated | 2026-09-26 |
| OPS | [Operational guidance](https://planetscale.com/docs/postgres/search/operations) | undated | 2026-09-26 |
| OP | [The ==> operator](https://planetscale.com/docs/postgres/search/reference/operator) | undated | 2026-09-26 |
| IDX | [Indexes](https://planetscale.com/docs/postgres/search/reference/indexes) (also served at `/reference`) | undated | 2026-09-26 |
| FN | [Functions](https://planetscale.com/docs/postgres/search/reference/functions) | undated | 2026-09-26 |
| SQL | [Recommended SQL shapes](https://planetscale.com/docs/postgres/search/reference/sql-shapes) | undated | 2026-09-26 |
| LIM | [Limitations](https://planetscale.com/docs/postgres/search/reference/limitations) | undated | 2026-09-26 |
| SET | [Settings](https://planetscale.com/docs/postgres/search/reference/settings) | undated | 2026-09-26 |
| LEADREPO | [github.com/planetscale/lead](https://github.com/planetscale/lead), HEAD `bd95c7e` (2026-09-19): README and `tinql/docs/src/*.md` | 2026-09-19 | 2026-09-26 |
| BENCHREPO | [github.com/planetscale/paradedb-benchmarker](https://github.com/planetscale/paradedb-benchmarker), the benchmark driver the blog links (a fork of paradedb/benchmarker; last push 2026-09-17) | 2026-09-17 | 2026-09-26 (metadata only) |
| X | [PlanetScale on X, launch post](https://x.com/PlanetScale/status/2100258072333865363) (marketing; no contracts) | 2026-09-16 | search result only |
| M-ARCH | Measured, TIN 1.0.2, 2026-09-17: `git show 33e4ea8^:docs/archive/tin-observed-shape.md` (deleted from the current tree) | — | 2026-09-26 |
| M-CAT | Measured, TIN 1.0.2, 2026-09-19: [docs/benchmarks/tin-plan-catalog.md](benchmarks/tin-plan-catalog.md) | — | 2026-09-26 |
| M-BEH | Measured, TIN 1.0.3, 2026-09-26: [docs/tin-behavior.md](tin-behavior.md) | — | 2026-09-26 |

The raw `.md` form of each docs page is at `<url>.md`. For example,
`https://planetscale.com/docs/postgres/search/tinql.md` gives full SQL examples
without a rendering step. The docs index is `https://planetscale.com/docs/llms.txt`.
It lists no search pages, so the search section was found through cross-links.
These guessed pages returned **404**: `/docs/postgres/search/reference/errors`,
`/tokenization`, `/reference/tokenizer`, `/faq`, `/troubleshooting`. There is
no public error or SQLSTATE reference, no grammar file, no changelog beyond the
launch entry, and no talk or README beyond Lead's. Third-party write-ups
(runtimewire.com, byteiota.com) repeat PlanetScale's claims and are not used.

The public docs never mention these functions and settings seen on the live
install: `tin.ql_parse`, `tin.segment_info`, `tin.merge`, `tin.fsm_rebuild` and
`tin.promote`. They also omit the 8-argument form of `tin.tokenize`: FN documents
`tin.tokenize(text) → setof text`, plus named options.

---

## 1. Query syntax (TINQL)

Common fixture for the ideas below: `t(id int primary key, body text)`, a TIN
index on `body`, and the query `SELECT id FROM t WHERE body ==> Q ORDER BY id`.

### 1.1 Terms, case, keywords

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| Q1 | A bare word is one dictionary term, folded for case and accents under the default tokenizer. | DOC TQ (Rules, Terms) | 1 `Apple pie` / `apple`, `APPLE` → {1} |
| Q2 | Keywords are case-sensitive and must be upper case. Lower-case `and`/`or` are search terms. | DOC TQ, GS | 1 `cats and dogs`, 2 `cats dogs` / `cats and dogs` → {1} (three terms ANDed); `cats AND dogs` → {1,2} |
| Q3 | A reserved word is searched as a term by quoting it (`"AND"`). `tin.maybe_quote` adds the quotes when needed. | DOC TQ, FN | `SELECT tin.maybe_quote('AND')` → `"AND"`; `maybe_quote('beer')` → `beer`; `maybe_quote('foo*bar')` → `"foo*bar"` (LEAD tinql/src/quote.rs:40-55) |
| Q4 | The keyword set is AND OR NOT THEN NEAR WITHIN ENCLOSES ENCLOSED BY OVERLAPPING BEFORE AFTER TO IN FIRST LAST MIDDLE WORDS CONTAINS MATCHES AT LEAST OF ALL. IN, AT, ALL and ENCLOSED/BY are special only in context. | DOC TQ (Keyword index) | See Q5 |
| Q5 | Lead's grammar reserves IN, AT, ALL, TO, WITHIN, CONTAINS and MATCHES even outside their context, so bare `IN` is a parse error. FIRST, LAST, MIDDLE, WORDS, BY, LEAST and OF are plain terms. | LEAD tinql/src/parser/pest_parser/grammar.pest:136-144. **OPEN**: DOC says IN/AT/ALL are special only in context | Query `IN`, `AT`, `ALL`, `FIRST`, `OF`, `BY` against doc `in at all first of by`: record parse error or match per query |
| Q6 | `CONTAINS term` means the same as `term`. | DOC TQ | `CONTAINS apple` = `apple` |
| Q7 | Standalone `*` matches all documents. Lead excludes documents with no tokens. | DOC TQ. LEAD tinql/src/runtime/eval.rs:798 (empty doc never matches `*`) | 1 `a`, 2 `''`, 3 `...`, 4 NULL / `*` → {1}? record whether 2 and 3 match |
| Q8 | Term characters are anything except whitespace and `( ) [ ] " ~ ^`. The analyzer then splits a term that contains punctuation. | DOC TQ. LEAD grammar.pest:150-153 | `example.com` matches `visit example.com today`; also check `example` alone |
| Q9 | `-` is not negation. `-peel` becomes the term `peel`. | DOC GS (pitfalls). LEAD (the word tokenizes to `peel`) | 1 `apple peel`, 2 `apple` / `apple -peel` → {1} if positive, error otherwise. Record which |
| Q10 | A hyphenated term splits into a phrase: `wi-fi` → `"wi fi"`. | DOC TQ, IDX | 1 `wi-fi`, 2 `wi fi`, 3 `fi wi`, 4 `wifi` / `wi-fi` → {1,2} |
| Q11 | Whitespace in the grammar is only ASCII space, tab, CR and LF. Other Unicode spaces are term characters that the analyzer splits. | LEAD grammar.pest:6 | `apple pie` as a query (NBSP): record match set and `ql_parse` output |

### 1.2 Boolean operators and grouping

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| B1 | Juxtaposition means AND. | DOC TQ | 1 `apple grape`, 2 `apple` / `apple grape` → {1} |
| B2 | Explicit `AND` and `OR` work as expected. | DOC TQ | `apple OR grape` → {1,2} |
| B3 | Exclusion is written `A AND NOT B`. `NOT` never stands alone. It appears only in AND NOT, NOT ENCLOSES, NOT ENCLOSED BY and NOT OVERLAPPING. | DOC TQ | `NOT apple` → parse error (capture SQLSTATE and message); `* AND NOT apple` → docs without apple |
| B4 | Parentheses group, and `(…)` may nest. | DOC TQ | `(apple OR grape) AND pie` |
| B5 | Precedence, loose to tight: OR < AND (explicit and implicit) < AND NOT < IN… < relations < THEN/NEAR < WITHIN < ^ < primary. | DOC TQ (Precedence) | `A OR B THEN/5 C AND D` equals `A OR ((B THEN/5 C) AND D)`: compare match sets on a 6-doc corpus built to separate the parses |
| B6 | Operators at the same level associate to the left. | DOC TQ | `a AND NOT b AND NOT c` equals `(a AND NOT b) AND NOT c`; `a THEN/0 b THEN/0 c` equals `(a THEN/0 b) THEN/0 c` |
| B7 | AND NOT binds tighter than AND. `a b AND NOT c` parses as `a AND (b AND NOT c)`. | DOC TQ. LEAD grammar.pest:16-21 | Same match set as the explicit form |
| B8 | A relation (ENCLOSES and the rest) is not chainable: `rel_expr` allows at most one relation operator. | LEAD grammar.pest:42. **OPEN** for TIN | `a BEFORE b BEFORE c` → parse error? |
| B9 | A positional filter applies to the expression immediately to its left. | LEADDOC positional-filters.md | `beer IN FIRST 2 WORDS AND wine` vs `(beer AND wine) IN FIRST 2 WORDS` |
| B10 | A NOT whose operand is empty after analysis excludes nothing. An empty term as a positive conjunct matches nothing. | LEAD tinql/src/runtime/lower.rs:489-504 | `apple AND NOT ,` → docs with apple; `apple AND ,` → {} |
| B11 | Duplicate terms in flat AND/OR chains are removed during lowering. | LEAD lower.rs:12 | `apple apple` matches as `apple`; see S12 for the score effect |
| B12 | Deep nesting and very long queries: TIN 1.0.3 accepted 3,000 words, a 3,000-term OR and 1,000 levels. It **crashed the server** at 10,000 words or terms and 5,000 levels. | MEAS M-BEH | Do not run the crash sizes against shared TIN. Record the accepted sizes (3,000 and 1,000) as must-pass |

### 1.3 Phrases, slop, gaps, alternatives inside phrases

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| P1 | `"a b c"` matches adjacent words in order. | DOC TQ | 1 `fuji apple`, 2 `apple fuji`, 3 `fuji red apple` / `"fuji apple"` → {1} |
| P2 | `_` inside a phrase is exactly one wildcard position. `__` is two. | DOC TQ. LEAD ast.rs:532, lower.rs:414-427 | 1 `big bad wolf`, 2 `big wolf`, 3 `big very bad wolf` / `"big _ wolf"` → {1}; `"big __ wolf"` → {3} |
| P3 | Gaps are pinned per slot. `"a _ b __ c"` requires one position between a and b, and two between b and c. | LEAD lower.rs:414-427 | Check with a two-document corpus |
| P4 | Leading and trailing `_` have no anchor and are ignored. | LEAD lower.rs:352-354 | `"_ apple"` matches like `apple` |
| P5 | `"a b"~N` allows up to N extra words in total, in order. | DOC TQ | 1 `fuji red apple`, 2 `fuji x y z apple`, 3 `apple fuji` / `"fuji apple"~2` → {1}; `~3` → {1,2}; record whether 3 ever matches (order) |
| P6 | Slop combined with gaps: the pinned gaps become the minimum and slop adds tolerance on top. | LEAD lower.rs:403-412 | `"a _ c"~1` on `a c` (0 gaps), `a x c` (1), `a x y c` (2), `a x y z c` (3) → {2,3} |
| P7 | `[x y]` inside a phrase gives alternatives for one position. | DOC TQ | 1 `big bad wolf`, 2 `big large wolf` / `"big [bad large] wolf"` → {1,2} |
| P8 | Alternatives inside a phrase can hold expressions (Lead reparses them with the alternatives grammar). | LEAD pest_parser/mod.rs:734-758 | `"alpha [MATCHES b.*]"` (already in the oracle) |
| P9 | Phrase escapes are `\"`, `\\`, `\_`, `\[` and `\]`. | DOC TQ | `"a\_b"` → searches `a_b` (the analyzer keeps `_` inside a word? record `tin.tokenize('a_b')`) |
| P10 | An unescaped `_` inside a phrase word splits the word: `"foo_bar"` becomes foo, one gap, bar. | LEAD grammar phrase.pest:25 (`_` excluded from phrase_word_char) | Docs `foo_bar`, `foo x bar` / `"foo_bar"` → record. Outside a phrase, `foo_bar` is one query term |
| P11 | `""` is a parse error. So is a phrase of only whitespace. A phrase whose words all analyze away (`"..."`) matches nothing. | DOC TQ, OP. LEAD pest_parser/mod.rs:676-729, lower.rs:379-383 | `""` → error; `"   "` → error?; `"..."` → 0 rows, no error |
| P12 | A single-word phrase is just that term. | LEAD lower.rs:99-107 | `"apple"` = `apple` (match set and score bits) |
| P13 | A phrase does not cross the end of one document into another, and punctuation or newlines between words do not break adjacency. | MEAS (raw-text oracle agreed with TIN): benchmarks/oracle.py:596-625 | `alpha, beta` and `alpha\nbeta` both match `"alpha beta"` |

### 1.4 Alternatives, AT LEAST, ALL OF

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| A1 | `[a b c]` is an OR of its elements, separated by whitespace. | DOC TQ | `[mango plum]` |
| A2 | **Conflict.** The product docs say elements are separated by whitespace, "not commas". Lead's docs and code treat commas as separators too, except a comma between two digits. | DOC TQ vs LEADDOC alternatives.md:18-21, LEAD pest_parser/mod.rs:594-649. **OPEN** | `[beer,wine]` on docs `beer`, `wine`, `beer wine`: TIN's answer decides. Also `[47,000 48,000]` |
| A3 | `[]` is a parse error. | DOC TQ, OP | Capture the error |
| A4 | Implicit AND is disabled inside brackets, so `[a b]` never means `[a AND b]`. Explicit AND/OR/AND NOT are allowed. | LEAD grammar.pest:23-27 | `[a AND b c]` → (a∧b) ∨ c |
| A5 | `AT LEAST N OF [..]` needs at least N elements to match. | DOC TQ | 4 docs holding 1–4 of `a b c d` / `AT LEAST 2 OF [a b c d]` |
| A6 | `AT LEAST N% OF [..]` rounds **up**: 50% of 5 needs 3. | LEADDOC alternatives.md:75-76. LEAD lower.rs:458 (`div_ceil`) | `AT LEAST 50% OF [a b c]` → needs 2; `AT LEAST 34% OF [a b c]` → needs 2; `AT LEAST 33% OF [a b c]` → needs 1 |
| A7 | `ALL OF [..]` requires every element. | DOC TQ | Same result as a chain of ANDs |
| A8 | N larger than the number of elements, and `AT LEAST 0`, are undocumented. | **OPEN** (Lead: min > n never matches; min 0?) | `AT LEAST 5 OF [a b]`, `AT LEAST 0 OF [a b]`, `AT LEAST 101% OF [a]`: capture rows or error |
| A9 | Elements that analyze to nothing still count toward the list length. | LEAD lower.rs:518-531 | `AT LEAST 2 OF [beer , wine , stout]` needs 2 of 3 |

### 1.5 Proximity and WITHIN

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| X1 | THEN and NEAR require `/N`. N counts the extra words allowed between the operands, so 0 means adjacent. | DOC TQ | `a THEN b` → parse error |
| X2 | `A THEN/N B` is ordered. | DOC TQ | 1 `a b`, 2 `a x b`, 3 `b a` / `a THEN/0 b` → {1}; `THEN/1` → {1,2} |
| X3 | `A NEAR/N B` matches either order. | DOC TQ | `a NEAR/0 b` → {1,3} |
| X4 | Operands can be phrases or other spans. Measured TIN results on phrase operands are recorded. | MEAS M-BEH | Reuse the M-BEH table: `alpha THEN/1 "beta gamma"` → {1,2}, and so on |
| X5 | `(…) WITHIN N` keeps match spans of width ≤ N, where width = end − start + 1. | DOC TQ. LEAD boldi-vigna/src/interval.rs:28-30 | `(a NEAR/5 b) WITHIN 2` on `a b` (width 2) vs `a x b` (3) → {1} |
| X6 | WITHIN attaches to a primary: the grammar is `primary WITHIN n`, not `expr WITHIN n`. | LEAD grammar.pest:60 | `a NEAR/5 b WITHIN 3` → parses as `a NEAR/5 (b WITHIN 3)`? Record the difference from the parenthesized form |
| X7 | Self-proximity with a single occurrence, as in `a NEAR/2 a`. | **OPEN** | 1 `a`, 2 `a x a` / `a NEAR/2 a` → {2} only? |
| X8 | Numbers larger than u32 are rejected with an out-of-range error. | LEAD error.rs:23-26 | `a THEN/99999999999 b` → error text |

### 1.6 Span relations

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| R1 | `A ENCLOSES B` keeps A spans that contain a B span, and returns the outer span. | DOC TQ | `(a NEAR/3 c) ENCLOSES b` on `a b c` vs `a c b` |
| R2 | `A ENCLOSED BY B` tests the same relation but returns the inner span, which changes highlighting. | DOC TQ | Highlight both forms on `a b c` |
| R3 | NOT ENCLOSES and NOT ENCLOSED BY are the negated relations. | DOC TQ | `* NOT ENCLOSES spam` (MatchAll inside a span; see R8) |
| R4 | OVERLAPPING keeps A spans that share a position with B. NOT OVERLAPPING keeps those that share none. | DOC TQ | `(a NEAR/1 b) OVERLAPPING (b NEAR/1 c)` on `a b c` |
| R5 | `A BEFORE B` keeps A spans with some B that **starts** later. Spans may overlap. | DOC TQ ("start before"). LEAD boldi-vigna/src/state/relation.rs:277-282 | 1 `a b`, 2 `b a` / `a BEFORE b` → {1}; `(a NEAR/2 b) BEFORE b` on `a b` → overlap case |
| R6 | `A AFTER B` keeps A spans with some B that starts earlier. | DOC TQ. LEAD relation.rs:302-307 | Mirror of R5 |
| R7 | Every expression yields spans, so the operators compose. | DOC TQ, OV | `(AT LEAST 2 OF [a b c]) WITHIN 3` |
| R8 | `*` inside a span or position context is an error in Lead ("MatchAll (*) is not valid inside a span/positional context"), yet the product docs show `* NOT ENCLOSES spam`. | LEAD lower.rs:25-26 vs DOC TQ. **OPEN** | `* NOT ENCLOSES spam`, `* IN FIRST 3 WORDS`, `"*"`: rows or error |
| R9 | AND NOT used inside a span context lowers to span NOT-containing. | LEAD lower.rs:217-224 | `(a AND NOT b) THEN/3 c`: record TIN |

### 1.7 Positional filters

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| F1 | Positions are 0-based. `IN FIRST N WORDS` covers positions 0…N-1. | DOC TQ | Doc `a b c d e` / `c IN FIRST 2 WORDS` → no; `c IN FIRST 3 WORDS` → yes |
| F2 | **Conflict.** For `IN WORDS X TO Y`, the product docs say an inclusive 0-based range [X, Y]. Lead's docs say "positions 500 through 1000" counted from 1, and Lead's code maps `IN WORDS lo TO hi` to 0-based [lo-1, hi-1]. | DOC TQ vs LEADDOC positional-filters.md:14,99; LEAD tinql/src/runtime/position_filter.rs (Between arm: `lo.max(1)-1 … hi-1`). **OPEN, high priority** | Doc `a b c d e` / `a IN WORDS 0 TO 0`: 0-based matches, Lead does not. `c IN WORDS 2 TO 2`: 0-based matches, Lead gives position 1 (b), no match. The oracle's `rare IN WORDS 2 TO 4` does not tell the two apart |
| F3 | `IN LAST N WORDS` covers the last N token positions. | DOC TQ | `e IN LAST 1 WORDS` yes, `d IN LAST 1 WORDS` no |
| F4 | `IN FIRST N%` and `IN LAST N%` scale with document length. Lead rounds the window size up: ceil(len·N/100). | DOC TQ. LEAD position_filter.rs (`percent_count`) | A 10-token doc with the term at position 2 / `IN FIRST 25%` → window 3 (ceil 2.5) includes it; with floor it would not |
| F5 | `IN MIDDLE N%` excludes (100−N)/2 % at each end, rounding the excluded part up. `IN MIDDLE N WORDS` does not exist. | DOC TQ, LEADDOC positional-filters.md:87. LEAD position_filter.rs (Middle arm) | 10-token doc, `IN MIDDLE 50%` → window [3,6]; `x IN MIDDLE 5 WORDS` → parse error |
| F6 | The whole span must fit inside the window. | LEAD position_filter.rs (`matches_start_width`) | `"c d" IN FIRST 3 WORDS` on `a b c d` → no |
| F7 | "Document length" here is the token count. With `position_gaps=preserve`, a removed token still uses a position. | LEAD; **OPEN** for TIN | Doc with an over-long token removed (`long_tokens=discard`) followed by `x IN LAST 1 WORDS` |
| F8 | Zero and reversed ranges: `IN FIRST 0 WORDS`, `IN WORDS 5 TO 2`, `IN FIRST 0%`. | LEAD: no match. **OPEN** | Rows or error |
| F9 | `%` must follow the number with no space. | LEADDOC | `a IN FIRST 25 %` → error? |

### 1.8 Term expansions: wildcard, fuzzy, regex, range

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| E1 | `*` in a term matches zero or more characters and `?` exactly one. Both may lead, trail or sit inside the term. | DOC TQ | 1 `apple`, 2 `apply`, 3 `appl`, 4 `pineapple` / `appl*` → {1,2,3}; `appl?` → {1,2}; `*apple` → {1,4} |
| E2 | `\*` and `\?` are literal characters. | DOC TQ | `a\*b`: record tokenize/match |
| E3 | Wildcard literals are folded like terms. For example, `JALA*` matches `jalapeño`. | LEAD tinql/src/runtime/subtokenize.rs:323-340 | `JALAP*` on `Jalapeño` |
| E4 | A wildcard literal that splits (`e-mail*`) becomes a phrase whose last position is a regex. | LEAD subtokenize.rs:332-340 | `e-mail*` on `e mailbox` |
| E5 | A wildcard whose literal analyzes to nothing (`@*`) matches nothing. | LEAD lower.rs:553-556 | Error or 0 rows |
| E6 | `term~N` is fuzzy with edit distance N and a fixed prefix of 1 character. | DOC TQ | `aple~1` does **not** match `apple` under prefix 1? (a kept, delete p → yes). `pple~1` vs `apple` (prefix `p`≠`a`) → no |
| E7 | `term~P:N` sets the prefix length explicitly. | DOC TQ | `pple~0:1` matches `apple` |
| E8 | Lead's fuzzy distance is plain Levenshtein, so a transposition costs 2. | LEAD tinql/src/runtime/eval.rs:538-568. **OPEN** (Damerau?) | `teh~1` vs doc `the` → Lead no. Record TIN |
| E9 | Fuzzy operands are folded. `jalapeño~1` matches `jalapeno`. | LEAD Lead@bd95c7e:postgres/src/operator.rs tests | Record TIN |
| E10 | **Conflict.** For fuzzy on a hyphenated term, the docs say `wi-fi~2` errors because it is two words. Lead instead builds the phrase `"wi fi~2"` with the fuzzy on the last token. It errors only when the long-token policy splits a term. | DOC TQ, IDX, GS vs LEAD subtokenize.rs:51-83 | `wi-fi~1` on `wi fx`: capture the error or rows |
| E11 | Fuzzy is allowed on a single word only. It is not allowed on phrases (`"a b"~2` is phrase slop). | DOC TQ | — |
| E12 | `MATCHES regex` matches the **whole** dictionary term (anchored) and is not folded. | DOC TQ, GS. LEAD regex.rs:109-111 | `MATCHES Apple.*` → 0 rows; `MATCHES apple.*` → rows; `MATCHES ppl` vs `apple` → no (anchored) |
| E13 | A regex pattern runs to the first unescaped whitespace. `\ ` is a literal space. `)` or `]` ends the pattern unless it closes a group or class opened inside it. | DOC TQ. LEAD grammar.pest:105-121 | `(MATCHES a.*)` parses; `MATCHES foo\ bar` |
| E14 | An invalid regex is an error with the text "invalid regex …". | LEAD regex.rs:18-23 | `MATCHES (` → capture the SQLSTATE and message |
| E15 | Regex dialect: Lead uses Rust `regex` syntax, so no backreferences or lookaround and `.` matches one scalar. | LEAD. **OPEN** for TIN | `MATCHES (a)\1`: error? `MATCHES caf.` on `café` with accent_folding=preserve |
| E16 | `A TO B` is an inclusive range over the term dictionary. `* TO B` and `A TO *` are open. | DOC TQ | Docs `ant`, `bee`, `cat`, `dog` / `bee TO cat` → {bee, cat}; `* TO bee` → {ant, bee} |
| E17 | Range order is byte order of the folded terms. | LEAD eval.rs:493-501. **OPEN** | `a TO b` vs doc `ä` (folds to `a`), `B` (folds to `b`), `aé`, `9` |
| E18 | A range bound containing a wildcard is an error. | LEAD error.rs:28-31 | `a* TO c` → error |
| E19 | A range bound that analyzes to several tokens is concatenated: `wi-fi TO z` uses the bound `wifi`. A bound that analyzes to nothing is an error. | LEAD subtokenize.rs:442-460. **OPEN** | `wi-fi TO wz` vs doc `wig`; `... TO z` → error? |
| E20 | Expansions are scored by the dictionary terms they reach (see S10). | MEAS M-ARCH | — |

### 1.9 Boosts

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| W1 | `expr^N` with N in [0.0, 10000.0]. Values out of range are rejected at parse time. | DOC TQ, GS | `a^10000` OK; `a^10000.1` → error; `a^0` OK (score 0 contribution?) |
| W2 | `^` must touch the expression, with no space before it. | LEAD pest_parser/mod.rs:480-490 | `apple ^2` → error |
| W3 | The number grammar is `digits[.digits][e±digits]`. `^.5` and `^-1` are rejected. | LEAD grammar.pest:157 | `a^.5`, `a^-1`, `a^1e3` |
| W4 | Postfix modifiers compose: `apple~2^3`, `"big bad"~2^1.5`. | DOC TQ | Both parse |
| W5 | A boost changes scores, never matching. | DOC GS/SQL (usage) | Same ids for `a^5` and `a` |
| W6 | Nested boosts multiply. | LEAD Lead@bd95c7e:postgres/src/score.rs:338-340 | `(a^2)^3` → weight 6 via `score_inspect` |

---

## 2. Tokenization

The default pipeline is `tokenizer=unicode`, `case_folding=fold`,
`accent_folding=fold`, `long_tokens=split`, `max_token_bytes=256`,
`graphemes=emoji`, `position_gaps=preserve` (DOC IDX. LEAD
tokenizer/src/spec.rs:28-40). Use `SELECT * FROM tin.tokenize(x [, options])`
as the primary observation, then confirm with `==>`.

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| K1 | Documents and queries go through the same pipeline. | DOC IDX, OV | — |
| K2 | Unicode word boundaries (UAX #29) apply. Lead uses `unicode_words`. | DOC OV ("Unicode word boundaries"). LEAD tokenizer/src/tokenizers/unicode.rs:119 | `tokenize('3.14 can''t wi-fi example.com e-mail U.S.A. 1,000 foo_bar @user #tag')` → record the list |
| K3 | Case folding. Lead uses `char::to_lowercase`, which is not full case folding, so `ß` stays `ß`. | DOC (fold). LEAD tokenizer/src/folder.rs:85-97. **OPEN** | `tokenize('Straße STRASSE İstanbul ΣΊΣΥΦΟΣ')` |
| K4 | Accent folding: NFD, strip combining marks, NFC. Lead applies no compatibility normalization (NFKC). | DOC (fold). LEAD folder.rs:10-17, 99-160. **OPEN** | `tokenize('Jalapeño ﬁne Ｆｕｌｌ ① x²')`: ligature, fullwidth, circled, superscript |
| K5 | NFC and NFD input produce the same term. | LEAD (NFD→NFC) | Doc `café` (NFC), query `café` (NFD) → match |
| K6 | Emoji are searchable terms by default. | DOC OV, IDX, GS | Doc `I ❤️ 😀 👩‍💻`, query `😀` → match; `tokenize('👩‍💻 🇺🇸 ❤️')` |
| K7 | `graphemes=retain` also keeps standalone symbols and marks; `discard` drops them. | DOC IDX. LEAD unicode.rs (RetainGraphemes) | `tokenize('€ © ✓ 😀', graphemes=>'retain'/'discard')` |
| K8 | `tokenizer=whitespace` splits on whitespace only. Punctuation stays in the token, and folding still applies. | DOC IDX. LEAD tokenizers/whitespace.rs | `tokenize('Hello, World! wi-fi', tokenizer=>'whitespace')` → `hello,` `world!` `wi-fi`? |
| K9 | CJK: UAX #29 without a dictionary gives one token per Han ideograph. Kana runs behave differently. | LEAD (unicode-segmentation). **OPEN** | `tokenize('東京タワー ひらがな 한국어')`; phrase `"東京"` vs doc |
| K10 | Numbers: `3.14` and `1,000` stay one token each (UAX #29 MidNum). | LEAD. MEAS (`3.14` agreed in the oracle) | `tokenize('3.14 1,000 1.2.3 -5')` |
| K11 | An apostrophe inside a word keeps the word whole, so `can't` is one token. Curly `’` also counts as MidLetter. | LEAD. MEAS (oracle `can't`) | `tokenize('can''t can’t rock''n''roll')` |
| K12 | No stemming. | DOC OV (comparison table) | Doc `running`, query `run` → no match |
| K13 | No index-time stop words. `score_stop_words` affects scoring only. | DOC IDX | `the` is searchable |
| K14 | `max_token_bytes` (default 256, range [4, 2692]) counts UTF-8 bytes **after** folding. `long_tokens=split` cuts on grapheme boundaries, `truncate` keeps the prefix, `discard` drops the token. | DOC IDX. LEAD Lead@bd95c7e:postgres/src/udfs.rs:23, tokenizer/src/long_tokens.rs | 300 × `a`: split → 256+44 (two positions?), truncate → 1 token, discard → 0. `max_token_bytes=3` → error |
| K15 | With `position_gaps=preserve`, a removed token still uses a position. With `collapse`, it does not. | DOC IDX | `x <long> y` with discard: `"x y"` matches only under collapse |
| K16 | Changing tokenizer options needs REINDEX. Stored rows are not retokenized. | DOC IDX, LIM | After `ALTER INDEX SET (case_folding=preserve)`, old rows vs new rows |
| K17 | Invalid option values give errors naming the allowed set. | LEAD Lead@bd95c7e:postgres/src/udfs.rs:66-105 | `tokenize('x', tokenizer=>'icu')` → message and SQLSTATE |
| K18 | `tin.tokenize(NULL)` returns no rows. | LEAD udfs.rs:152 | Row count 0 |
| K19 | Query terms that analyze to nothing match nothing: `''`, whitespace, bare punctuation, and a lone emoji under `graphemes=discard`. | DOC TQ, OP | `''`, `'   '`, `'!!!'` → count 0, no error |

---

## 3. Scoring

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| S1 | BM25 with defaults k1=1.2 and b=0.75. The per-index `WITH (k1, b)` settings apply, and `ALTER INDEX … SET` takes effect with no rebuild. | DOC SC, IDX | Score bits before and after `ALTER INDEX SET (k1=2)` |
| S2 | Domains: k1 ∈ [0, 10000] and b ∈ [0, 1], checked at DDL time and for runtime overrides. | DOC IDX, SC, GS | `WITH (b=1.5)` → error; `score(ctid, k1=>-1)` → error |
| S3 | Lead's idf = ln(1 + (N − df + 0.5)/(df + 0.5)), floored at 0. Computed in f64, then cast to f32. | LEAD Lead@bd95c7e:postgres/src/bm25.rs:290-296. MEAS M-ARCH: Lead's arithmetic matched TIN bit-for-bit | Single-term bits on a 5-doc corpus; compare with the formula |
| S4 | Per-term score = (idf·boost·tf·(k1+1)) / (tf + k1(1−b) + (k1·b/avgdl)·dl), in f32 with fixed operation order. Terms are summed left to right in lexical order. | LEAD bm25.rs:324-377, 233-278 | Bits for `a OR b` vs `b OR a` → identical |
| S5 | **TF is quantized into 16 buckets** with representatives 1, 2, 3, 5, 10, 20, 41, 85, 177, 371, 777, … So tf 3 and 4 score the same, and tf 5–9 score the same. | LEAD Lead@bd95c7e:postgres/src/tf_bucket.rs:27-45. MEAS M-ARCH (bucket agreement) | Docs of equal length with `w` repeated 3 vs 4 times → equal scores; 4 vs 5 → different |
| S6 | Document length is the analyzed token count. avgdl is over indexed documents. | LEAD score.rs:192-196, 214-222 | Two docs with the same tf and different padding |
| S7 | Documents with no tokens (empty, `...`) are **not** counted in N or avgdl on TIN. Upstream Lead still counts them. NULLs are excluded everywhere. | MEAS M-ARCH. LEAD score.rs:266 (`IS NOT NULL` only) | Add 5 empty rows: scores unchanged on TIN? |
| S8 | Dense elision: `tin.score` drops terms with df ≥ dense_ratio·N (default 0.10) unless they are pinned by an explicit boost (even `^1.0`) or by `term_add`. A match with no scored term scores exactly 0.0. `dense_ratio > 1` disables elision. | DOC SC | 100 docs: terms in 9, 10 and 11 docs (see S9) |
| S9 | **Exact 10% boundary.** The docs say "10% or more" is dense, but TIN 1.0.2 kept a term at exactly 10% and elided it at 10.1%. The Lead code shows why: the f32 0.1 widened to f64 is 0.10000000149…, so df = 0.1·N falls just below the threshold. | DOC SC vs MEAS M-CAT, LEAD bm25.rs:101-105 | df=10/N=100 → retained? df=11 → elided. The oracle's boundary case expects `ten` to be elided; recheck it against TIN |
| S10 | Elision uses **immutable-segment** statistics only. The mutable segment's documents do not count. | LEAD bm25.rs:99-105 (comment "immutable-only statistics"). **OPEN** | Create the index on an empty table, insert 20 rows sharing `w`: is `w` elided before and after `tin.promote`? |
| S11 | `tin.full_score` scores every term. It ignores both elision and `score_stop_words`. | DOC SC | On `the AND midnight`: full_score > score |
| S12 | Repeated query occurrences add their boosts. Duplicates in flat AND/OR are removed first. | LEAD bm25.rs:233-258, lower.rs:12 | `score_inspect(idx,'a OR a^2')` weight? `a a` weight? |
| S13 | Expansions (`ra*`, `MATCHES`, ranges, fuzzy) score each dictionary term they reach, at the node's boost. Lead scores only the fuzzy literal. | MEAS M-ARCH vs LEAD score.rs:318, 341 | `appl*` on apple and apply docs: nonzero full_score. `appl*^2` doubles each term |
| S14 | A boolean `NOT` removes its subtree from scoring. Negative span relations still score both sides. | MEAS M-ARCH vs LEAD score.rs:337 (Lead scores negated terms) | `score_inspect(idx,'a AND NOT b')` → {a}; `a NOT OVERLAPPING b` → {a, b} |
| S15 | `term_add` adds analyzed terms, pinned at weight 1.0. `term_replace` makes them the whole scored set. Using both is an error. Neither changes matching. | DOC SC. LEAD bm25.rs:139-176 | `score(ctid, term_add=>ARRAY['Gamma'])` (analyzed to `gamma`); conflict → error text |
| S16 | Every call in one statement must pass the same `dense_ratio`, `term_add` and `term_replace`. k1 and b may differ. Runtime arguments must be constant within the statement. | DOC SC | `score(ctid, dense_ratio=>0.1), score(ctid, dense_ratio=>0.2)` → error; different k1 → OK; `score(ctid, k1=>id::real)` → error |
| S17 | `score` and `full_score` cannot be used on the same scanned relation. | DOC SC, MEAS M-ARCH | Both in one SELECT → error |
| S18 | Scoring requires a TIN scan in the same query. Otherwise it raises "requires a tin index scan and cannot be used in this query context". This covers DML `RETURNING` without a scan, and the functions never return NULL instead. | DOC SC, GS. LEAD score.rs:57-59 | `SELECT tin.score(ctid) FROM t` → error. `UPDATE … WHERE body ==> 'x' RETURNING tin.score(ctid)` → ? |
| S19 | `tin.max_score` is the highest actual score among visible matches. It is constant across rows and follows whichever policy (score or full_score) the query uses. Used alone it uses the full policy. | DOC SC, FN. MEAS M-ARCH | `score/max_score` = 1.0 for the top row; max_score alone equals max(full_score) |
| S20 | With multiple `==>` predicates on different TIN columns, the score **sums** relevance across the columns, for both AND and OR. Lead scores only the first bound column. | DOC SC, SQL, GS vs LEAD score.rs:590-595 | `title ==> 'x' OR body ==> 'x'`: a row with both > a row with one |
| S21 | Two `==>` predicates on the **same** column are combined as `(q1) OR (q2)` for scoring. | LEAD score.rs:622-629. **OPEN** | `body ==> 'a' AND body ==> 'b'` score vs `body ==> 'a b'` |
| S22 | In a join, each side scores through its own ctid. A side without a `==>` predicate makes the query be refused. | DOC SQL | `tin.score(a.ctid)` where `a` has no predicate → error |
| S23 | Statistics include dead rows until a segment rewrite or REINDEX. Visible rows alone are scored and compete for `LIMIT`. | DOC SC (Visibility), MEAS M-ARCH (bit-for-bit table) | Delete 2 of 5 → scores unchanged; after REINDEX they change |
| S24 | Under `FOR UPDATE` or `FOR SHARE`, concurrently updated rows may get a NULL score. | DOC SC | Two-session test (low priority) |
| S25 | Scores do not depend on the execution plan. | DOC OV | Same bits with `tin.enable_custom_scan` on and off, and with each `tin.debug_force_*` value |
| S26 | On a partitioned table, each partition has its own statistics. | DOC LIM | Two partitions with different df → different scores for the same text |
| S27 | Ties in `ORDER BY score DESC LIMIT k` are undocumented. Top-k must return the exhaustive top. | MEAS M-BEH (k1=0 ulp case) | Ten equal-scoring rows, LIMIT 3: record ids; add `, id` as a tiebreak for determinism |
| S28 | `score_inspect(index, query, dense_ratio, term_add, term_replace)` returns (term, weight) after stop words and elision. It requires SELECT on the table. NULL array elements are an error. | DOC SC. LEAD score.rs:346-424 | The DOC example `common^1.0 OR mid OR rare` with ratio 0.25 |
| S29 | `score_stop_words` entries are exact analyzed terms, comma-separated, trimmed and never tokenized. | DOC IDX. LEAD bm25.rs:113-122 | `WITH (score_stop_words='The')` does not stop `the`; `'the'` does |

---

## 4. Highlighting

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| H1 | `tin.highlight(text, begin_tag='<b>', end_tag='</b>', query=NULL)`. When the query is omitted, the `==>` predicate on the same column is used, found anywhere in the statement, including subqueries, CTEs, RETURNING, MERGE and ON CONFLICT. | DOC HL | Implicit and explicit forms give equal output |
| H2 | Only the text that produced the match is marked. | DOC HL | `a AND NOT b` marks only `a` |
| H3 | **Conflict** for BEFORE. The prose says only the qualifying B occurrences are wrapped. The example `highlight('b a b', query=>'a BEFORE b')` wraps both `a` and the trailing `b`. Lead marks both sides (the witnessing b and a). | DOC HL (internal contradiction). LEAD tinql/src/runtime/eval.rs:580-600 | Exactly that example |
| H4 | A multi-token span, such as a phrase, is wrapped once from its first token to its last, including the punctuation between them. | LEAD Lead@bd95c7e:postgres/src/highlight.rs:251-290 | `"fuji apple"` on `Fuji, apple!` → `<b>Fuji, apple</b>!` |
| H5 | Overlapping or touching matches merge into one tag, so tags never nest. Lead merges only byte-adjacent or overlapping ranges, so terms separated by a space stay separate. | DOC HL. LEAD highlight.rs:307-327 | `fuji OR apple` on `fuji apple` → `<b>fuji</b> <b>apple</b>`; `"fuji apple" OR apple` → one tag |
| H6 | `$QUERY_PART` in `begin_tag` becomes the HTML-escaped query part that matched. For a wildcard it is the pattern (`MATCHES email.*`). Merged parts are joined with `"; "` in Lead. | DOC HL. LEAD highlight.rs:333-345 | `highlight('send email now','<b title="$QUERY_PART">','</b>','email*')` → DOC output |
| H7 | `$QUERY_LABEL` gives a CSS-safe label: lower case, punctuation runs collapsed to `-`. Lead maps non-ASCII to `-`, uses `qpart` when the label is empty, prefixes `qpart-` when it starts with a digit, and joins multiple labels with a space. | DOC HL. LEAD highlight.rs:443-463 | Queries `Apple`, `"fuji apple"`, `3d`, `café` |
| H8 | The document text is **not** HTML-escaped. Only placeholders are escaped. | LEAD highlight.rs:212-236. **OPEN** | Doc `<script>apple</script>` → raw `<`? |
| H9 | `tin.highlight(NULL, …)` returns NULL. | LEAD Lead@bd95c7e:postgres/src/highlight_udfs.rs:28-37 | — |
| H10 | With no explicit query and no bindable predicate, the call errors: "requires an explicit query or a matching tin index scan". | LEAD highlight_udfs.rs:22-24 | `SELECT tin.highlight('x')` |
| H11 | An **invalid** explicit query leaves the text unchanged in Lead, with no error. | LEAD highlight.rs:165-169. **OPEN** | `highlight('a b','<b>','</b>','a AND')` |
| H12 | A document with no match comes back unchanged. | LEAD highlight.rs:199-201 | Explicit query that does not match |
| H13 | `tin.highlight_ansi(text, wrap_to=NULL, query=NULL)` uses ANSI colors. Lead colors a single term with `ESC[1;3Xm` and a span with a background color. Named colors apply when the query part is a color word. Otherwise the color comes from a hash. Each line inside a highlight is closed with `ESC[0m`. | DOC HL. LEAD highlight.rs:23-60, 351-420 | Byte-exact capture on `red apple` for `red` and `apple`, and on a phrase |
| H14 | `wrap_to` > 0 reflows the text on whitespace before highlighting. `wrap_to` ≤ 0 is an error in Lead. | DOC HL. LEAD highlight_udfs.rs:48, highlight.rs:132-160 | `wrap_to=>10`, `0`, `-1` |
| H15 | Highlighting uses the index's tokenizer. TIN binds to the index. Lead always uses the default pipeline. | MEAS M-ARCH (binding) vs LEAD highlight.rs:113,125 | Index with `case_folding=preserve`: query `Apple` on `apple Apple` |
| H16 | Byte offsets come out right for multi-byte text and emoji. | LEAD | `café 😀 naïve` with query `naive` → `<b>naïve</b>` |
| H17 | `ENCLOSES` highlights the outer span and `ENCLOSED BY` the inner one. | DOC TQ | See R2 |

---

## 5. Index creation, options, settings

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| I1 | `CREATE INDEX … USING tin (col)` accepts `text` or `citext` columns, or an expression that produces text. Only one column is allowed. | DOC IDX, LIM | `USING tin (a, b)` → error; `USING tin (int_col)` → error |
| I2 | Partial and expression indexes are supported. The query must repeat the indexed expression exactly, and a partial index is used only when its predicate is implied. | DOC IDX | `(data->>'title') ==> 'x'` uses the index; `lower(body)` index vs `body ==>` |
| I3 | `CREATE INDEX CONCURRENTLY` and `REINDEX [CONCURRENTLY]` are supported. | DOC IDX, LIM | Smoke test |
| I4 | Options and their domains: k1 [0, 1e4]; b [0, 1]; score_stop_words (text); tokenizer, case_folding, accent_folding, long_tokens, max_token_bytes [4, 2692], graphemes, position_gaps; initial_segment_count and target_segment_count [1, 4096]; max_mutable_segment_size ≥ 131072 (default 4 MiB; folding also happens every 16,384 docs); max_merged_segment_size ≥ 100 MB (default 2000); dead_percent_threshold [0, 1] (default 0.5). | DOC IDX | Each endpoint and one step outside it: accepted or error, with message and SQLSTATE |
| I5 | Lead accepts `initial_segment_count` only in 1..1024 and ignores it. It does not register the other four segment options, so DDL that sets them fails on Lead. | LEAD Lead@bd95c7e:postgres/src/options.rs:113-121 | Portability note for the suite: skip on Lead |
| I6 | Without an index, `==>` runs as an ordinary filter using the default tokenizer. | MEAS M-ARCH | Table with no TIN index: `'Apple' ==> 'apple'` → true |
| I7 | On an index with non-default tokenization, a plan that cannot use the TIN custom scan errors with "queries targeting a tin index with non-default tokenization require a usable tin custom scan path". | MEAS M-ARCH (1.0.2). **OPEN** for 1.0.3 | `case_folding=preserve` index, `SET tin.enable_custom_scan=off` → error? |
| I8 | The database encoding must be UTF8 or SQL_ASCII; `CREATE EXTENSION` refuses others. | DOC GS | Low priority (needs its own DB) |
| I9 | Settings: `tin.enable_custom_scan` (on), `index_maintenance_mode` (background/foreground/manual), `build_io_concurrency` (0), `rss_baseline`, `maintenance_jobs_per_db` (0; a session SET is rejected), `track_page_reuse_stats` (off). The `debug_*` settings never change results. | DOC SET | `SET tin.maintenance_jobs_per_db=1` → error. Every `debug_force_*` value gives the same ids and score bits (see S25) |
| I10 | A user can set `tin.debug_force_*`. These choose plans, not results. | DOC SET, MEAS M-BEH | Plan-equivalence matrix |
| I11 | The index relation never shrinks except through REINDEX. | DOC LIM | Not a correctness test |

---

## 6. `==>`, counts, top-k, and SQL shapes

| # | Behavior | Source | Test ideas |
| --- | --- | --- | --- |
| C1 | `text ==> text` returns a boolean. The right side may be a parameter (`$1`) or a column of an outer query (LATERAL). | DOC OP, SQL | Prepared statement: custom vs generic plan (`plan_cache_mode`) gives the same ids |
| C2 | `col ==> ANY(ARRAY[…])` matches if any element matches. | DOC OP | `ANY(ARRAY['apple','grape'])` = `apple OR grape` (ids). With a NULL element? Empty array → 0 rows? |
| C3 | `==>` combines with ordinary SQL predicates through AND and OR. OR with a btree predicate returns the union without duplicates. | DOC OP, SQL | `body ==> 'x' OR id = 3` where row 3 also matches → counted once |
| C4 | `count(*)` over `==>` is exact and answered from the index. It stays exact after deletes, before and after VACUUM, and on replicas. | DOC OV, SQL, OPS | Count after `DELETE` (no VACUUM), after VACUUM, after INSERT, after ROLLBACK |
| C5 | `ORDER BY tin.score(ctid) DESC LIMIT k` returns the k best rows. Top-k is exact (MEAS M-BEH: the pruned LIMIT 1 equals the exhaustive top). | DOC SQL | Compare with the same query without LIMIT, taking the first k |
| C6 | LIMIT/OFFSET paging is undocumented. It must equal slices of the full ordering, modulo ties. | **OPEN** | `LIMIT 2 OFFSET 2` vs full ordering (with an id tiebreak) |
| C7 | A filter on an unindexed column is checked per candidate, and top-k must still fill k. | DOC SQL | `body ==> 'x' AND ups > 100 ORDER BY score DESC LIMIT 3` on a corpus where the top-scoring rows fail the filter |
| C8 | A correlated `LATERAL` with a `==>` bound to an outer column returns the top k for each outer row. | DOC SQL | Two outer rows with different query strings |
| C9 | `NULL ==> 'x'` and `'x' ==> NULL` are undocumented. Lead's operator function is STRICT, so both give NULL and the row is filtered. | LEAD Lead@bd95c7e:postgres/src/operator.rs:35-38 (non-Option args). **OPEN** | `SELECT NULL::text ==> 'a'`, `'a' ==> NULL::text`; `WHERE body ==> NULL` → 0 rows or error? |
| C10 | NULL documents are not indexed and never match, not even `*`. | LEAD score.rs:266. **OPEN** | Rows with NULL body / `*` and `count(*)` |
| C11 | Snapshot visibility holds: uncommitted rows are invisible to other sessions, and a transaction sees its own inserts. | BLOG, LEADBLOG | Session A inserts without committing; session B counts |
| C12 | On replicas, `40001` may be raised under replay and should be retried. | DOC LIM, OPS | Out of scope for v1 |
| C13 | A query error in `==>` surfaces as an ERROR. Lead's message is prefixed `invalid ==> query:`. | LEAD operator.rs:37 | Capture SQLSTATE and message for each malformed query (see §7) |

---

## 7. Errors and SQLSTATEs

No error reference is published. The only SQLSTATE PlanetScale documents is
**40001**, raised on replicas under replay (DOC LIM, OPS). Lead raises every
error through `pgrx::error!`, which is SQLSTATE XX000. Record TIN's SQLSTATE
and message verbatim for each case below. The Lead message is given for
comparison.

| Case | Lead message (LEAD) |
| --- | --- |
| Parse error, e.g. `apple AND` | `invalid ==> query: expected … (at byte N), found unexpected input` |
| `""` | `empty phrase (at byte 0)` |
| `[]` | `empty alternatives (at byte 0)` |
| Boost out of range | `boost factor "…" is out of range (at byte N): must be a finite value of at most 10000` |
| Number > u32 | `number "…" is out of range …` |
| Wildcard in a range bound | `range bound "…" contains a wildcard …` |
| Empty range bound | `range bound "…" sub-tokenizes into no tokens` |
| Invalid regex | `invalid regex "…": …` |
| `*` in a span context | `MatchAll (*) is not valid inside a span/positional context` |
| Score outside a scan | `tin.score() requires a tin index scan and cannot be used in this query context` (text documented in DOC SC) |
| term_add with term_replace | `tin.score(): term_add and term_replace cannot both be non-NULL` |
| Negative dense_ratio | `dense_ratio must be finite and non-negative` |
| k1/b out of range at runtime | `tin score parameters: invalid BM25 parameters` |
| Highlight with no binding | `tin.highlight() requires an explicit query or a matching tin index scan` |
| `wrap_to` ≤ 0 | `wrap_to must be positive` |
| Bad tokenize option | `invalid tokenizer value "icu"; expected unicode or whitespace` |
| `max_token_bytes` out of range | `max_token_bytes must be between 4 and 2692` |
| Session SET of `tin.maintenance_jobs_per_db` | (TIN only) |
| Non-default tokenization, no custom scan | (TIN 1.0.2) `queries targeting a tin index with non-default tokenization require a usable tin custom scan path` |

---

## 8. Documentation conflicts and Lead divergences (measure first)

1. **`IN WORDS X TO Y` numbering.** The product docs say 0-based. Lead's docs and code count from 1 (F2).
2. **Commas in `[…]`.** The product docs say "not commas". Lead treats them as separators (A2).
3. **Fuzzy on a hyphenated term.** The docs say it errors. Lead builds a phrase (E10).
4. **Scoring across columns.** The docs say scores are summed. Lead uses only the first bound column (S20).
5. **BEFORE highlighting.** The prose contradicts its own example (H3).
6. **Exact dense boundary.** The docs say ≥10%. Measured TIN keeps a term at exactly 10%, which an f32→f64 artifact explains (S9).
7. **Expansion and NOT scoring.** TIN scores expansions and drops NOT subtrees. Lead does the reverse (S13, S14).
8. **Stats lifecycle.** TIN counts dead rows until rewrite and ignores empty documents. Lead uses only visible rows and counts empty documents (S7, S23).
9. **`*` inside spans.** The docs use it in an example. Lead rejects it (R8).
10. **Reserved `IN`/`AT`/`ALL`.** The docs say they are special only in context. Lead reserves them everywhere (Q5).
11. **Tokenizer binding.** TIN binds `==>` and highlighting to the index's pipeline. Lead uses the default pipeline outside the index path (H15, I7).
12. **`initial_segment_count` domain and other segment options.** The docs give 1..4096. Lead accepts 1..1024 and lacks the other options (I5).
13. **`tin.tokenize` signature.** The docs show one argument plus named options. The live install has 8 positional arguments; confirm that named arguments work.
14. **`score` with `full_score` on one relation.** TIN rejects it. Lead allowed it (S17).

---

## 9. First-version test list (140 cases)

Priority order: the section 8 conflicts first (F-02, A-02, E-05, S-18, H-02,
S-05, S-09 to S-11, R-06, Q-11, K-12, H-14), then errors and edge cases
(Q-06, Q-13 to Q-15, N-05), then everything else by area.

Conventions:

- `T` is `t(id int primary key, body text)`, with `CREATE INDEX ti ON t USING tin(body)` created **after** the inserts.
- `PAD` means 90 extra rows `(1000+n, 'pad'||n)`. Each row holds one unique term, so small corpora have N=100 and ordinary terms are not dense.
- Default query: `SELECT id FROM t WHERE body ==> Q ORDER BY id`.
- `bits(x)` means `float4send(x)`.
- Capture keys:
  - `ids`: sorted ids
  - `count`: `count(*)`
  - `rank`: ids ordered by score DESC, id
  - `bits`: score bits per id
  - `hl`: highlight text
  - `err`: SQLSTATE and message
  - `tok`: the token list
  - `inspect`: `score_inspect` rows

| id | area | corpus (id: body) | query / SQL | capture |
| --- | --- | --- | --- | --- |
| Q-01 | syntax | 1 `Apple pie`, 2 `grape` | `APPLE` | ids {1} |
| Q-02 | syntax | 1 `cats and dogs`, 2 `cats dogs` | `cats and dogs`; `cats AND dogs` | ids ({1}; {1,2}) |
| Q-03 | syntax | 1 `apple grape`, 2 `apple` | `apple grape` | ids {1} |
| Q-04 | syntax | same | `apple OR grape` | ids |
| Q-05 | syntax | 1 `apple peel`, 2 `apple` | `apple AND NOT peel` | ids {2} |
| Q-06 | syntax | same | `NOT peel` | err |
| Q-07 | syntax | same | `apple -peel` | ids or err |
| Q-08 | syntax | 1 `a`, 2 `b`, 3 `c`, 4 `b c`, 5 `a d` | `a OR b THEN/5 c AND d` vs `a OR ((b THEN/5 c) AND d)` | ids equal |
| Q-09 | syntax | 1 `a b c`, 2 `a b`, 3 `a c` | `a b AND NOT c` | ids {2} |
| Q-10 | syntax | 1 `and or not` | `"AND"`; `SELECT tin.maybe_quote('AND'), maybe_quote('beer'), maybe_quote('foo*bar'), maybe_quote('a b')` | ids; values |
| Q-11 | syntax | 1 `in at all first of by` | each of `IN`, `AT`, `ALL`, `FIRST`, `OF`, `BY` | ids or err per query |
| Q-12 | syntax | 1 `apple`, 2 `''`, 3 `...`, 4 NULL | `*`; `SELECT count(*) … ==> '*'` | ids, count |
| Q-13 | syntax | 1 `apple` | `''`, `'   '`, `'!!!'` | count 0, no err |
| Q-14 | syntax | 1 `apple` | `""`, `[]`, `"   "` | err each |
| Q-15 | syntax | 1 `apple` | `apple AND`, `(apple`, `apple)`, `apple ^2` | err each |
| Q-16 | syntax | 1 `wi-fi`, 2 `wi fi`, 3 `fi wi`, 4 `wifi` | `wi-fi` | ids {1,2} |
| Q-17 | syntax | 1 `visit example.com today`, 2 `example` | `example.com` | ids |
| Q-18 | syntax | 1 `apple` | `CONTAINS apple` | ids {1} |
| Q-19 | syntax | 1 `a b c` | `a BEFORE b BEFORE c` | ids or err |
| Q-20 | syntax | generate 3,000 distinct words in one doc | a 3,000-word AND query; a 3,000-term OR; 1,000 nested parens | ids (must not error) |
| P-01 | phrase | 1 `fuji apple`, 2 `apple fuji`, 3 `fuji red apple` | `"fuji apple"` | ids {1} |
| P-02 | phrase | 1 `big bad wolf`, 2 `big wolf`, 3 `big very bad wolf` | `"big _ wolf"`; `"big __ wolf"` | ids |
| P-03 | phrase | 1 `a b x c`, 2 `a x b x x c`, 3 `a x b x c` | `"a _ b __ c"` | ids {2} |
| P-04 | phrase | 1 `fuji red apple`, 2 `fuji x y z apple`, 3 `apple fuji` | `"fuji apple"~2`; `~3` | ids |
| P-05 | phrase | 1 `a c`, 2 `a x c`, 3 `a x y c`, 4 `a x y z c` | `"a _ c"~1` | ids {2,3} |
| P-06 | phrase | 1 `big bad wolf`, 2 `big large wolf`, 3 `big wolf` | `"big [bad large] wolf"` | ids {1,2} |
| P-07 | phrase | 1 `alpha beta`, 2 `alpha gamma` | `"alpha [MATCHES b.*]"` | ids {1} |
| P-08 | phrase | 1 `foo_bar`, 2 `foo x bar`, 3 `foo bar` | `"foo_bar"`; `"foo\_bar"`; `foo_bar` | ids each; tok(`foo_bar`) |
| P-09 | phrase | 1 `alpha, beta`, 2 `alpha\nbeta`, 3 `beta alpha` | `"alpha beta"` | ids {1,2} |
| P-10 | phrase | 1 `apple` | `"apple"` vs `apple` | ids and bits equal (PAD) |
| P-11 | phrase | 1 `apple` | `"..."` | count 0, no err |
| A-01 | alternatives | 1 `beer`, 2 `wine`, 3 `beer wine`, 4 `water` | `[beer wine]` | ids {1,2,3} |
| A-02 | alternatives | same | `[beer,wine]`; `[beer, wine]` | ids (comma conflict) |
| A-03 | alternatives | 1 `47,000`, 2 `48,000` | `[47,000 48,000]` | ids |
| A-04 | alternatives | docs with 1, 2, 3 or 4 of `a b c d` (ids 1–4) | `AT LEAST 2 OF [a b c d]` | ids {2,3,4} |
| A-05 | alternatives | 1 `a`, 2 `a b`, 3 `a b c` | `AT LEAST 50% OF [a b c]`; `34%`; `33%` | ids (ceil) |
| A-06 | alternatives | same | `ALL OF [a b c]` | ids {3} |
| A-07 | alternatives | same | `AT LEAST 5 OF [a b]`; `AT LEAST 0 OF [a b]` | ids or err |
| A-08 | alternatives | 1 `a b`, 2 `c` | `[a AND b c]` | ids {1,2} |
| X-01 | proximity | 1 `a b`, 2 `a x b`, 3 `b a`, 4 `a x y b` | `a THEN/0 b`; `a THEN/1 b` | ids |
| X-02 | proximity | same | `a NEAR/0 b`; `a NEAR/1 b` | ids |
| X-03 | proximity | same | `a THEN b` | err |
| X-04 | proximity | M-BEH corpus: 1 `alpha x beta gamma`, 2 `alpha beta gamma`, 3 `beta gamma alpha`, 4 `alpha x y beta gamma` | `alpha THEN/1 "beta gamma"`; `"beta gamma" THEN/1 alpha`; `alpha NEAR/1 "beta gamma"` | ids, rank |
| X-05 | proximity | 1 `a b`, 2 `a x b` | `(a NEAR/5 b) WITHIN 2`; `a NEAR/5 b WITHIN 2` | ids (WITHIN binding) |
| X-06 | proximity | 1 `a`, 2 `a x a` | `a NEAR/2 a` | ids |
| R-01 | relations | 1 `a b c`, 2 `a c b` | `(a NEAR/3 c) ENCLOSES b` | ids {1} |
| R-02 | relations | 1 `a b c` | `b ENCLOSED BY (a NEAR/3 c)` with `tin.highlight(body)`; the same with ENCLOSES | hl each |
| R-03 | relations | 1 `a b c`, 2 `a c` | `(a NEAR/3 c) NOT ENCLOSES b` | ids {2} |
| R-04 | relations | 1 `a b c`, 2 `a b x x c` | `(a NEAR/1 b) OVERLAPPING (b NEAR/1 c)`; `NOT OVERLAPPING` | ids |
| R-05 | relations | 1 `a b`, 2 `b a`, 3 `b a b` | `a BEFORE b`; `a AFTER b` | ids |
| R-06 | relations | 1 `spam eggs`, 2 `eggs` | `* NOT ENCLOSES spam`; `* IN FIRST 3 WORDS` | ids or err |
| F-01 | positions | 1 `a b c d e` | `c IN FIRST 2 WORDS`; `c IN FIRST 3 WORDS` | ids ({}; {1}) |
| F-02 | positions | 1 `a b c d e` | `a IN WORDS 0 TO 0`; `c IN WORDS 2 TO 2`; `a IN WORDS 1 TO 1` | ids (0- vs 1-based) |
| F-03 | positions | 1 `a b c d e` | `e IN LAST 1 WORDS`; `d IN LAST 1 WORDS` | ids |
| F-04 | positions | 1 `x0 x1 t x3 x4 x5 x6 x7 x8 x9` (t at position 2) | `t IN FIRST 25%`; `t IN FIRST 20%` | ids (ceil rounding) |
| F-05 | positions | 10-token doc with `m` at position 3, another with `m` at position 2 | `m IN MIDDLE 50%` | ids |
| F-06 | positions | 1 `a b c d` | `"c d" IN FIRST 3 WORDS`; `"c d" IN FIRST 4 WORDS` | ids |
| F-07 | positions | 1 `a b` | `a IN FIRST 0 WORDS`; `a IN WORDS 5 TO 2`; `a IN MIDDLE 5 WORDS` | ids or err |
| F-08 | positions | 1 `beer x wine`, 2 `x x beer wine` | `beer IN FIRST 2 WORDS AND wine`; `(beer AND wine) IN FIRST 2 WORDS` | ids |
| E-01 | expansion | 1 `apple`, 2 `apply`, 3 `appl`, 4 `pineapple` | `appl*`; `appl?`; `*apple`; `a*e` | ids |
| E-02 | expansion | 1 `Jalapeño` | `JALAP*` | ids {1} |
| E-03 | expansion | 1 `apple`, 2 `ample` | `aple~1`; `pple~1`; `pple~0:1`; `apple~0` | ids |
| E-04 | expansion | 1 `the` | `teh~1`; `teh~2` | ids (Levenshtein vs Damerau) |
| E-05 | expansion | 1 `wi fx`, 2 `wi fi` | `wi-fi~1` | ids or err |
| E-06 | expansion | 1 `apple`, 2 `Apple` | `MATCHES Apple.*`; `MATCHES apple.*`; `MATCHES ppl` | ids |
| E-07 | expansion | 1 `apple` | `MATCHES (`; `MATCHES (a)\1` | err |
| E-08 | expansion | 1 `ant`, 2 `bee`, 3 `cat`, 4 `dog` | `bee TO cat`; `* TO bee`; `cat TO *`; `cat TO bee` | ids |
| E-09 | expansion | 1 `ä`, 2 `b`, 3 `9`, 4 `aé` | `a TO b` | ids (byte order after folding) |
| E-10 | expansion | 1 `apple` | `a* TO c`; `... TO z` | err |
| K-01 | tokenize | — | `SELECT array_agg(t) FROM tin.tokenize('3.14 can''t wi-fi example.com e-mail U.S.A. 1,000 foo_bar @user #tag') t` | tok |
| K-02 | tokenize | — | `tokenize('Straße STRASSE İstanbul ΣΊΣΥΦΟΣ')` | tok |
| K-03 | tokenize | — | `tokenize('Jalapeño ﬁne Ｆｕｌｌ ① x²')` | tok |
| K-04 | tokenize | 1 `café` (NFC) | query `E'café'` (NFD); `cafe` | ids |
| K-05 | tokenize | 1 `I ❤️ 😀 👩‍💻 🇺🇸` | `😀`; `👩‍💻`; tokenize with graphemes emoji/retain/discard | ids, tok |
| K-06 | tokenize | — | `tokenize('東京タワー ひらがな 한국어')` | tok |
| K-07 | tokenize | 1 `東京タワー` | `"東京"`; `東` | ids |
| K-08 | tokenize | — | `tokenize('Hello, World! wi-fi', tokenizer=>'whitespace')` | tok |
| K-09 | tokenize | 1 `running` | `run` | ids {} (no stemming) |
| K-10 | tokenize | — | `tokenize(repeat('a',300))` with long_tokens split/truncate/discard; `max_token_bytes=>3` | tok; err |
| K-11 | tokenize | index `WITH (long_tokens=discard, max_token_bytes=8, position_gaps=preserve)`; 1 `x aaaaaaaaaaaa y` | `"x y"`; the same with `collapse` | ids |
| K-12 | tokenize | index `WITH (case_folding=preserve)`; 1 `apple`, 2 `Apple` | `Apple`; again with `SET tin.enable_custom_scan=off` | ids; err? |
| K-13 | tokenize | — | `tokenize('x', tokenizer=>'icu')`; `tokenize(NULL)` | err; row count |
| S-01 | scoring | 1 `a`, 2 `a b`, 3 `b` + PAD | `a OR b` with `full_score` and `score` | bits, rank |
| S-02 | scoring | 1 `w w w x y` (tf 3, 5 tokens), 2 `w w w w x` (tf 4, 5 tokens) + PAD | `w` full_score | bits equal (tf 3 and 4 share a bucket) |
| S-03 | scoring | 1 `w w w w x` (tf 4), 2 `w w w w w` (tf 5), both 5 tokens + PAD | `w` full_score | bits differ (bucket boundary at 5) |
| S-04 | scoring | 5 docs: `rare common`, `common`, `common`, `rare x`, `common y` | `rare OR common` full_score | bits (compare with the M-ARCH table) |
| S-05 | scoring | the oracle BOUNDARY_FIXTURE (100 docs; `nine` ⊂ 9, `ten` ⊂ 10, `eleven` ⊂ 11) | `score` for each term; `nine^1`; `eleven` with `dense_ratio=>1.1` | bits, zero or not |
| S-06 | scoring | same | `inspect('…','nine OR ten OR eleven')`; with `0.25` | inspect |
| S-07 | scoring | index created on an empty T, then 20 rows `w k<n>` | `score(ctid)` for `w` vs `full_score`, before and after `tin.promote` | bits (mutable stats) |
| S-08 | scoring | 1 `the midnight`, … + PAD, with `the` in >10% | `the AND midnight` with score vs full_score | bits |
| S-09 | scoring | 1 `apple`, 2 `apply` + PAD | `appl*` full_score; `appl*^2` | bits nonzero; ×2 |
| S-10 | scoring | 1 `ant`, 2 `bee` + PAD | `MATCHES a.*`; `ant TO bee`; `ant~1` full_score | bits nonzero? |
| S-11 | scoring | 1 `a`, 2 `a b` + PAD | `inspect('a AND NOT b')`; `inspect('a NOT OVERLAPPING b')` | inspect |
| S-12 | scoring | 1 `a b` + PAD | `inspect('a OR a^2')`; `inspect('a a')`; `inspect('(a^2)^3')` | inspect weights |
| S-13 | scoring | 1 `alpha gamma` + PAD | `score(ctid, term_add=>ARRAY['Gamma'])`; `term_replace=>ARRAY['gamma']`; both together | bits; err |
| S-14 | scoring | same | two calls with different dense_ratio; different k1; `k1=>id::real` | err / ok / err |
| S-15 | scoring | same | `SELECT tin.score(ctid), tin.full_score(ctid) … WHERE body ==> 'alpha'` | err |
| S-16 | scoring | same | `SELECT tin.score(ctid) FROM t`; `UPDATE t SET body=body WHERE body ==> 'alpha' RETURNING tin.score(ctid)` | err / bits |
| S-17 | scoring | 1 `a`, 2 `a a b` + PAD | `score/max_score`; `max_score` alone vs `max(full_score)` | bits |
| S-18 | scoring | fruits(name, notes), two indexes; 1 (`fuji`,`citrus`), 2 (`fuji`,`x`), 3 (`x`,`citrus`) + PAD | `name ==> 'fuji' OR notes ==> 'citrus'` score; the same with `fuji^1.5` | bits (sum) |
| S-19 | scoring | 1 `a b` + PAD | `body ==> 'a' AND body ==> 'b'` vs `body ==> 'a b'` | bits |
| S-20 | scoring | 5 docs as in S-04 | delete ids 2 and 3 → full_score; VACUUM → full_score; REINDEX → full_score | bits per stage |
| S-21 | scoring | 5 empty rows + S-04 corpus | `rare OR common` full_score | bits vs S-04 |
| S-22 | scoring | S-01 | same bits with `enable_custom_scan` on/off and each `debug_force_topk` value | bits equal |
| S-23 | scoring | `WITH (k1=2, b=0.3)` and `ALTER INDEX SET (k1=1.2)`; `score_stop_words='The'` vs `'the'` | `the OR x` | bits |
| S-24 | scoring | `WITH (b=1.5)`; `score(ctid, k1=>-1)`; `score(ctid, dense_ratio=>-0.1)` | — | err |
| T-01 | top-k | 20 rows with distinct tf of `w` + PAD | `ORDER BY score DESC LIMIT 5` vs no LIMIT | rank prefix equal |
| T-02 | top-k | 10 identical docs `w` + PAD | `ORDER BY score DESC LIMIT 3` | ids (tie behavior) |
| T-03 | top-k | T-01 | `ORDER BY score DESC, id LIMIT 3 OFFSET 3` | ids = slice |
| T-04 | top-k | T-01 plus an `ups` column where the top 5 have ups=0 | `AND ups > 0 ORDER BY score DESC LIMIT 3` | rank |
| T-05 | top-k | the M-BEH k1=0 corpus (1 `w w w`, 2–10 `w`) | `ORDER BY full_score(ctid,0,0.75) DESC LIMIT 1` | ids {2}; bits |
| T-06 | top-k | authors(topics) × posts | the LATERAL top-2 per author | ids per author |
| N-01 | count | T with 100 rows, half containing `x` | `count(*) ==> 'x'` → delete 10 → count → VACUUM → count | count per stage |
| N-02 | count | same | `count(*)` with `debug_disable_count_pushdown` on and off | equal |
| N-03 | count | 1 `a`, 2 `b` | `body ==> 'a' OR id = 1`; `body ==> 'a' OR id = 2` | ids (no duplicates) |
| N-04 | count | 1 `apple`, 2 `grape`, 3 NULL | `==> ANY(ARRAY['apple','grape'])`; `ANY(ARRAY['apple',NULL])`; `ANY('{}'::text[])` | ids |
| N-05 | nulls | same | `SELECT NULL::text ==> 'a'`; `'a' ==> NULL::text`; `WHERE body ==> NULL` | values / ids / err |
| N-06 | visibility | two sessions | A: `BEGIN; INSERT 'zz'`; B: `count(*) ==> 'zz'`; A: `ROLLBACK` | count 0 in B; A sees its own row |
| N-07 | prepared | 1 `a`, 2 `b` | `PREPARE p(text) AS SELECT id … ==> $1`, 6 executions with alternating params under `plan_cache_mode` force_generic/force_custom | ids |
| H-01 | highlight | 1 `I like apple pie` | `tin.highlight(body)` with `==> 'apple'` | hl `I like <b>apple</b> pie` |
| H-02 | highlight | — | `tin.highlight('b a b', query=>'a BEFORE b')` | hl (conflict) |
| H-03 | highlight | 1 `Fuji, apple!` | `"fuji apple"` implicit | hl |
| H-04 | highlight | 1 `fuji apple` | `fuji OR apple`; `"fuji apple" OR apple` | hl (merge rule) |
| H-05 | highlight | — | `highlight('send email now','<b title="$QUERY_PART">','</b>','email*')` | hl |
| H-06 | highlight | 1 `Apple 3d café fuji apple` | `highlight(body,'<i class="$QUERY_LABEL" title="$QUERY_PART">','</i>')` with `Apple OR 3d OR café OR "fuji apple"` | hl |
| H-07 | highlight | 1 `<script>apple</script> & "x"` | `apple` | hl (escaping) |
| H-08 | highlight | — | `highlight(NULL, query=>'a')`; `highlight('x')` with no predicate; `highlight('a b','<b>','</b>','a AND')` | NULL / err / hl |
| H-09 | highlight | 1 `red apple` / 2 `fuji apple` | `highlight_ansi(body)` for `red`, `apple`, `"fuji apple"` | hl bytes (hex) |
| H-10 | highlight | 1 `one two three four five apple` | `highlight_ansi(body, 10)`; `wrap_to 0`; `-1` | hl / err |
| H-11 | highlight | 1 `café 😀 naïve` | `naive` | hl `café 😀 <b>naïve</b>` |
| H-12 | highlight | 1 `urgent x` | `UPDATE t SET body=body WHERE body ==> 'urgent' RETURNING tin.highlight(body)`; CTE form; subquery form | hl |
| H-13 | highlight | 1 `a b c` | `a AND NOT z`; `(a NEAR/3 c) ENCLOSES b` | hl |
| H-14 | highlight | index `WITH (case_folding=preserve)`; 1 `apple Apple` | `highlight(body)` with `==> 'Apple'` | hl or err |
| I-01 | ddl | T | `USING tin (id, body)`; `USING tin (id)` (int) | err |
| I-02 | ddl | T | every WITH option at both endpoints and one step past each | ok / err + message |
| I-03 | ddl | T + `data jsonb` | expression index on `(data->>'title')`: query via the expression vs via a column | ids; plan node |
| I-04 | ddl | T + `active bool` | partial index; `WHERE active AND body ==> 'x'` vs `WHERE body ==> 'x'` | ids |
| I-05 | ddl | T with `citext` column | index and query `APPLE` | ids |
| I-06 | ddl | table without a TIN index | `WHERE body ==> 'apple'` | ids (plain filter) |
| I-07 | settings | — | `SET tin.maintenance_jobs_per_db = 1` | err |

Run notes:

- Record `SELECT extversion FROM pg_extension WHERE extname='tin'` with every result file.
- Pin `initial_segment_count` (for example 1) so segment layout is reproducible. It must not affect results.
- Separate scoring cases by index build: build after inserts to keep immutable statistics.
- Compare score bits exactly only when corpus statistics are identical on both engines.
- Otherwise compare `rank`.
