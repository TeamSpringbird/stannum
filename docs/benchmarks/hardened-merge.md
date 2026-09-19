# Validated direct merge API

`segment::merge::merge` turns the direct-merge experiment into an explicit codec
API. It borrows complete input blobs paired with their own dead sets, takes caller
limits and a cancellation callback, and returns a current-format segment or a
structured error. It has no PostgreSQL publication side effects. The subsequent
[foreground integration](direct-merge-integration.md) calls it under the existing
metadata lock; the measurements below describe the earlier standalone API.

## Contract

Input-count, aggregate encoded-byte and aggregate document-count admission checks
run before full verification. Encoded input is also capped at `u32::MAX` bytes.
Each input is then checked by the existing whole-segment verifier, including dead
documents, before the merger trusts its lengths, buckets or ordering. The expected
LSG1 legacy warning is allowed; other findings fail validation. Every dead TID must
belong to its own source document table. The API pairs source and dead set in one
`MergeInput` to avoid mismatched parallel arrays.

Duplicate live TIDs fail. A dead tuple in one source does not suppress a live
occurrence in another source. Dictionary and posting order is preserved through
heap merges; output statistics and score bounds are rebuilt by existing codecs.
Extent widths, document counts, posting counts and total length are checked.
Output growth is checked against the caller's encoded-output limit. No partially
built blob is returned after a limit failure, invalid input or cancellation.

The cancellation callback runs at entry, around each whole-input verification,
and between documents, terms and postings. Existing whole-input verification and
individual codec calls are not interruptible internally, so this is cooperative
cancellation, not a bounded-latency guarantee. PostgreSQL integration must
also respect interrupt holdoff while buffer content locks are held; callbacks
alone do not establish query cancellation responsiveness.

These limits are not a peak-memory cap. Input blobs, document-length lookup,
per-term builders, validation scratch state and output buffers use memory. The
final output reserve is fallible, but existing codecs use infallible allocation;
process-wide OOM is not converted to a merge error. The [integration report](direct-merge-integration.md) records isolated-process
peak RSS measurements and the PostgreSQL memory-policy limitations.

## Tests

Tests compare all three output formats against the existing builder using mixed
input formats, interleaved/disjoint TIDs and no/some/all deletions. They cover
empty output, exact and exceeded limits, unknown dead TIDs, duplicate live TIDs,
dead TID reuse, corrupt document lengths even in dead documents, and cancellation
at every exposed checkpoint followed by a successful retry. Generated inputs
check equivalence. Arbitrary bytes and single-byte mutations check rejection or
verified output without panics.

A separate decoder fix accompanies the lower POC PR: refreshed CI found that a
small malformed posting stream could request a 21 GB allocation from its claimed
count. Initial posting/dictionary allocation is now bounded by actual input bytes;
bound tables grow after successful decoding. A deterministic regression also
covers huge dictionary and bound-table claims.

Local validation passed 454 core unit tests and four documentation tests,
formatting and warnings-denied segment Clippy. At that stage, PostgreSQL integration was
unmodified and covered by CI build/tests rather than a new runtime claim.

## Measurement

The release probe compares the existing reconstruction builder against the API
including full verification. It uses the same synthetic fixture family as the
[POC](direct-merge-poc.md): eight mixed-format segments, interleaved TIDs, every
seventh document dead, ten alternating rounds with two discarded warmups. Parsing,
validation for the new API, merging and encoding are inside the timed region;
fixture construction and output comparisons are outside. Both methods must produce
identical bytes every round.

```sh
cargo test -p segment --release hardened_merge_microprobe -- --ignored --nocapture
```

Apple M4 Max, native ARM64, Rust 1.96.0 release build, after the decoder allocation
fix. Median milliseconds from eight retained samples per method:

| Documents per segment | Tokens/document | Vocabulary | Reference ms | Validated merge ms | Ratio |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 32 | 40 | 4 | 0.489 | 0.250 | 1.95× |
| 512 | 400 | 4 | 10.005 | 7.390 | 1.35× |
| 512 | 400 | 400 | 252.289 | 105.921 | 2.38× |

No server, WAL, locks, I/O or concurrent readers are measured. The initial POC
percentages do not describe this validated API. Raw samples and test logs are
retained locally in ignored `benchmarks/results/hardened-merge/`.

## Next integration gate

Promotion of the production merge path requires representative
memory/cancellation measurements and passing PostgreSQL lifecycle, ranking,
recovery and concurrent reader/writer campaigns. In particular, verify that CPU savings reduce
locked merge stalls without worsening reader or writer tails. The unlocked-fold
experiment remains independent and unmerged. The on-disk SIMD migration still follows
the separate [architecture decision](../adr/0001-preserve-posting-order-before-changing-encoding.md).
