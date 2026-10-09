<!--
Copyright (C) 2026 Ben Weis <ben@springbird.app>

See LICENSE in the repository root for license terms.
-->

# Measuring the query engine outside PostgreSQL

The ranked walk, the count fold and the BM25 arithmetic live in the `engine`
crate, which does not link PostgreSQL. The `bench` crate runs them over a
dumped index in seconds, so a change to a data structure, a memory layout or
a kernel can be measured and checked before it is validated in a server.
There are three levels, fastest first:

| level | what | command |
| --- | --- | --- |
| kernels | the walk's and the fold's hot loops, on synthetic blocks and a real segment's | `script/bench-native` |
| replay | a query trace over a dumped index: answers, latency, page touches, pruning | `cargo run -p bench --release --bin replay -- ...` |
| server | the extension in PostgreSQL | [testing](testing.md), [benchmarks](benchmarks.md) |

## Crate layout

| crate | holds |
| --- | --- |
| `engine` | `bm25` (scorers and bounds), `walk` (the ranked ordinal walk: block-max WAND, MaxScore's essential terms, the word sieve, required terms, the conjunction warm-up, phrase and mixed-shape checks), `fold` (the Boolean count over ordinals), `terms` (a query's scoring terms and their statistics), `spec` (the tokenizer settings a meta page stores) |
| `postgres` | the extension: views, shared buffers and pins, visibility, the score functions' caches, the custom scan; it installs the engine's hooks (`CHECK_FOR_INTERRUPTS`, the corruption report, `pgBufferUsage`) in `_PG_init` |
| `bench` | the dump reader, a paged source that serves a blob as runs are served and counts page touches, the replay, the bound what-if, and the kernel benchmarks |

The engine takes what a server provides through small seams: a source's
bytes through `segment::index::Index` (whose hold spans keep pages pinned
while the walk reads them in place), dead documents as
`segment::dead::DeadDocs`, a candidate's visibility through
`engine::walk::Visibility`, whether a walk's view still stands through the
caller of `Scorer::top_k`, and settings through `WalkConfig`. Its `stats`
and `kernels` features, which only the bench crate enables, add pruning
counters and expose the private word kernels; the extension builds without
them.

Still in `postgres/src`, because they are PostgreSQL's: `score.rs` keeps
the score functions and their per-statement and per-scan caches, scoring a
row by heap location (`IndexScorer::score`, HOT roots), `top_k_streamed`
over the candidate stream, `max_score`, heap scoring and `score_inspect`;
`customscan.rs` the scan nodes, completions past k and the zero-fill from
the candidate stream; `stream.rs` the candidate stream; `fold.rs` the
visibility-map read. The replay reimplements the small pieces it needs of
those (the candidate stream's heap order, scoring a candidate by ordinal),
which `--expect` against PostgreSQL checks.

## Dumping an index

```sh
script/dump-segments.py --dbname DB --index documents_body_idx --id-column id --out DIR
```

It reads the relation file after a `CHECKPOINT`, so it needs the data
directory: the pgrx server and private clusters run as the developer. For a
server in a container, bind-mount the data directory and pass the host's
path as `--data-directory`. It writes each segment and its dead list, a
manifest (tokenizer settings, `k1`, `b`, `score_stop_words`, the directory)
and, with `--id-column`, the id of every heap location. The write buffer is
not dumped: fold it first (a `VACUUM`, or a build with no inserts since), or
the replay scores with other statistics; the manifest records its size and
the replay warns.

A saved benchmark database (`benchmarks/tin.py --save-database DIR`, a copy
of the container's `/var/lib/postgresql`, described by `DIR/snapshot.json`)
is dumped from a container of the image the snapshot names, with a copy of
the directory bind-mounted:

```sh
cp -a SAVED /tmp/db-copy                 # never write the saved copy
docker run -d --name dump -p 127.0.0.1:55432:5432 -e POSTGRES_PASSWORD=postgres \
    -v /tmp/db-copy:/var/lib/postgresql IMAGE postgres
PGHOST=127.0.0.1 PGPORT=55432 PGUSER=postgres PGPASSWORD=postgres \
    script/dump-segments.py --dbname benchmark --index documents_body_idx --id-column id \
    --data-directory /tmp/db-copy/18/docker --out DIR
```

The replay reads segments of the current format (`STN3`). A database built
by an older image whose segments have another signature must be rebuilt
with a current image first (`REINDEX INDEX documents_body_idx`), which at
15 million rows is a long build; the dump script prints each blob's
signature.

## Replaying a trace

```sh
script/replay-oracle.py trace --queries .../stackexchange/queries.json --out trace.tsv
cargo run -p bench --release --bin replay -- --dump DIR --trace trace.tsv \
    [--style conjunction] [--threads 1,8] [--cold] [--whatif] \
    [--out answers.tsv] [--expect answers.tsv] [--per-query queries.tsv]
```

For each query it runs the ranked top 10 of `stannum.score(ctid)` and the
count, as the custom scan does, and reports per style: p50, p99 and mean
latency on one thread and throughput over `--threads` (each thread with its
own readers and caches, as a backend has); page touches per query by area,
warm (readers and caches kept across queries) and `--cold`, also folded into
TIN's categories and into the categories of EXPLAIN's `Page Touches By
Area`; and the walk's chunks and sub-blocks judged and pruned, candidates
examined, classes and lengths read, candidates scored and the final
threshold. `--whatif` recomputes each sub-block bound the walk judged from
the members (it must equal the walk's, bit for bit) and counts the kept
sub-blocks, and their candidates, that a bound at the sub-block's own
shortest member or its exact maximum would have skipped.

Every row is visible to the replay: there is no heap, only the dead lists
VACUUM published.

`--out` writes each answer as ids with score bits, and each count;
`--expect` fails on any difference. To compare with PostgreSQL on the same
index:

```sh
$STANNUM_PYTHON script/replay-oracle.py postgres --dbname DB --trace trace.tsv --out pg.tsv
cargo run -p bench --release --bin replay -- --dump DIR --trace trace.tsv --expect pg.tsv
```

## Kernel benchmarks

```sh
STANNUM_BENCH_DUMP=DIR script/bench-native --save-baseline   # once
STANNUM_BENCH_DUMP=DIR script/bench-native [FILTER]          # after a change
script/bench-native --portable; script/bench-native --against target/bench-native/portable-latest.tsv
```

`script/bench-native` builds `bench/benches/kernels.rs` with
`-C target-cpu=native` in `target/native` (production builds are unchanged)
and prints each kernel against the saved baseline. A kernel is timed as the
median of seven 20 ms batches; expect a few percent of noise between runs.

## Profiling on macOS

Apple's Instruments samples a release binary with call stacks and, on Apple
silicon, reads the CPU's counters (cycles, instructions, L1 and L2 misses,
branch mispredictions) through its CPU Counters template. With Xcode
installed (`xcrun xctrace` needs it; the command-line tools alone do not
include it):

```sh
cargo build -p bench --release --bin replay
xcrun xctrace record --template 'Time Profiler' --output replay.trace \
    --launch -- target/release/replay --dump DIR --trace trace.tsv --style disjunction --no-count
xcrun xctrace record --template 'CPU Counters' --output counters.trace --launch -- ...
open replay.trace
```

`cargo install cargo-instruments` wraps the same (`cargo instruments -t time
-p bench --release --bin replay -- ...`), and `samply record` (`cargo install
samply`) gives a Firefox Profiler view without Xcode. Release builds keep
symbols; add `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` for source lines.
There is no `perf` on macOS; Linux's `perf stat -e
cycles,instructions,cache-misses` works on the same binaries there.

## The ctid-addressed format (phase A)

`cargo run -p bench --release --bin tinshape -- --dump DIR --out OUT` converts
a dump to the TIN-shaped format of [the TIN-shape guide](architecture/tin-shape.md)
(`TNS1`), checks every posting, bucket, position and length against the
dump, writes the blob and a size report by area and document frequency
(`OUT/sizes-LABEL.md` and `.json`), and with `--trace FILE --expect FILE
[--ranked]` replays the trace's counts and ranked top k over it through
`engine::tinshape`, failing on any difference, with latency and page touches
by TIN's EXPLAIN areas. `--block`, `--grid-density`, `--no-paged`,
`--no-ef-groups`, `--no-sparse` and `--fixed-tf` vary the encoder.
