# Introduction

TinQL is the query language used in the text argument of TIN's `==>` operator
in PostgreSQL. It supports terms, phrases, Boolean expressions, and constraints
on where matches occur in a document.

For example, this query requires “craft” before “beer”, with at most five
intervening positions, in the first 200 positions of the document:

```sql
SELECT * FROM articles
WHERE body ==> '(craft THEN/5 beer) IN FIRST 200 WORDS';
```

## Positions and spans

TIN stores the positions of tokens produced by the index's tokenizer. Positions
start at 1. The tokenizer's settings determine word boundaries, normalization,
and whether gaps left by discarded tokens are preserved.

A **span** is a range of positions in a document. A phrase such as
`"big bad wolf"` produces a span for each occurrence of those consecutive words.
`THEN/N` and `NEAR/N` combine spans based on their distance and order.
`ENCLOSES` and `OVERLAPPING` filter spans by their relationship to other spans.
Positional filters restrict them to a region of the document.

These expressions can be combined with Boolean conditions on the whole document.

## Query examples

| Query | Meaning |
|---|---|
| `beer` | Documents containing the word "beer" |
| `"craft beer"` | Documents containing the exact phrase "craft beer" |
| `craft NEAR/5 beer` | "craft" and "beer" with at most 5 words between them, either order |
| `craft THEN/0 beer` | "craft" followed by "beer" (adjacent) |
| `beer AND NOT bud` | Documents with "beer" but without "bud" |
| `beer IN FIRST 100 WORDS` | "beer" appears in the first 100 words |
| `(hops NEAR/5 malt) WITHIN 6` | "hops" near "malt", with the matching span at most 6 words wide |

## Keywords and tokenization

Keywords such as `AND`, `OR`, `THEN`, and `NEAR` are case-sensitive and must be
written in upper case. Lowercase words such as `and`, `or`, and `to` are search
terms. With implicit AND, `to be or not to be` requires each of those words.

The parser preserves the spelling of a search term. Before executing the query,
TIN analyzes literal terms with the index's tokenizer. Under the default
settings, that includes case and accent folding. A written term can produce
several tokens; see [Terms](./terms.md) for the syntax and tokenization rules.
