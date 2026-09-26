# Benchmarks

Stannum is measured on the workloads PlanetScale published for TIN, at a scale
where the index does not fit in memory. Every run also checks its answers: a
faster query that changes the result is not an improvement.

## Current results

### AWS, 150 million rows

Run r8 at commit `313a696`, 2026-09-26, on the published protocol (below):

| Workload | Queries/s | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: |
| mixed | 269.9 | 15 ms | 114 ms | 172 ms |
| conjunction-phrase | 437.9 | 12 ms | | 91 ms |
| disjunction-updates | 127.6 | 40 ms | | |

The disjunction-updates run applied 254,224 updates in 600 seconds (p50
1.1 ms, p99 5.5 ms) with 0 errors. The count and ranked checks were clean
before and after the updates.

PlanetScale's TIN announcement reports 199 queries a second for the mixed
workload on the same instance type and corpus. That figure is PlanetScale's own
measurement, not one taken beside Stannum in the same run.

### Local, 150 million rows

On an Apple Silicon laptop (arm64 Docker under OrbStack), with the container's
device reads capped at 20,000 IOPS, 2026-09-26, commits `33e6c22` and
`a5cd99f`: the mixed workload ran at 373 to 377 queries a second, p50 12 ms,
p99 116 ms, with 0 ranked mismatches.

The local database answers the same queries over the same corpus, so pages
touched and candidates scored carry over to the AWS host directly. Throughput
does not: in the two AWS runs of the current format, the host ran the mixed
workload at 0.81 and 0.75 of the same build's local figure.

## Datasets

All corpora are PlanetScale's prepared datasets, fetched and checksum-verified
by `benchmarks/published_dataset.py` from the pinned
[paradedb-benchmarker](https://github.com/planetscale/paradedb-benchmarker)
revision `f487fbaa`. No row is truncated, normalized, remapped or reordered.

| Dataset | Rows | Used for |
| --- | ---: | --- |
| Stack Exchange | 150,000,000 (84.5 GB of CSV) | Headline runs, on AWS and locally |
| Stack Exchange, 15 million row prefix | 15,000,000 | Fast local iteration ("the mock") |
| Wikipedia | 5,032,104 | Count workloads and earlier campaigns |

The Stack Exchange trace has 1,254 query records. Each expands to a
conjunction, a disjunction and a phrase form of the same words, 3,762 forms
in all, run as ranked top-ten queries (`ORDER BY` score `LIMIT 10`).

## Workloads

The driver is PlanetScale's benchmarker (Go and k6), pinned and run through
`benchmarks/tin.py`, which adds the Stannum adapter, isolated containers, the
correctness gates and provenance.

- **mixed**: every form of the trace, conjunctions, disjunctions and
  phrases, read-only.
- **conjunction-phrase**: only the conjunction and phrase forms, read-only.
- **disjunction-updates**: the disjunction forms while a separate connection
  updates rows at a paced rate. The run fails if any update errors or
  misses the driver's deadline.

The published protocol, used on AWS: an i7i.8xlarge instance in us-east-1,
PostgreSQL 18 in a container with 8 CPUs, 32 GB of memory and 24 GB of shared
buffers, 8 clients, 600 seconds per workload after warmup. The local 150
million row runs use the same container sizes and clients. Because Docker's
virtual disk is served from the host's page cache, local runs cap device
reads to what the instance's NVMe delivered, and drop the VM's page cache as
measurement starts so the index is read cold except for shared buffers. The
15 million row mock scales the same regime down: 2 GB of shared buffers in a
5 GB container, eight clients on eight CPUs.

## How correctness is checked

Speed and correctness are measured in the same run, outside the timed
interval:

- **Membership and counts.** Before timing, a sample of query forms is
  checked for exact result sets on a sample of rows: the indexed candidate
  set against the same operator evaluated without the index. Full-corpus
  counts of every checked form are recorded. Update runs repeat the checks
  after measurement.
- **Ranked results.** Sampled ranked forms compare the scores of the top
  ten with exhaustive scoring of every match on the same engine, allowing
  any selection among tied rows. A **ranked mismatch** is a form whose pruned
  top ten differs from that exhaustive answer. This proves the pruning exact,
  not the BM25 formula itself.
- **Differential oracle.** `benchmarks/oracle.py` runs 47 query shapes over
  five mutation states against a reference engine: upstream Lead in CI on
  every push, and PlanetScale TIN by hand. See
  [compatibility](compatibility.md#reference-oracle-against-lead).
- **Conformance.** The [TIN conformance suite](../conformance/README.md)
  checks Stannum against TIN 1.0.3's recorded answers.

The pgrx suite also compares the pruned top k against exhaustive scoring bit
for bit, and the ranked-scan fuzzer checks it under concurrent writes; see
[testing](testing.md).

## Reproducing

Run from the repository root. [Testing](testing.md) covers the correctness
suites; the benchmark tools live in `benchmarks/`, each with `--help`.

1. Fetch a corpus:
   `python3 benchmarks/published_dataset.py --corpus stackexchange --output DIR`.
2. Build an image from the working tree:
   `python3 benchmarks/tin.py build-image --image TAG --output DIR`
   (`--base postgres:18-trixie` builds a native image for local runs).
3. Run a workload:
   `python3 benchmarks/tin.py run --published-corpus stackexchange --dataset DIR --image TAG --workload topk --style mixed ...`.
   `--save-database` keeps the built, vacuumed and checked database so later
   runs start from it with `--load-database`; `--validation-queries` and
   `--ranked-validation-queries` size the correctness sample.
4. For local runs that do not fit in memory, `benchmarks/local/` wraps these
   steps with the read caps and cache drops described above; see
   [its README](../benchmarks/local/README.md).
5. `benchmarks/aws/` holds the CloudFormation stack and host scripts for the
   AWS protocol.

Each run writes a manifest (source and image identity, corpus hashes,
settings), the correctness outputs, per-query and per-family latency, and
resource counters to its output directory. Keep results and connection
credentials out of Git.
