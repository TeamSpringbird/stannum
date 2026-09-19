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
| [dabf3aa](https://github.com/planetscale/lead/commit/dabf3aa) | Adapted: copyright notices with per-file provenance, Ben's confirmed identity, and a LICENSE reference. Extended beyond Rust to other source formats. License wording clarification remains pending. |
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

For future updates, fetch Lead, pin its SHA, and enumerate commits after the
last reviewed revision. Record whether each patch is ported, already covered
by tested Stannum behavior, intentionally divergent, or pending. A cherry-pick
does not make the upstream commit an ancestor: use this ledger as well as Git
history. Preserve upstream authorship for adapted patches and retain notices
when moving or extracting inherited code.
