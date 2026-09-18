# TIN index options and highlighting compatibility

Audited 2026-09-18 against Stannum `45d2696` plus this change. The live
[PlanetScale index reference](https://planetscale.com/docs/postgres/search/reference/indexes)
was retrieved on that date; the supplied `/docs/postgres/tin` URL is not the
current reference. Historical observations dated 2026-09-17 were recovered from
`33e4ea8^:docs/archive/tin-configuration-research.md`,
`33e4ea8^:docs/archive/tin-observed-shape.md`, and
`33e4ea8^:docs/archive/tin-research.md` (deleted from the current tree).
The historical live observations used TIN 1.0.2 on PostgreSQL 18.6. Current web
pages do not identify a TIN release, so this is an audit of the documented
surface, not a claim to enumerate undocumented options.

## Complete documented index-option surface

All 15 options on the public index reference are listed below. Stannum's
registration is in [`options.rs`](../postgres/src/options.rs); tokenization
is represented by [`TokenizerPipelineSpec`](../tokenizer/src/spec.rs).
An equal option surface does not establish identical behavior on every Unicode
version or script; the exact Stannum behavior and remaining uncertainty follow.

| Option | TIN accepted values; default | Meaning | Stannum values; default | Status / decision |
| --- | --- | --- | --- | --- |
| `tokenizer` | `unicode`, `whitespace`; `unicode` | Word boundaries or whitespace fields | Same | Matches documented modes; test punctuation, URLs, numerics, apostrophes, mixed scripts |
| `case_folding` | `fold`, `preserve`; `fold` | Case-insensitive or original-case terms | Same | Surface matches; Stannum lowercases Unicode scalars, not full Unicode case folding (`ß` stays `ß`); TIN docs do not specify an algorithm |
| `accent_folding` | `fold`, `preserve`; `fold` | Remove or retain accents | Same | Surface matches; Stannum uses canonical decomposition, removes combining marks, recomposes; TIN algorithm unspecified |
| `long_tokens` | `split`, `truncate`, `discard`; `split` | Handle terms beyond byte ceiling after folding | Same | Matches documented modes; chunks prefer grapheme boundaries, oversized clusters fall back to bounded scalar chunks |
| `max_token_bytes` | integer `4..2692`; `256` | UTF-8 analyzed-term byte ceiling | Same | Matches; tested with accented source text that shrinks after folding |
| `graphemes` | `emoji`, `retain`, `discard`; `emoji` | Standalone emoji/symbol clusters | Same | Matches documented modes; tests ZWJ emoji, flags, symbols |
| `position_gaps` | `preserve`, `collapse`; `preserve` | Positions consumed by removed analyzed tokens | Same | Matches documented modes; discarded long tokens leave gaps only in preserve mode; base-tokenizer discarded punctuation/graphemes do not consume positions |
| `k1` | real `0..10000`; `1.2` | BM25 saturation | Same | Matches; query-time setting, no rebuild |
| `b` | real `0..1`; `0.75` | BM25 length normalization | Same | Matches; query-time setting, no rebuild |
| `score_stop_words` | comma-separated text; unset | Exact analyzed terms omitted from default scoring | Same | Matches; entries are not tokenized; does not remove indexed tokens or change matching; full scoring ignores this list |
| `initial_segment_count` | integer `1..4096`; available parallelism | Build partitions; default target count | `1..4096`; stored default `1` | Accepted **and ignored, with warning**; expanded previous `1..1024` domain |
| `target_segment_count` | integer `1..4096`; initial count | Background maintenance target | Same range; inert stored default `1` | Newly accepted **and ignored, with warning** |
| `max_mutable_segment_size` | integer `>=131072`; `4194304` bytes | Promotion trigger; also 16,384 docs | Signed 32-bit integer `131072..2147483647`; `4194304` | Newly accepted **and ignored, with warning**; public docs give no explicit upper bound beyond `int` |
| `max_merged_segment_size` | integer `>=100`; `2000` MB | Segment merge-size ceiling | Signed 32-bit integer `100..2147483647`; `2000` | Newly accepted **and ignored, with warning**; public docs give no explicit upper bound beyond `int` |
| `dead_percent_threshold` | real `0..1`; `0.5` | Dead-entry rewrite threshold | Same | Newly accepted **and ignored, with warning** |

The five storage options are accepted for portable DDL, with a warning whenever
explicitly validated by CREATE/ALTER INDEX. Stannum still uses its own documented
[storage and maintenance settings](architecture/segmented-storage.md). In
particular, setting a compatibility option does not enforce a memory limit,
create workers, or change the rewrite threshold. Default values in the catalog
are inert; no background maintenance is introduced. The new fields are only
PostgreSQL reloptions data, not serialized segment or meta-page fields. There
is no new SQL function, upgrade script, or on-disk format change.

No documented tokenizer behavior is missing from the pipeline. The reference
lists neither a stemming option nor a language selector nor indexing-time stop
words. This change adds none: inventing a Snowball or other stemmer would change
matching without a TIN contract. `score_stop_words` remains scoring-only. The
[settings reference](https://planetscale.com/docs/postgres/search/reference/settings)
contains server/session GUCs, not additional `WITH` index options; they are
outside this table and are not accepted as reloptions.

Tokenization changes require REINDEX for stored rows, as the public reference
states. Stannum binds matching and highlighting to the index's persisted
pipeline, including sequential/bitmap rechecks. This is more permissive than
the archived TIN 1.0.2 observation that non-default tokenization could reject a
query lacking a usable custom scan. The unit tests pin all folding, boundary,
grapheme, long-token and gap modes with concrete edge inputs; pg_tests create
indexes with each mode and compare plans, matches and highlighting. These
checks establish Stannum's contract and Lead compatibility, not exhaustive
binary equivalence to hosted TIN.

## Reference highlighting oracle

`script/reference-oracle` records the actual Lead revision and `\df
tin.*highlight*` output alongside its report. Lead exposes
`tin.highlight(text, begin_tag text DEFAULT '<b>', end_tag text DEFAULT '</b>',
query text DEFAULT NULL)` and `tin.highlight_ansi(text, wrap_to integer DEFAULT
NULL, query text DEFAULT NULL)`; Stannum has the equivalent schema-qualified
functions plus its internal bound-query overloads.

Each query observes the one-argument HTML and ANSI functions for every matching
document, ordered by ID, exercising implicit query binding. Rendered strings,
including escape codes, are compared exactly in both score-bit and rank-order
modes. Highlight errors are retained independently of matching/scoring evidence.
Score exclusions for Lead expansions do not exclude their highlights. A
separate `REFERENCE_UNHIGHLIGHTED` reason map controls only proven reference
highlight defects in order mode; raw observations and exclusions are always
recorded in `oracle.json`.

The fixture now contains numeric decimals, apostrophes, hyphens, URL hosts,
accented text, and ZWJ emoji. Six new ordered shapes bring the suite to 47
queries across five mutation states (235 query/state pairs).

Local result: **235/235 agree, zero differences**, against Lead revision
`0e29dbe5177bb64d027d6afeaa20eb0b46536be6` (extension `tin` 1.0.3),
PostgreSQL 18, 5,000 generated fixture rows plus the edge rows and later inserts.
HTML and ANSI observations contain no errors. **Highlight exclusion list:
empty.** No Stannum renderer bug was found, and no renderer change was needed.
The six pre-existing score-order exclusions remain restricted to scoring.

The initial harness attempted an aggregate over a flattenable subquery. Both
engines lost implicit query binding in that shape. The final harness uses a
direct ordered projection, `SELECT json_build_array(id, engine.highlight(body))
FROM oracle_docs WHERE body ==> query ORDER BY id`, and parses each JSON row.
This exercises the supported implicit-binding form without suppressing any
query shape or using explicit-query arguments to bypass binding.

Validation: PG18 suite 76 tests passed; all non-extension workspace tests
passed, including four new tokenizer audit tests; PG18 and PG17 clippy with
pg_test and warnings denied passed; 44 Python benchmark tests passed. Full
reference report and catalog evidence were saved locally under
`/tmp/stannum-ab7-oracle-final/`; the large raw report is not checked in.
