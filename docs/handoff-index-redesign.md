# Handoff: redesign the index layout

Stannum answers the published Stack Exchange ranked workloads at 17.8 queries a
second where PlanetScale's TIN publishes 199, on the same instance type and
corpus. This document records what we measured, why the gap is the index layout
rather than the query code, what to build instead, and how to measure it without
an AWS campaign. Backwards compatibility is explicitly not a concern: there is no
production usage, old indexes need not load, and `REINDEX` is an acceptable
migration.

Branch `perf/ranged-reads`, head `af24e4c`, pushed to remote `stannum`. Working
tree clean; the full pgrx suite and the install, lifecycle, upgrade and reference
oracle gates all pass there.

## The finding

At 150 million rows the index is 246 GB against 24 GB of shared buffers, so
almost every query reads from disk: 26 MB and about 1,500 pages per query,
which saturates the NVMe at eight clients. TIN's index for the same 85 GB corpus
is 50.7 GB, and it reads 1.7 MB per query.

PlanetScale's own guidance, from
<https://planetscale.com/blog/anatomy-of-a-postgres-search-engine> and
<https://planetscale.com/blog/introducing-tin>:

- An English text index with positions and frequencies is normally 30-50% of the
  corpus; TIN measured 33-61% across datasets.
- Positional data is stored apart from the postings so it is loaded only when
  needed.
- Visibility comes from a per-segment liveness bitmap of one bit per `ctid` plus
  the page-level visibility map; for all-visible pages TIN touches no heap page.
- TIN stores one representation of the document set: two-level `ctid` bitmaps,
  approaching one bit per posting for frequent terms.

Ours is 109% of the corpus live, and 310% as a relation. Every measurement below
is reproducible with the tooling in the last section.

## What we measured

Live segment bytes, 15,000,000-row mock, ten segments, 7,057,883,909 bytes total
(`script/dump-segments.py` then the `breakdown` example):

| section | bytes | share |
| --- | --- | --- |
| payload (positions and frequency buckets) | 3,134,821,907 | 44.4% |
| ordinals (document sets as ordinal bitmaps) | 1,840,605,506 | 26.1% |
| postings (the same document sets as TID lists) | 1,718,764,992 | 24.4% |
| dictionary | 273,810,325 | 3.9% |
| lengths | 59,999,440 | 0.9% |
| docs (ordinal to TID table) | 23,278,940 | 0.3% |
| page tables | 6,602,448 | 0.1% |

Two conclusions fall straight out. **The document set is materialised twice**, as
TID postings and again as ordinal streams, which is 50.5% of the index; the
ordinal streams were added for `LSG4` on top of the postings rather than
replacing them. And **positions are interleaved with the frequency bucket** in one
payload stream, so scoring a candidate reads pages that are mostly positions it
does not need.

Terms with `df >= 128` are 1.1% of terms and 86% of the bytes.

The relation is 20 GB holding those 7.06 GB, so 65% is retired runs and free
pages. `af24e4c` bounds reclamation per insert, which fixes the latency spike it
caused, but not the space.

Disk reads per ranked query in steady state on the mock (2 GB shared buffers
against a 21.7 GB index, 360 distinct published queries, 120 per style): 318
pages, 2.43 MB. By relation, index 92% and heap 8%. The heap is 49 visibility
checks per query, 25 of them random reads. Within the index, by area, now that
every scan reports its counters (see below): payload 42%, ordinals 28%, TID
postings 27%, and 2% planning. Scorer setup (capturing the view plus a
dictionary lookup per term per segment) is 14%, inside those areas.

| style | index pages | ordinals | payload | postings | heap | visibility checks |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| conjunction | 307 | 154 | 76 | 56 | 16 | 20 |
| disjunction | 205 | 90 | 115 | 0 | 58 | 125 |
| phrase | 368 | 1 | 182 | 184 | 1 | 0 |

Phrase queries are the heaviest readers and read only the two areas the
redesign removes or moves: the TID postings and the interleaved positions.
Disjunctions read no postings at all; their payload reads are the frequency
buckets the scorer needs, dragged in with positions. Measured with
`benchmarks/local/attrib.py`.

### Negative results worth not repeating

- Reading the document length lazily, only after a bound that costs no read, is
  correct and takes the disjunction median from 193 ms to 114 ms. Throughput did
  not move at all: 55.3 to 55.2 queries a second. Latency is not the constraint;
  bytes are.
- Shrinking the length window sixteenfold cut bytes copied by 42% and disk reads
  by 0.3%, because disk is page-granular.
- Raising the per-backend read cache to 256 MB eliminates disk reads in a
  small probe, but that is an artifact of a sixteen-query working set fitting in
  it, and eight backends at 256 MB exhausted the container, reproducing the
  out-of-memory kills that ended two AWS campaigns.
- Chunk-level skipping in the disjunction walk already works: 446 of 2,760
  candidate chunks are loaded. Seeding the walk with the true tenth-best score,
  which is the best any champion-list scheme could achieve, cut candidates scored
  by 43% and pages by 16%, and by nothing at all on the large disjunctions.

## The target

Roughly a four to five times smaller index. At 50 GB against 24 GB of shared
buffers the workload is mostly cached and CPU-bound, which is the regime where we
already measure well: the same build on the 15 million row prefix, with the index
resident, does 282 queries a second and beat TIN on the Wikipedia count workload
at 12,362 against 10,260.

## The layout to build

1. **One document-set encoding.** Keep the ordinal bitmaps, delete the TID
   postings entirely, and serve everything that reads postings today from the
   ordinal streams plus the page table. This is the single largest item and it is
   removal rather than invention. Check every consumer: the count fold, the
   candidate stream for unordered scans, phrase skeletons in `tinql`, merges,
   `verify`, and the dead-list intersections.
2. **Frequency inline with the document set.** Put the term-frequency bucket
   beside the membership bits in the ordinal chunk so scoring never touches the
   payload area. Dense chunks grow; measure the trade against the payload reads
   it removes.
3. **Positions in their own area, read only by phrase verification.** They are
   44% of the index today and are dragged through the scoring path for every
   candidate.
4. **A per-segment liveness bitmap, one bit per document**, consulted with the
   page-level visibility map, replacing the per-candidate heap fetch. Ours is 49
   random heap reads per query to return ten rows.
5. **Compact document lengths.** A one-byte length class for bounding, with the
   exact length read only for rows actually admitted, keeps scores bit-identical.
6. **Fewer, larger segments.** Scorer setup is 13% of reads and scales with the
   segment count: ten locally, eighteen at 150 million rows. The 3 GiB
   `SEGMENT_BYTES_CAP` and the `u32` run length are the current limits.

Items 1 and 3 are where the size comes from. Items 2, 4 and 5 are where the
per-query read count comes from.

## Order of work

1. **Close the attribution gap first.** Done 2026-09-23. PostgreSQL reported
   105,532 index page reads where the extension's counters accounted for
   58,335. Nothing unknown was reading the index: the EXPLAIN callback printed
   the area and phase counters only for scans that pruned by ordinal, so every
   phrase query (scored from the candidate stream) and every conjunction that
   fell back reported nothing. With the counters printed for every scan and
   reset at the start of unordered and count scans too, the extension accounts
   for 103,038 of the 105,522 pages and planning for the other 2,484. The
   table above is the corrected breakdown; the previous one understated payload
   and postings by leaving phrase queries out entirely.
2. Delete the TID postings (item 1). Measure size and reads.
3. Split positions out and put frequency inline (items 2 and 3). Measure.
4. Liveness bitmap and compact lengths (items 4 and 5). Measure.
5. Segment sizing (item 6), which is configuration plus lifting two limits.

Re-measure after each step rather than at the end: three hypotheses were
falsified today by measuring, and each one looked obvious beforehand.

## How to measure

Everything below runs on a laptop and needs no AWS. See
`benchmarks/local/README.md`.

The mock reproduces the out-of-memory regime: the 15 million row prefix, a 21.7
GB index against 2 GB of shared buffers in a 5 GB container, eight clients on
eight CPUs. Two details make a Docker container behave like the instance. Its
disk is served from the host's page cache at memory speed, so reads are capped
through `STANNUM_DOCKER_RUN_ARGS`, which `benchmarks/tin.py` passes to
`docker run`. And copying and validating the database leaves the index in the
VM's page cache, uncharged to the container, so the runner drops that cache when
the measurement starts.

- Build the database once: `benchmarks/local/mock-build.sh`, about 25 minutes.
  It is saved and reused by every later run.
- A workload: `benchmarks/local/mock-run.sh IMAGE LABEL STYLE UPDATES [SECONDS]`
  reports queries a second, latency, disk read per query and CPU.
- Per-query detail: `benchmarks/local/mock-probe.py IMAGE LABEL [N]` reports
  pages touched, candidates scored, disk read and time with the cache dropped
  before each query.
- Read attribution: `benchmarks/local/attrib.py IMAGE [WARM] [STEADY]` runs a
  warm-up slice and then a disjoint steady slice of the published queries and
  reconciles three views of every page read: `pg_statio` by relation, the
  EXPLAIN node counters, and the extension's area and phase counters, per
  style. It needs `psycopg`; `/tmp/stannum-venv/bin/python` has it on this
  machine.
- An iteration is a new image plus a five-minute run:
  `python3 benchmarks/tin.py build-image --image stannum-bench:arm64-<tag> --base postgres:18-trixie --output /tmp/<tag>`.

Read accounting, all behind `EXPLAIN (ANALYZE, BUFFERS)` on the custom scan:
`Bytes Fetched` and `Disk Pages By Area` per segment area, `Disk Pages By Phase`
for paths outside the segment reader, `Walk Setup Blocks`, `Walk Body Blocks`,
`Chunks Loaded`, `Visibility Checks`. `segment::cache::set_disk_probe` is the hook
that lets the segment crate attribute storage reads. `stannum.debug_seed_score`
prunes a walk against a known threshold, which is how the champion-list idea was
tested and rejected.

Index size: `script/dump-segments.py --dbname DB --index NAME --out DIR
--data-directory PATH` dumps segment blobs (the `--data-directory` override is
for a server in a container whose directory is bind-mounted), then
`cargo run -p segment --release --example breakdown -- DIR/*.segment`.

Correctness on every change: `cargo pgrx test pg18 -p stannum` and the four
gates. The pgrx suite includes a test that compares the pruned top k against
exhaustive scoring bit for bit, which is the one that catches pruning mistakes.
Use `python3 /tmp/stannum-pgrx-lock.py -- <command>` to serialise pgrx runs, and
reinstall the release build afterwards, since `cargo pgrx test` overwrites it.

A 322 GB snapshot of the built 150 million row database is at
`s3://springbird-dev-stannum-corpus-cache-860510875764/postgres-snapshots/stackexchange-150m-lsg5-7372fc6.tar`,
so a full-scale run restores in about fifteen minutes instead of rebuilding for
3.7 hours. It is only valid while the segment format matches, so this redesign
retires it; rebuild once the new format settles.

## Also outstanding

- The published write workload at 150 million rows has never completed. `af24e4c`
  fixes the reclamation stall that failed it; the run itself was never retried.
- The article's four charts carry full-corpus Stannum runs for the mixed and
  conjunction-phrase workloads and prefix runs for the write workload. The count
  chart is from an older build. The importer replaces the series per chart, so
  re-importing a newer run is enough.
