# Phrases

A phrase is a sequence of words enclosed in double quotes. It matches documents
where those words appear consecutively, in exactly that order.

```
"big bad wolf"
```

The words must occur consecutively. Gaps, per-position alternatives, and a slop
suffix let you allow other arrangements.

## Gaps

An underscore `_` inside a phrase represents a gap of one position. Multiple
consecutive underscores widen the gap.

```
"big _ wolf"       one word between "big" and "wolf"
"big __ wolf"      two words between "big" and "wolf"
"big ___ wolf"     three words between "big" and "wolf"
```

`"big _ wolf"` matches text like "big bad wolf", "big old wolf", or "big mean
wolf", each with exactly one word between "big" and "wolf".

Each `_` counts as one position. If you need a gap of 5 words, use five
underscores: `"big _____ wolf"`.

Gaps only count *between* words. Underscores at the start or end of a phrase
have no neighboring word to anchor to and are ignored: `"_ wolf"` matches the
same documents as `"wolf"`, and `"big bad _"` is the same as `"big bad"`. A
phrase containing only gaps matches nothing.

### When to use gaps vs. proximity

Gaps are exact: `"big _ wolf"` requires *exactly* one word between "big" and
"wolf". If you want *up to* a certain distance, use slop (below) or the
[`THEN/N` proximity operator](proximity.md#thenn).

## Per-position alternatives

Square brackets inside a phrase let you specify multiple choices for a single
position.

```
"[big large] bad wolf"
```

This matches "big bad wolf" or "large bad wolf". The alternatives are
whitespace-separated, and exactly one must match at that position.

Inside the brackets, whitespace separates the choices, so commas are just
literal term characters.

You can place alternatives at any position, and use multiple sets:

```
"[big large] [bad mean] wolf"
```

This matches any combination: "big bad wolf", "big mean wolf", "large bad
wolf", or "large mean wolf".

Alternatives inside phrases are useful for synonym expansion at specific
positions. Each alternative is a full expression, so you can nest
spans and Boolean operators inside the brackets.

A quoted phrase cannot appear inside the brackets of a phrase: an unescaped
`"` ends the enclosing phrase, and an escaped quote (`\"`) causes a parse
error inside the brackets. Multi-word alternatives can arise when the
tokenizer splits a written token: an
alternative like `wi-fi` occupies consecutive positions at that slot.

## Slop

A tilde and integer after the closing quote adds **slop**, the allowed number
of extra positions between the phrase terms.

```
"big bad wolf"~2
```

This matches "big bad wolf" (exact), but also allows up to 2 additional word
positions between the terms. So it would match text like "big, very bad wolf"
where extra words are interspersed.

Place `~` immediately after the closing quote.

### Slop vs. gaps

- **Gaps** (`_`) specify *exactly* how many words to skip at a specific
  position.
- **Slop** (`~N`) specifies a *total tolerance* for extra positions across the
  entire phrase.

You can combine them: `"big _ wolf"~1` means "big", then a gap of one word,
then "wolf", with one additional position of tolerance applied to the overall
match.

## Escaping inside phrases

Several characters have special meaning inside quotes. To use them as literal
text, escape them with a backslash:

| Escape | Produces |
|---|---|
| `\"` | literal `"` (quote) |
| `\\` | literal `\` (backslash) |
| `\_` | literal underscore |
| `\[` | literal `[` (not the start of alternatives) |
| `\]` | literal `]` (not the end of alternatives) |

Examples:

```
"she said \"hello\""       phrase containing a literal quote
"big \_ wolf"              matches the literal text: big _ wolf
"array\[0\]"               matches the literal text: array[0]
```

Without the backslash, `_` would be interpreted as a gap and `[` would start
an alternatives group.

## Empty phrases

An empty pair of quotes `""` is an error. A phrase must contain at least one
element.
