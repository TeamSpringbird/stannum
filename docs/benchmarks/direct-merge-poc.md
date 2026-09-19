# Direct posting merge proof of concept

The test-only implementation in `segment/src/direct_merge_poc.rs` answers whether
merging already-sorted postings can remove the current document reconstruction,
regrouping and sorting round trip without changing segment bytes. The answer is
promising: all compared bytes match, and the synthetic merge probe is 2.3–3.8 times
faster than the codec-improved reference. This is sufficient to proceed with
production hardening, not sufficient to change the on-disk encoding or claim a
database throughput improvement.

The [architecture decision](../adr/0001-preserve-posting-order-before-changing-encoding.md)
separates direct merge, SIMD encoding and publication changes. The POC is based on
`a69b3b5` (PR #9), independently of the unlocked-fold experiment in PR #10.
It is compiled only for tests. No production merge path, SQL interface, disk
format or locking behavior changes.

## What it does

A min-heap merges the document tables in CTID order, rejects duplicate live CTIDs
and builds a live document-length lookup. A second min-heap walks sorted term
dictionaries; for each term, a heap merges its posting cursors. Each source's dead
set filters its own postings. Positions use a reusable scratch vector; the existing
postings/payload builders emit the result and reconstruct score bounds. Empty terms
are omitted. The output uses the requested existing LSG1/2/3 encoding.

The reference parses each input, reconstructs forward records, ingests them using
PR #9's optimized `add_record`, and finishes with the existing builder. Both paths
include parsing and output encoding in their timing. Fixture construction and
byte comparisons are outside the timed region. Neither path reads the PostgreSQL
heap or calls the server.

## Correctness

Deterministic tests compare complete bytes for every output format with mixed
LSG1/2/3 inputs, interleaved and disjoint CTID ranges, no/some/all deletions, no
inputs, empty inputs, sparse CTIDs, maximum valid CTID components, Unicode terms,
long position gaps, duplicate live CTIDs and dead CTID reuse. A 32-case property
test varies fan-in, document count, vocabulary, token count, deletes and ordering.
The whole-segment verifier additionally checks deterministic output, including
positions, document lengths and score bounds, allowing only the expected LSG1
legacy-header warning. The full core workspace passed 445 unit tests and four
documentation tests; segment Clippy passed with warnings denied, and formatting
passed. PostgreSQL integration tests were not rerun for this test-only POC.

These are valid-input equivalence checks. The POC is not a hardened public merge
API: it assumes one dead set per input and canonical, valid source segments;
it does not promise parity with the reference on corrupt inputs. In particular,
it carries stored document lengths/buckets whereas the reference reconstructs
some metadata from positions. Production promotion must define and test input
validation rather than relying on valid-input equivalence alone.

## Local release measurements

Apple M4 Max, native ARM64, Rust 1.96.0, release profile. Each configuration has
ten paired rounds with alternating order; the first two are discarded. Values
are medians of eight samples per method. Eight mixed-format input segments are
used, and every seventh input document is dead.

| Documents per segment | Tokens/document | Vocabulary | CTID ranges | Reference ms | Direct ms | Ratio |
| ---: | ---: | ---: | --- | ---: | ---: | ---: |
| 32 | 40 | 4 | Interleaved | 0.479 | 0.150 | 3.2× |
| 512 | 400 | 4 | Interleaved | 9.867 | 4.326 | 2.3× |
| 512 | 400 | 400 | Interleaved | 249.447 | 66.318 | 3.8× |
| 512 | 400 | 400 | Disjoint | 223.239 | 60.925 | 3.7× |

Output sizes are identical between paths: respectively 12,377; 1,456,636;
6,062,488; and 6,062,488 bytes. These synthetic fixtures are not a representative
text corpus. No WAL, I/O, locks, PostgreSQL transactions or concurrent readers are
measured. Peak memory and allocation counts have not been measured. Input blobs,
output buffers, per-term encoding buffers and an O(live documents) length lookup
remain resident. Ordered cursor traversal does not make the whole operation
bounded-memory. No explicit SIMD instructions were added.

Reproduce correctness and timings with:

```sh
cargo test -p segment --release direct_merge_poc -- --include-ignored --nocapture --test-threads=1
```

The probe prints all retained timing samples. The initial local raw log is retained
under ignored `benchmarks/results/direct-merge-poc/microprobe.log`.

## Next work, in order

1. Keep PR #9's independently validated ingestion improvement ready for review;
   do not couple it to the unresolved unlocked-fold regression in PR #10.
2. Harden a production direct-merge API, including malformed-input policy,
   cancellation, checked extent/count conversions and input ownership. Measure
   allocations and peak memory; evaluate the remaining document-length lookup.
3. Integrate under the existing storage lock and run PostgreSQL lifecycle,
   recovery, ranking and concurrent maintenance campaigns on both architectures.
4. Address the optimistic-fold regression with measured build-discard and
   lock-reacquisition costs; this is separate from the direct-merge algorithm.
5. Prototype integer block encodings independently. Migrate only after the
   architecture decision's codec, compatibility and recovery gates pass.
