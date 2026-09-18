# Recipes

Adapted from [PlanetScale Lead’s query-language documentation](https://github.com/planetscale/lead/tree/3fcf441ac7c3d183de179b1f846ceb0ef83e1358/tinql/docs/src).

These examples combine TinQL operators for common search tasks.

## Find a topic in the opening section

Find documents that mention "machine learning" near the beginning.

```
"machine learning" IN FIRST 200 WORDS
```

The phrase must appear within the first 200 positions.

## Synonym expansion with proximity

**Goal:** Find discussions of security threats, using multiple synonyms, where
the terms are near each other.

```
security NEAR/5 [threat vulnerability risk exploit]
```

"security" within 5 extra positions of any of the listed synonyms, in either
order.

## Filter out commercial content

Find security-related passages that omit selected commercial terms.

```
(security NEAR/5 [threat vulnerability risk])
NOT ENCLOSES [buy purchase subscribe pricing discount]
```

The proximity match finds security-related spans. NOT ENCLOSES removes any
spans that also mention commercial terms.

## Topic in the introduction, with a required concept

**Goal:** Security in the opening, plus the document must discuss compliance.

```
(
    (security NEAR/5 [threat vulnerability risk])
    NOT ENCLOSES [buy purchase subscribe pricing]
) IN FIRST 200 WORDS
AND compliance
```

The query applies these conditions:

1. Proximity for conceptual co-occurrence
2. NOT ENCLOSES to exclude commercial spans
3. IN FIRST for positional restriction
4. AND for a document-level requirement

## Ordered multi-word concept with flexibility

**Goal:** Find "results" followed by "discussion" within a reasonable distance.

```
results THEN/20 discussion
```

"results" must come before "discussion", with up to 20 words between them. This
captures section-style patterns in academic papers.

## Dense cluster of related terms

**Goal:** Three terms all near each other, in a tight span.

```
(hops NEAR/10 malt NEAR/10 yeast) WITHIN 20
```

Each proximity step allows up to 10 extra positions. WITHIN limits the full
matching span to 20 positions.

## Require a phrase before another phrase

**Goal:** "methods" section must appear before "results" section.

```
methods BEFORE results
```

Keeps occurrences of "methods" that have "results" somewhere later in the
document.

## Minimum coverage across facets

**Goal:** A document must touch at least 3 of 5 related concepts.

```
AT LEAST 3 OF [
    "machine learning"
    "neural network"
    "deep learning"
    "training data"
    "model accuracy"
]
```

The document must match at least three of these five phrases.

## Negation to exclude unwanted matches

**Goal:** Find "java" in a programming context, not the island or coffee.

```
java AND NOT [island coffee indonesia sumatra]
```

To require a nearby programming term:

```
java NEAR/10 [programming code class method object]
```

The proximity query requires evidence of a programming context. The exclusion
query removes documents that contain any of the listed unwanted terms.

## Phrase with flexible positions

**Goal:** Match "big ??? wolf" where the middle word can be anything.

```
"big _ wolf"
```

Or allow two unknown words:

```
"big __ wolf"
```

## Phrase with synonym at one position

**Goal:** Match "the [big|large|huge] wolf".

```
"the [big large huge] wolf"
```

The alternatives expand at exactly that position in the phrase.

## Boosted compound query

**Goal:** Find documents containing "machine learning" and "research", giving
"research" more weight in the score.

```
"machine learning" AND research^3
```

The phrase and "research" must both match. The boost multiplies the weight of
"research" by 3; term frequency and document length also affect the score.

## Complex compositional query

**Goal:** Find technical security content in document introductions that
discusses specific vulnerability types, excluding sales material, with boosted
emphasis on critical severity.

```
(
    (
        (security NEAR/5 [threat vulnerability risk])
        ENCLOSES [CVE zero-day "buffer overflow" injection]
    )
    NOT ENCLOSES [buy purchase subscribe "free trial"]
) IN FIRST 300 WORDS
AND (compliance OR [NIST ISO SOC])
AND critical^2
```

The query applies these conditions:

1. Security-threat proximity spans that contain specific vulnerability types
2. Exclude commercial language
3. Must be in the first 300 words
4. Document must discuss compliance frameworks
5. Require "critical" and double its contribution to the score (AND makes it
   a mandatory clause, not an optional boost)
