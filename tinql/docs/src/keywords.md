# Keywords & Special Characters

Keywords and syntax characters have the meanings listed below. Quoting and
escaping let you use them as search text.

## Keywords

All keywords are **case-sensitive** and must be written in **UPPER CASE**.
`AND` is a keyword, but `and` and `And` are ordinary search terms. This means
common English words that happen to share spelling with keywords (`to`, `and`,
`or`, `not`, `in`, `at`, `all`, `near`, etc.) work as search terms
without quoting.

Keyword recognition checks the word boundary. `THENSOME` is one term.

### Boolean keywords

| Keyword | Purpose | Example |
|---|---|---|
| `AND` | Both sides must match | `beer AND wine` |
| `OR` | At least one side must match | `beer OR wine` |
| `NOT` | Used in compound operators (see below) | `beer AND NOT bud` |

`NOT` never appears alone as an operator. It always combines with another
keyword: `AND NOT`, `NOT ENCLOSES`, `NOT ENCLOSED BY`, or `NOT OVERLAPPING`.

### Proximity keywords

| Keyword | Purpose | Example |
|---|---|---|
| `THEN/N` | Ordered proximity with gap allowance | `craft THEN/3 beer` |
| `NEAR/N` | Unordered proximity with gap allowance | `craft NEAR/5 beer` |

The `/N` suffix is required. `THEN/0` means strict adjacency (no gap allowed).

### Relation keywords

| Keyword | Purpose | Example |
|---|---|---|
| `ENCLOSES` | Left span encloses right span | `A ENCLOSES B` |
| `NOT ENCLOSES` | Left span does not enclose right span | `A NOT ENCLOSES B` |
| `ENCLOSED BY` | Left span is inside right span | `A ENCLOSED BY B` |
| `NOT ENCLOSED BY` | Left span is not inside right span | `A NOT ENCLOSED BY B` |
| `OVERLAPPING` | Spans share at least one position | `A OVERLAPPING B` |
| `NOT OVERLAPPING` | Spans share no positions | `A NOT OVERLAPPING B` |
| `BEFORE` | Left span starts before right span | `A BEFORE B` |
| `AFTER` | Left span starts after right span | `A AFTER B` |

`BY` is a keyword when it follows `ENCLOSED`. Elsewhere it is an ordinary
search term. `ENCLOSED` itself is always reserved: a bare
`ENCLOSED` is a parse error, so quote it (`"ENCLOSED"`) to search for the word.

### Range keyword

| Keyword | Purpose | Example |
|---|---|---|
| `TO` | Lexicographic range between two terms | `aardvark TO cat` |

Use `*` for an open bound: `* TO cat` or `monkey TO *`. See
[Terms](./terms.md#ranges) for details.

### Positional keywords

| Keyword               | Purpose                                | Example                     |
|-----------------------|----------------------------------------|-----------------------------|
| `IN FIRST ... WORDS`  | Match in the first N positions         | `beer IN FIRST 100 WORDS`   |
| `IN FIRST ...%`       | Match in the first N% of the document  | `beer IN FIRST 25%`         |
| `IN LAST ... WORDS`   | Match in the last N positions          | `beer IN LAST 50 WORDS`     |
| `IN LAST ...%`        | Match in the last N% of the document   | `beer IN LAST 25%`          |
| `IN MIDDLE ...%`      | Match in the middle N% of the document | `beer IN MIDDLE 50%`        |
| `IN WORDS ... TO ...` | Match in a position range              | `beer IN WORDS 500 TO 1000` |
| `WITHIN`              | Limit total span width                 | `(A NEAR/5 B) WITHIN 6`     |

`IN` only forms a positional filter when followed by `FIRST`, `LAST`,
`MIDDLE`, or `WORDS`; in any other position it is still reserved and causes a
parse error. For example, `beer IN bar` does not parse; quote `"IN"` or use
lowercase `in` to search for the word. `TO` also appears in positional filters
(`IN WORDS 500 TO 1000`).
`WITHIN` limits span width; see [Proximity](./proximity.md#within).

### Term keyword

| Keyword | Purpose | Example |
|---|---|---|
| `CONTAINS` | Explicit term match (optional) | `CONTAINS beer` |

`CONTAINS beer` means the same thing as bare `beer`. You can use it alongside
`MATCHES`:

```
CONTAINS beer AND MATCHES hop.*s
```

`CONTAINS` must be followed by a term, wildcard, or fuzzy match. Writing
`CONTAINS` with nothing after it is an error.

### Regex keyword

| Keyword | Purpose | Example |
|---|---|---|
| `MATCHES` | Full-term regex match | `MATCHES hop.*s` |

`MATCHES` must always be followed by a pattern. Writing `MATCHES` with nothing
after it is an error.

### Minimum-match keywords

| Keyword | Purpose | Example |
|---|---|---|
| `AT LEAST N OF` | Minimum count of alternatives must match | `AT LEAST 2 OF [a b c]` |
| `AT LEAST N% OF` | Minimum percentage of alternatives must match | `AT LEAST 50% OF [a b c]` |
| `ALL OF` | Every alternative must match | `ALL OF [a b c]` |

`AT` is only a keyword when followed by `LEAST`. `ALL` is only a keyword when
followed by `OF`. A bare `AT` or `ALL` causes a parse error. Use quotes
(`"AT"`, `"ALL"`) to search for either word. Lowercase `at` and `all`
are ordinary terms.

## Special characters

### Delimiters

| Character | Meaning | Context |
|---|---|---|
| `"` | Opens/closes a phrase | `"big bad wolf"` |
| `(` `)` | Grouping (override precedence) | `(A OR B) AND C` |
| `[` `]` | Alternatives list | `[beer ale lager]` |

An opening delimiter requires a matching closing delimiter.

### Operators and modifiers

| Character | Meaning | Context |
|---|---|---|
| `~` | Fuzzy suffix (after a term) | `beer~2`, `beer~0:2` |
| `~` | Phrase slop (after closing `"`) | `"big bad"~2` |
| `^` | Boost factor | `beer^2` |
| `*` | Match-all (standalone) | `*` |
| `*` | Wildcard (inside a pattern) | `brew*`, `*house` |
| `*` | Open range bound | `* TO cat` |
| `?` | Single-character wildcard | `b?er` |

### Characters inside phrases

Inside double quotes, these characters have special meaning:

| Character | Meaning | How to escape |
|---|---|---|
| `_` | Gap (one word position) | `\_` for literal underscore |
| `[` | Start of phrase alternatives | `\[` for literal bracket |
| `]` | End of phrase alternatives | `\]` for literal bracket |
| `"` | End of phrase | `\"` for literal quote |
| `\` | Escape character | `\\` for literal backslash |

### Characters in terms

Bare terms accept characters other than whitespace and `(`, `)`, `[`, `]`,
`"`, `~`, `^`. See [Terms](./terms.md) for the complete syntax and how the
index tokenizer analyzes a term.

## Escaping

The backslash `\` has different meanings depending on context:

- **In bare terms:** `\` is an escape only before `*` and `?` (`file\*` is the
  literal term `file*`; see
  [Terms → Escaping `*` and `?`](./terms.md#escaping--and-)). Before any other
  character it remains literal: `foo\bar` is a single term containing a
  backslash.
- **Inside phrases:** `\` escapes the next character, letting you include
  characters that would otherwise have special meaning.
- **Inside MATCHES patterns:** `\ ` (backslash-space) embeds a literal space.
  All other `\X` sequences pass through as regex content (`\d`, `\w`, etc.).

### Quoting search text

If your search text contains syntax characters, whitespace, or keywords,
put it in double quotes:

```
"foo(bar)"       literal text containing parentheses
"hello world"    two words as a phrase (whitespace is significant)
"THEN"           the literal word THEN, not the operator
```

### Escaping inside phrases

Inside double quotes, a backslash escapes the next character:

| Escape | Produces | Why needed |
|---|---|---|
| `\"` | literal `"` | would otherwise close the phrase |
| `\\` | literal `\` | would otherwise start an escape |
| `\_` | literal `_` | would otherwise be a gap |
| `\[` | literal `[` | would otherwise start alternatives |
| `\]` | literal `]` | would otherwise end alternatives |

Any other character after `\` is passed through as-is. For example, `\x`
produces `x`.

Examples:

```
"she said \"hello\""       phrase containing a literal quote
"score\_count"             literal underscore, not a gap
"array\[0\]"               literal brackets, not alternatives
"back\\slash"              literal backslash
```

### Escaping in MATCHES patterns

In a `MATCHES` pattern, `\ ` (backslash-space) embeds a literal space:

| Escape | Produces |
|---|---|
| `\ ` | literal space (would otherwise end the pattern) |

All other backslash sequences are passed through to the regex engine as-is,
so `\d`, `\w`, `\b`, `\\`, etc. work as you would expect.

### Interaction with SQL

Since `tin` queries are typically written inside SQL string literals, it helps
to understand how the layers interact.

In **standard SQL strings** (the default in PostgreSQL), backslash has no
special meaning. `'foo\bar'` delivers the literal text `foo\bar` to `tin`,
which treats it as a single term containing a backslash.

In **escape strings** (`E'...'`), PostgreSQL interprets `\\` as a single
backslash before passing the string along. So `E'foo\\bar'` delivers
`foo\bar`.

Use standard SQL strings when passing TinQL backslash escapes. With `E'...'`
strings, double each backslash that TinQL needs to receive.

## Using keywords as search terms

Since keywords are UPPER CASE only, **lowercase forms are already plain
terms**. You never need quoting for common English words:

```
to be or not to be     six search terms joined by implicit AND
all in                 two search terms
```

If you need the UPPER CASE form as a search term (rare), wrap it in a phrase:

```
"THEN"           searches for the literal word THEN
"AND"            searches for the literal word AND
"NOT"            searches for the literal word NOT
```

Inside phrases, all keywords are treated as ordinary terms. This works because
the phrase parser treats the words as text.

You can also embed keywords alongside other words in a phrase:

```
"and then what"         three-word phrase
"what comes after"      three-word phrase
```
