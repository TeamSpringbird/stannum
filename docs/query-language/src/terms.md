# Terms

A term searches for matching tokens in a document.

## Bare terms

A bare word is a term. It matches documents that contain that word.

```
beer
```

The parser preserves the term's spelling. Stannum then analyzes it with the
index's tokenizer, which may normalize its case and accents or split it into
several tokens. A split literal becomes a phrase. Stannum's default tokenizer
folds case and accents; it does not stem words.

## Term characters

The parser accepts most non-whitespace characters in bare terms, including the
characters used in paths, URLs, email addresses, and identifiers:

```
wi-fi                                  hyphenated word
550e8400-e29b-41d4-a716-446655440000   UUID
https://example.com/path               URL
C:\Users\docs\file.exe                 Windows path
/usr/bin/foo                           Unix path
user@host:port                         connection string
#hashtag                               starts with #
$variable                              starts with $
@user                                  starts with @
C++                                    ends with ++
o'reilly                               apostrophe
47,000                                 comma
v2.0                                   dot
café                                   accented Latin
日本語                                  CJK ideographs
🍺                                     emoji
```

Each example is one parser term. Tokenization determines which indexed tokens
it searches for; for example, the default tokenizer splits `wi-fi` into `wi`
and `fi`.

### What can appear in a term

A term can start with or contain any character except whitespace and these
syntax characters:

`(`, `)`, `[`, `]`, `"`, `~`, `^`

Other accepted characters include `/`, `\`, `:`, `#`, `$`, `@`, `+`,
`!`, `.`, `'`, `&`, `%`, and all Unicode characters.

A term starting with `:` needs an explicit operator after another expression:
use `beer AND :tag`, or quote the second term. `beer :tag` is a parse error.

### Hyphens are not negation

The parser treats `-` as part of a term, including UUIDs and hyphenated words.
Use `AND NOT` for exclusion.

### Escaping `*` and `?`

The characters `*` and `?` trigger [wildcard matching](#wildcards). If you need a literal `*` or `?` (for
example, in a URL with a query string), escape it with a backslash:

```
https://example.com/page\?id=42     literal ? (not a wildcard)
file:\*.tar.gz                       literal * (not a wildcard)
```

Without the backslash, `*` and `?` are treated as wildcard characters.

### Keywords and case

Keywords are UPPER CASE only, so **lowercase forms are already plain terms**.
You can search for `and`, `or`, `to`, `not`, `in`, etc. without any quoting:

```
to be or not to be   six search terms joined by implicit AND
```

If you need the UPPER CASE form as a search term, wrap it in a phrase:

```
"THEN"           the literal word THEN, not the operator
"AND"            the literal word AND, not the operator
```

### Quoting a term

If your search text contains syntax characters, whitespace, or UPPER CASE
keywords, put it in double quotes. Inside a phrase, keywords and `*`/`?` are
literal text. The characters `_` (gap), `[` `]` (per-position alternatives),
and `\` (escape) keep their special meanings;
escape them with a backslash to treat them as literal text. See the
[Phrases](phrases.md) chapter.

## CONTAINS

`CONTAINS` is an optional keyword that makes term searches more explicit.
`CONTAINS beer` means exactly the same thing as bare `beer`.

```
CONTAINS beer AND MATCHES hop.*s
```

`CONTAINS` also accepts wildcards and fuzzy matches:

```
CONTAINS brew*
CONTAINS beer~2
```

`CONTAINS` must be followed by a term. Writing `CONTAINS` with nothing after
it is an error.

## Match-all

A standalone `*` matches every document in the index.

```
*
```

This is primarily useful with boolean exclusion: `* AND NOT spam` means
"every document that does not contain the word spam." `*` is a
document-level query and cannot appear as an operand of span, relation,
or positional operators; `* NOT ENCLOSES spam` is rejected with an error
("MatchAll (*) is not valid inside a span/positional context").

`*` has separate meanings in [wildcards](#wildcards), such as `brew*`, and
[ranges](#ranges), such as `* TO cat`.

## Wildcards

Wildcard patterns use `*` (any number of characters) and `?` (exactly one
character) to match families of terms.

```
brew*        matches: brewery, brewing, brewed, ...
*house       matches: warehouse, firehouse, house, ...
b?er         matches: beer, bier, ...
*craft*      matches: aircraft, craftsman, witchcraft, ...
```

A wildcard pattern is any term that contains at least one unescaped `*` or
`?`. The pattern is expanded against the index's term dictionary at query
time, so it matches terms present in your data.

To include a literal `*` or `?` without triggering wildcard expansion, escape
it with `\`, as shown in [Escaping `*` and `?`](#escaping--and-).

### Wildcards and tokenization

The literal text of a wildcard pattern goes through the index's tokenizer
before expansion, exactly like a plain term: it is case/accent-folded, and
punctuation can split it into more than one token. The pattern then
normalizes to the anchored regex form of [`MATCHES`](#regular-expressions):
`*` becomes `.*` (zero or more characters), `?` becomes `.` (exactly one),
and literal fragments are regex-escaped.

- **Single-token patterns convert directly.** `email*` is `MATCHES email.*`,
  `e*mail` is `MATCHES e.*mail`, `b?er` is `MATCHES b.er`.
- **A boundary-spanning literal becomes a phrase.** Under the default
  tokenizer `e-mail` splits into `e`, `mail`, so `e-mail*` becomes the
  phrase `"e [MATCHES mail.*]"`, meaning `e` followed immediately by any term
  starting with `mail` (`e-mail`, `e-mails`, ...). The bracketed element is
  a per-position alternatives slot, which is how a regex occupies one phrase
  position. Flanking wildcards bind to the boundary token (`?wi-fi` is
  `"[MATCHES .wi] fi"`), and an interior wildcard fuses its adjacent
  fragments into one single-token pattern (`e-mail*tail` is
  `"e [MATCHES mail.*tail]"`). Position gaps inside the literal are
  preserved.
- **A literal that tokenizes away matches nothing.** `@*` or `.*` written as
  a wildcard has no analyzable literal left; expanding the bare wildcard
  would match the whole corpus, so it matches nothing instead.
- A literal chopped by the long-token policy has no faithful rewrite and is
  rejected with an error.

Prefix patterns keep their fast expansion: `email.*` scans only the `email`-
prefixed range of the term dictionary, exactly as the glob form did. Use
`stannum.ql_parse('e-mail*')` to see the rewrite a given pattern gets under
the options passed to the function. Supply the index's tokenization options
explicitly when they differ from the defaults.

## Fuzzy matching

Fuzzy matching finds terms within a given edit distance (insertions, deletions,
or substitutions) of your term.

```
beer~2
beer~0:2
```

`term~N` uses a default stable prefix of 1 character. So `beer~2` can match
terms like `bear`, `bee`, and `beet`, but not `peer`.

If you want full control, use `term~P:N`:

- `P` is the number of leading characters that must stay fixed
- `N` is the maximum edit distance

Examples:

```
beer~2      default prefix 1, distance 2
beer~3:1    first 3 characters fixed, distance 1
beer~0:2    no stable prefix, distance 2
```

Place the `~` suffix immediately after the term.

Like a wildcard literal, the fuzzy term is analyzed by the index's tokenizer
first. If it splits across a token boundary, the pattern becomes a phrase
whose last token carries the fuzzy suffix: `e-mail~1` is the phrase `"e [mail~1]"` (the
bracketed alternatives slot is what lets a fuzzy term occupy one phrase
position; a bare `mail~1` inside quotes would be a literal token), matching
`e-mail` (distance 0) and `e-mails` (distance 1). Leading and interior
tokens stay exact.

## Regular expressions

For patterns that go beyond what wildcards can express, use the `MATCHES`
keyword followed by a regex pattern.

```
MATCHES hop.*s
```

This matches any term in the index whose entire text matches the regex. In
other words, `MATCHES alpha` matches the term `alpha`, but not `alphabet`. The
pattern extends until the next unescaped whitespace or end of input. An
unescaped `)` or `]` also ends the pattern unless it closes a group or
character class opened inside the pattern. This lets `(MATCHES hop.*s)` and
`[MATCHES hop.*s]` use their closing delimiters while `MATCHES hop(s)?` keeps
its parentheses; escape a literal one as `\)` or `\]`. To include a literal space
in the pattern, escape it with a backslash:

```
MATCHES foo\ bar      matches the literal term: foo bar
MATCHES back\\slash   matches the literal term: back\slash
```

Standard regex escapes such as `\d`, `\w`, and `\b` pass through to the regex
engine. TinQL interprets `\ ` (backslash-space) as a literal space and leaves
other backslash sequences in the pattern.

The regex pattern is preserved exactly as written. `stannum` does not transparently
rewrite it to be case-insensitive or otherwise modify it before matching.
Invalid regex syntax is rejected during query lowering/planning instead of
silently behaving like an empty match.

Writing `MATCHES` without a pattern is an error.

## Ranges

A range expression matches all terms that fall between two bounds in
lexicographic (dictionary) order.

```
aardvark TO cat
```

This matches every term from "aardvark" through "cat" inclusive. You can leave
either bound open with `*`:

```
* TO cat       everything up through "cat"
monkey TO *    everything from "monkey" onward
```

`*` is an open-bound marker only when it occupies the entire bound.

Range bounds must be plain terms: a wildcard in a bound (`ban* TO cat`) is a
parse error, since a pattern has no defined range semantics. Bounds also pass
through the index's tokenizer for normalization (case/accent folding); a bound
the tokenizer erases entirely is rejected rather than being treated as an open
bound, and a bound chopped by the long-token policy is rejected rather than
silently altered.
