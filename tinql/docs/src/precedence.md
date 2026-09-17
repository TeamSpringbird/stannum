# Operator Precedence

When a query uses multiple operators without parentheses, the language has
rules for grouping their operands. Operators lower in the table bind more
tightly than operators near the top.

## Precedence table

From evaluated-last (loosest) to evaluated-first (tightest):

| Level | Operators | Example |
|---|---|---|
| 1 | `OR` | `A OR B` |
| 2 | `AND` (explicit and implicit) | `A AND B`, `A B` |
| 3 | `AND NOT` | `A AND NOT B` |
| 4 | Positional filters | `A IN FIRST 100 WORDS` |
| 5 | Relation operators | `A ENCLOSES B`, `A BEFORE B` |
| 6 | Proximity operators | `A THEN/0 B`, `A NEAR/5 B` |
| 7 | `WITHIN` | `A WITHIN 10` |
| 8 | Boost | `A^2` |
| 9 | Primary (leaves) | `beer`, `"phrase"`, `[a b]`, `MATCHES pat` |

## How to read the table

In this query:

```
A OR B THEN/5 C AND D
```

`THEN/5` (level 6) applies first, then `AND` (level 2), then `OR` (level 1),
giving:

```
A OR ((B THEN/5 C) AND D)
```

## Chaining operators

When the same operator appears multiple times, it groups left to right:

```
A AND B AND C        →  (A AND B) AND C
A OR B OR C          →  (A OR B) OR C
A THEN/0 B THEN/0 C  →  (A THEN/0 B) THEN/0 C
```

## Overriding with parentheses

Use parentheses whenever the default grouping isn't what you want:

```
(A OR B) AND C           group OR before AND
A THEN/0 (B NEAR/3 C)    override left-to-right chaining
(A AND B) IN FIRST 100 WORDS   apply filter to the whole conjunction
```

Use parentheses to make the intended grouping explicit.

## Implicit operators

When terms are written next to each other without an explicit operator
(`beer wine` instead of `beer AND wine`), the implicit operator follows the
same rules as explicit `AND`:

```
beer wine OR stout cheese  →  (beer AND wine) OR (stout AND cheese)
```

TIN's SQL interface uses implicit AND. Callers of the Rust parser API can
select implicit OR; in that mode, `beer stout AND wine` parses as
`beer OR (stout AND wine)`. See [Boolean Operators](./boolean-operators.md).

## Postfix operators

Several operators appear after the expression they modify:

- `~N` (slop/fuzzy): part of the primary expression
- `^N` (boost)
- `WITHIN N`
- `IN FIRST/LAST/WORDS`

For example:

```
"big bad"~2^1.5      slop first, then boost
(A NEAR/3 B) WITHIN 6   proximity first, then width limit
beer IN FIRST 200 WORDS AND wine   positional filter on beer, then AND
```
