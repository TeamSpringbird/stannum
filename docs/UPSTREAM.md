# Lead synchronization

This migration starts from Stannum `9bc663ac5d84082a04ae1ff0578be824dd2018d3`
(including PRs #16–18). Lead was inspected at
`6007456d659dc83411bb177385d16921f07391e4`; the latest shared ancestor is
`300ad3afbcaae42a90ef963cb3eb445784c733b3`.

| Upstream commit | Disposition |
| --- | --- |
| [9dc22dd](https://github.com/planetscale/lead/commit/9dc22dd355fddf2a4d9ce4445733fd9055d7c270) | Adapted: shared interrupt checks every ten documents for positioned-token heap scoring and plain-token score inspection. A PostgreSQL test queues cancellation during tokenization and requires it to interrupt the loop. |
| [fdffe7b](https://github.com/planetscale/lead/commit/fdffe7b63454d40154dfabb4b0e3dc14a7f4acff) | Matching-document maximum already implemented in both Stannum scorers. Imported Boolean/phrase/positional regression cases, expanded to explicit heap fallback and partial indexes. Also adopted propagation of evaluation errors in the fallback instead of treating errors as nonmatches. |
| [251df39](https://github.com/planetscale/lead/commit/251df396f0a13636c67331d08550b419209bb66a) | Imported unchanged with original author and cherry-pick reference. The Boldi–Vigna implementation is unchanged from the shared ancestor. Attribution added separately. |
| [dabf3aa](https://github.com/planetscale/lead/commit/dabf3aa) | Adapted: copyright notices with per-file provenance, Ben's confirmed identity, and a LICENSE reference. Extended beyond Rust to other source formats. Lead's later license clarification ([0d261de](https://github.com/planetscale/lead/commit/0d261de92c5f2d972b82c760894c68412d9e446e), AGPL-3.0-or-later) was adopted in #76; see [source attribution](ATTRIBUTION.md#notices). |
| [47519bd](https://github.com/planetscale/lead/commit/47519bd45df70ab3b66911c96cbbb8e358685338) | Intentionally not imported: retain pull-request CI, including fork PRs. Any duplicate-run optimization should preserve that validation. |

The scoring port passed all 119 extension tests on both PostgreSQL 17 and 18
on the local Apple Silicon machine. This includes existing partial-index and
expression-index tests as well as the new cases. CI remains responsible for
the Linux x86-64/AArch64 matrix. The cancellation test establishes delivery
between documents; this change does not add cancellation checks inside the
tokenization of one exceptionally large document.

The attribution layer has no runtime changes. All 158 modified files were
verified byte-for-byte against the parent revision after removing only the
inserted notices; insertion is idempotent, and LICENSE is unchanged. The
inventory includes 92 Rust files: 38 unchanged inherited, 18 modified inherited
(including the moved term-frequency module), and 36 Stannum-authored files.
All 79 benchmark-harness tests, six attribution-tool tests, and 467 library/doc
tests pass (six existing tests remain ignored). Formatting and Clippy with
warnings denied pass for PostgreSQL 17 and 18. See
[source attribution](ATTRIBUTION.md) for the policy and future-file checks.

PR #10 (unlocked fold construction) was still open at audit time. It can
update from the attribution change once; any new source files need provenance
entries and headers. Do not hold this migration indefinitely for that work.

## October 2026: Lead `bd95c7e..e3ed2f4`

Lead was synced to
[`e3ed2f4ce1388167b0254a0c0c706a61d0a37d83`](https://github.com/planetscale/lead/commit/e3ed2f4ce1388167b0254a0c0c706a61d0a37d83)
(2026-10-07), the reference oracle's new `LEAD_REF`. Lead's commits of
2026-10-06/07 copy fixes from PlanetScale's TIN, so they show TIN's current
behavior; TIN 1.0.4's recorded answers (`conformance/expected/tin-1.0.4`)
were checked as well.

| Upstream commit | Disposition |
| --- | --- |
| [b8018ac](https://github.com/planetscale/lead/commit/b8018ac35a1b5328e7bdb472b7fe9316e7a5bad9) / 56dff87 | Ported. boldi-vigna's NEAR over OR-group operands sharing a word (each operand its own occurrence) and the tinql changes imported; span scoring per written occurrence with operand boosts, the excluded side of a negated relation unscored. Lead's crate tests plus two pg_tests (index and heap paths). Both changes differ from TIN 1.0.3/1.0.4 (`catalog.S-11`, `span.minimal_interval.4`–`.6`, declared `newer`); Lead e3ed2f4 answers as Stannum. |
| [615e9ce](https://github.com/planetscale/lead/commit/615e9ce) / 77f1365 | Already equivalent: segments and the write buffer never record a token-less document (`catalog.S-21`). |
| [623174d](https://github.com/planetscale/lead/commit/623174d) | Ported: the `stemmer` option and stemmer arguments; see [stemming](compatibility.md#stemming). The stemmer is persisted with the tokenizer settings, so a changed stemmer applies at `REINDEX` (TIN applies it to queries at once). |
| [a03e682](https://github.com/planetscale/lead/commit/a03e682) | Ported (semantics): `==> ANY(...)`/`ALL(...)` bind to the index's tokenizer from the `get_relation_info` hook, with Lead's tests adapted. Lead's refusal of partitions with differing tokenization was not ported. |
| [a22cd04](https://github.com/planetscale/lead/commit/a22cd04) | Not ported, for the owner to decide: Lead refuses queries over 2,048 bytes or nested deeper than 64 brackets with SQLSTATE 54000. TIN 1.0.4 answers both (`query_size.or_chain.1000`, `nested.100`, `nested.1000`); Stannum keeps its 10,000-term and 1,000-level limits (54001). |
| [01d5d6c](https://github.com/planetscale/lead/commit/01d5d6c), [50f8c04](https://github.com/planetscale/lead/commit/50f8c04), [798e58a](https://github.com/planetscale/lead/commit/798e58a), [03c54f5](https://github.com/planetscale/lead/commit/03c54f5), [ca05758](https://github.com/planetscale/lead/commit/ca05758), [11beb68](https://github.com/planetscale/lead/commit/11beb68) | Ported where Stannum differed: parameters and several clauses score and highlight as one ORed query, each text parsed on its own; clauses under `NOT` or on other relations do not bind; inner-join ON search texts score per row; a row no search admits scores NULL; an unproved partial index refuses scoring (0A000). Column sums were already equivalent (`catalog.S-18`). Deferred: `max_score()` over several columns, Lead's index choice, outer-join ON clauses. See [which clauses score](compatibility.md#what-matches-tin-103). |
| [1abf236](https://github.com/planetscale/lead/commit/1abf236) | Not imported: an explanation for stale installed SQL; Stannum has no released upgrade path yet. |
| 9c5b8b9, 6117578, 7308d8f, 06cdfa4 | Not imported: Lead's version bump, Docker image and CI. |

For future updates, fetch Lead, pin its SHA, and enumerate commits after the
last reviewed revision. Record whether each patch is ported, already covered
by tested Stannum behavior, intentionally divergent, or pending. A cherry-pick
does not make the upstream commit an ancestor: use this ledger as well as Git
history. Preserve upstream authorship for adapted patches and retain notices
when moving or extracting inherited code.
