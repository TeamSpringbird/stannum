# Selective retrieval: first implementation slice

> Historical research/design note. References to Lead describe the original project
> or pre-rename fork; TIN refers to PlanetScale's extension. Current project names
> and status are in [README](../../README.md) and [BENCHMARKS](../benchmarks/README.md).

Status: this document records the first in-memory slice. The subsequent
[durable candidate storage slice](durable-postings.md) is now implemented.

`tinql/src/runtime/retrieval.rs` provides an immutable in-memory inverted segment.
It uses the existing normalized `TokenizedDoc` input and lowered `Query` tree, so
it does not introduce a second tokenizer or query parser. At this first milestone, the SQL access method remained unchanged and scanned/
rechecked heap pages; the durable follow-up now changes term, Boolean and phrase retrieval. No SQL speedup or durability
claim follows from this module alone.

## Implemented contract

* `SegmentBuilder::insert` accepts documents in arbitrary ID order and rejects
  duplicate IDs before changing their postings. Repeated terms produce one posting
  per document. Empty documents register their identity but produce no terms.
* `finish` seals a term dictionary of sorted, unique document IDs. Text and token
  positions are not retained. Document frequencies are available directly.
* `Segment::candidates` returns either an ordered, unique iterator of candidate IDs
  or an explicit `All` fallback. An empty iterator is a true candidate miss; it is
  never used to represent an unsupported operation.
* Terms, AND, OR and boosts use postings directly. Positional query trees retain
  their necessary positive terms while dropping positional restrictions. Phrases
  therefore narrow candidates but still require exact positional rechecks.
* Unsupported expansion/threshold/advanced-span queries and NOT use conservative
  fallback. An unsupported OR branch forces fallback for that union. AND can still
  use its other selective branches. Negative span relations retain only their
  positive side; they do not intersect with terms that must be absent.
* Intersection and union stream their sorted inputs. Iterator state grows with the
  query tree, not result cardinality. This first scalar implementation uses neither
  compression nor skip blocks and does not optimize conjunction order.

These are candidate supersets, not visible results. Callers must apply the exact
reference evaluator and snapshot visibility. Query/document tokenizer settings
must match. Document IDs are opaque `u64` values; no persistent encoding or native
TID mapping is claimed. Segment replacement, deletion, merging, and concurrent
publication are deliberately outside this first module's interface.

## Validation

Six targeted tests pass:

* Sorted/deduplicated postings, rejected duplicate identities, and true misses.
* Phrase candidates retain documents needing a positional recheck.
* Unsupported OR/NOT branches never masquerade as missing terms.
* Streaming union begins yielding before consuming its input lists.
* AND/OR agree with independent sets for all 4,096 pairs of six-element subsets.
* Candidate supersets include every reference match over all sequences of up to
  four tokens from a three-word vocabulary, plus case/punctuation/Unicode fixtures,
  across Boolean, phrase, positional, fuzzy, wildcard and fallback query forms.

```sh
PATH="$(brew --prefix rustup)/bin:$PATH" \
  cargo test --locked -p tinql runtime::retrieval --jobs 1 -- --test-threads=1
```

The tests do not establish crash recovery, multi-session visibility, SQL planning
behavior, or competitive performance.

## Historical follow-up plan: one durable term end to end

Before replacing `amgetbitmap`, implement and test the PostgreSQL storage adapter:

1. Specify a versioned on-disk dictionary/posting layout and atomic publication
   protocol, with buffer ownership, lock order and WAL behavior. This in-memory
   segment is not itself a serialized file format or a durable write buffer.
2. Define native tuple-location identity, including HOT chains and location reuse.
   Store build callback expression values, not assumed raw heap columns, so partial
   and expression indexes preserve their semantics.
3. Connect index build, insert and scan for a single positive term. Retain exact
   heap rechecks and conservative fallback for expressions outside that path.
4. Cover rollback, updates/deletes, VACUUM, concurrent readers/writers, restart,
   recovery, empty/unlogged indexes, and truncation before enabling the new SQL
   path. Existing zero-page indexes need a safe fallback or explicit rebuild policy.
5. Measure rare/missing term retrieval on the same frozen corpora, then extend the
   stored postings path to Boolean and phrase candidates. Ranking remains a later
   change: this slice stores no term frequencies, lengths, positions or BM25 state.

The running baseline remains pinned to its earlier image and frozen scripts.
Future images will include the changed `tinql/` source fingerprint; the existing
benchmark provenance mechanism already includes that directory.
