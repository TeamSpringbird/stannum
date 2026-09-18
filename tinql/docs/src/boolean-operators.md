# Boolean Operators

Boolean operators combine expressions to determine which documents match.

## AND

Both sides must match.

```
beer AND wine
```

Documents must contain both "beer" and "wine", at any positions in the text.

## OR

At least one side must match.

```
beer OR wine
```

Documents containing "beer", "wine", or both.

## AND NOT

The left side must match and the right side must not.

```
beer AND NOT bud
```

Documents with "beer" that do not contain "bud".

AND NOT binds more tightly than AND, so:

```
A AND B AND NOT C
```

is interpreted as `A AND (B AND NOT C)`. A and B must be present; C must be absent.

## Chaining

Boolean operators are left-associative:

```
A AND B AND C     →  (A AND B) AND C
A OR B OR C       →  (A OR B) OR C
```

## Mixing operators

AND binds more tightly than OR:

```
A OR B AND C      →  A OR (B AND C)
```

Use parentheses to override:

```
(A OR B) AND C
```

## Implicit operators

When you write terms next to each other with no explicit operator, they are
combined automatically. The default is AND:

```
beer wine                  →  beer AND wine
beer wine stout            →  beer AND wine AND stout
beer "craft ale"           →  beer AND "craft ale"
beer (wine OR stout)       →  beer AND (wine OR stout)
```

Explicit operators keep their precedence:

```
beer wine OR stout cheese  →  (beer AND wine) OR (stout AND cheese)
```

### Implicit OR mode

The Rust parser API accepts `ImplicitOp::Or`. Callers of that API can parse
adjacent expressions as OR:

```
beer wine                  →  beer OR wine
beer stout AND wine        →  beer OR (stout AND wine)
```

Stanum's SQL interface uses implicit AND for both `==>` and `stanum.ql_parse()`.
Write `OR` explicitly in the TinQL string to request either term.

## Hyphens

The `-` character belongs to a term. These are each single parser terms; the
index tokenizer may split them into several tokens:

```
wi-fi                      a single term
550e8400-e29b-41d4-a716-446655440000   a UUID as a single term
my-api-token               a single term
```

To exclude documents, always use `AND NOT`:

```
beer AND NOT bud
```
