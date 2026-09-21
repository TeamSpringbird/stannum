# Current postings decoding diagnostic

This isolates the existing codec iterators from the multi-term unions and segment
merges already measured by `segment/examples/count_paths.rs`. It changes no
production code, storage format, query strategy, or SIMD implementation.

Run on an Apple M4 Max (arm64), Rust 1.96.0, release profile, September 20, 2026:

```sh
CARGO_BUILD_JOBS=2 cargo run -p segment --release --example decode_postings -- 12000 9 4
cargo run -p segment --release --example decode_postings -- 36000 9 4
```

Arguments are populated heap pages, odd repetition count, and passes per sample.
Every sample reports milliseconds per complete pass. The CSVs retain all nine
samples in measurement order, plus median/minimum/maximum. Four passes amortize
clock overhead. Each repetition rotates the three modes; all modes are warmed
before timing. Fixture generation, encoding, and exact TID comparison against
independently constructed expected vectors are untimed. Empty streams and the
last valid TID are also checked. Timed outputs are consumed through `black_box`;
count/checksum checks occur after timing.

Each input is one contiguous byte buffer, built with the current `PostingsBuilder`
without score bounds. The builder chooses the actual sparse or grouped encoding;
this experiment does not force an artificial codec. The sparse far-page fixture
uses block stride 257; all others use consecutive block numbers. Offset density
ranges from one to 291 postings per populated page. Offsets are consecutive,
which is deliberately simple and reproducible rather than a corpus distribution.

Scalar and page-expand both enumerate every TID and compute the same count and
checksum. Page-count uses bitmap popcount without expanding offsets and only
computes count. Its gain includes avoided work, not just faster decoding. Parse,
cursor construction and existing dynamic dispatch are included in each pass.

At 36,000 populated pages, median milliseconds per pass were:

| Offsets/page | Actual codec | Scalar | Page expansion | Page count |
|---|---|---:|---:|---:|
| 1, block stride 257 | sparse | 0.168 | 0.355 | 0.298 |
| 1 | sparse | 0.127 | 0.284 | 0.246 |
| 4 | sparse | 0.469 | 0.702 | 0.608 |
| 8 | sparse | 0.944 | 1.235 | 1.088 |
| 18 | sparse | 2.117 | 2.805 | 2.520 |
| 19 | sparse | 2.261 | 2.985 | 2.670 |
| 64 | grouped | 3.633 | 1.474 | 0.295 |
| 291 | grouped | 15.790 | 6.159 | 0.300 |

The boundary fixture names refer to the grouped page-list/bitmap threshold of 18;
at these whole-stream distributions the writer still chooses sparse encoding for
both 18 and 19 offsets/page. They do not imply grouped lists were measured.

The result supports retaining different execution paths. For a single sparse
posting stream, page adaptation adds work. Dense grouped streams expand faster
through page iteration here, and count-only execution avoids most per-TID work.
These measurements do not contradict a page union winning over many individually
sparse terms: union/merge costs and final result density are absent from this probe.

The smaller 12,000-page run has the same directional result; see raw distributions
in [postings-decode](postings-decode/). It is a shared workstation with no affinity,
frequency control, or exclusive CPU reservation. The earliest small sparse samples
show frequency/warm-up drift, so sub-millisecond differences are diagnostic only.
The larger retained run was executed after this worktree's test/build completed;
other agents may still have used the workstation. Expected vectors remain resident.

This is not SQL latency, an AWS/TIN comparison, or a memory-pressure benchmark.
There is no MVCC, heap access, score-bound lookup, multi-term Boolean operation,
segment merging, concurrent query load, allocation profiler, or hardware counter
collection. Do not extrapolate the dense count ratio into end-to-end speedups.
Use the AWS profiles to determine whether these operations matter before selecting
batching/SIMD work. Representative irregular offsets and mixed grouped list/bitmap
pages can extend this same diagnostic if those profiles justify it.

Validation: all fixtures agree by exact TID and count/checksum; 119 segment library
tests pass (four existing tests ignored); source-header check and six attribution
tooling tests pass.
