# Addendum: Parser Internals

The `tinql` crate separates syntax parsing, tokenization, and lowering to the
query engine's runtime representation.

## The pest parser

The `tinql` crate parses queries with a PEG (Parsing Expression Grammar)
parser generated from a `.pest` grammar file (`grammar.pest`). The grammar is
a concise, formal specification of the language's syntax that the pest library
compiles into a parser; a conversion layer then walks the pest parse tree and
builds the AST.

## Operator precedence parsing

Each precedence level has a grammar rule. The rule parses operands at the next
higher level and then consumes operators at its own level.

The precedence levels, from lowest (parsed first, binds loosest) to highest:

1. OR
2. AND / implicit operator
3. AND NOT
4. Positional filters (IN FIRST, IN LAST, IN MIDDLE, IN WORDS)
5. Relation operators (ENCLOSES, ENCLOSED BY, OVERLAPPING, BEFORE, AFTER)
6. Span operators (THEN, NEAR)
7. WITHIN
8. Boost (^)
9. Primary expressions (terms, phrases, alternatives, MATCHES, parenthesized groups)

## Phrase sub-parsing

Inside quotes, the parser applies phrase rules:

- **Whitespace is significant**: it separates phrase elements (terms, gaps,
  alternatives).
- **Underscores are gaps**, not term characters.
- **Square brackets start alternatives**, parsed as whitespace-separated sub-expressions.
- **Escape sequences** (`\"`, `\\`, `\_`, `\[`, `\]`) must be interpreted.

Phrase content is therefore delegated to a dedicated phrase grammar
(`phrase.pest`) with its own rules.

When phrase alternatives are encountered (e.g., `[big large]` inside quotes),
the raw text between the brackets is extracted, re-wrapped in `[...]`, and
re-parsed with the main grammar's alternatives rule, which splits it into
whitespace-separated elements. In `"[A NEAR/3 B C THEN/0 D] wolf"`, the
alternatives are the complete expressions `A NEAR/3 B` and `C THEN/0 D`.

## Keyword disambiguation

Keywords are **case-sensitive** and must be UPPER CASE. The word `THEN` in
`A THEN/0 B` is an operator, but `then` is an ordinary search term. A product
named "THENSOME" is also a regular term because the keyword boundary check
requires the keyword to end at a non-word character.

This two-layer disambiguation (case-sensitive + word-boundary) means:

- `THEN` followed by a space → keyword
- `then` followed by a space → term (lowercase)
- `THENSOME` → term (no boundary after `THEN`)
- `"THEN"` → phrase term (inside quotes, keywords are not recognized)

A bare UPPER CASE keyword that cannot form a complete syntactic construct is a
parse error (e.g., standalone `ALL` without `OF [...]`). The user must quote it
to use as a search term.

The `NOT` keyword requires special care because it appears in multiple compound
operators: `AND NOT`, `NOT ENCLOSES`, `NOT ENCLOSED BY`, `NOT OVERLAPPING`.
After seeing `NOT`, the parser looks ahead to determine which compound operator
is being used.

## Error reporting

All parse errors carry byte-offset positions into the original input string.
The error types are:

- **Expected**: the parser expected specific syntax and found something else
- **EmptyAlternatives**: `[]` with nothing inside
- **EmptyPhrase**: `""` with nothing inside
- **NumberOutOfRange**: a numeric argument (`THEN/N`, `~N`, `IN FIRST N`,
  ...) does not fit in an unsigned 32-bit integer
- **WildcardInRangeBound**: a range bound contains a wildcard
  (`ban* TO beer`); bounds must be plain terms (`*` alone is the open bound
  and is allowed)
- **BoostOutOfRange**: a boost factor is not a finite value in `[0, 10000]`

## AST roundtrip property

The AST's `Display` implementation is precedence-aware and produces minimal
parentheses. It maintains a **roundtrip property**: for any valid query Q,
parsing `Q`, formatting the AST, and parsing the result produces an identical
AST. This is verified by extensive roundtrip tests in the test suite and means
the display output is always a valid, canonical representation of the query.

## Implicit operator insertion

Implicit operators (AND or OR, depending on configuration) are inserted between
adjacent expressions that are not already connected by an explicit operator.
The parser must avoid inserting implicit operators in several cases:

- Before or after explicit keywords (AND, OR, THEN, NEAR, etc.)
- Before closing delimiters (`)`, `]`)
- After opening delimiters (`(`, `[`)
- At end of input

The parser checks the next token before deciding whether to insert an implicit
operator and parse another operand.
