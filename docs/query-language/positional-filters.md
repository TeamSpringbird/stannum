# Positional Filters

Adapted from [PlanetScale Lead’s query-language documentation](https://github.com/planetscale/lead/tree/3fcf441ac7c3d183de179b1f846ceb0ef83e1358/tinql/docs/src).

Positional filters restrict matches to a range of token positions, such as the
first 100 positions or the final quarter of a document.

## IN FIRST N WORDS

Keeps only matches that occur within the first N word positions.

```
beer IN FIRST 100 WORDS
```

Only matches "beer" if it appears at position 1 through 100.

Use this to search an opening section when your documents put their main topic
near the beginning.

You can apply it to any expression:

```
(security NEAR/5 threat) IN FIRST 200 WORDS
```

Proximity matches in the opening section of the document.

## IN FIRST N%

Keeps matches in the first N percent of the document.

```
beer IN FIRST 25%
```

Matches "beer" in the first quarter of the document. This adapts to document
length: 25% of a 1000-word document is the first 250 words, while 25% of a
200-word document is the first 50 words.

Place `%` immediately after the number.

## IN LAST N WORDS

Keeps only matches within the final N positions.

```
beer IN LAST 50 WORDS
```

Matches "beer" only if it appears in the last 50 word positions of the
document. Useful for finding terms in conclusions, sign-offs, or closing
paragraphs.

## IN LAST N%

Keeps matches in the final N percent of the document.

```
beer IN LAST 25%
```

Matches "beer" in the last quarter of the document. This adapts to document
length: 25% of a 1000-word document is the last 250 words, while 25% of a
200-word document is the last 50 words.

Place `%` immediately after the number.

## IN MIDDLE N%

Keeps matches in the middle portion of the document, excluding equal amounts
from the beginning and end.

```
beer IN MIDDLE 50%
```

Matches "beer" only in the middle 50% of the document, excluding the first and
last 25%. This is useful for skipping introductions and conclusions to
focus on the body of the text.

```
(security NEAR/5 threat) IN MIDDLE 80%
```

Find security-threat spans in the middle 80% of the document, skipping the
first and last 10%.

`IN MIDDLE` supports percentages only. `IN MIDDLE N WORDS` is not implemented.

Place `%` immediately after the number.

## IN WORDS LO TO HI

Restricts matches to a specific word-position range.

```
beer IN WORDS 500 TO 1000
```

Matches "beer" only at positions 500 through 1000, inclusive. Useful when you
know the structure of your documents and want to target a specific section.

## What positional filters apply to

A positional filter applies to the expression immediately to its left. So:

```
beer IN FIRST 200 WORDS AND wine
```

is interpreted as:

```
(beer IN FIRST 200 WORDS) AND wine
```

The positional filter applies to "beer", not to the entire AND expression. To
apply it more broadly, use parentheses:

```
(beer AND wine) IN FIRST 200 WORDS
```

Now both "beer" and "wine" must appear in the first 200 words.
