# Million-document memory-pressure investigation

This extends the published-trace measurements from 100,000 to 1,000,000
Wikipedia documents on the local Apple M4 Max. The purpose is to locate the
next bottleneck under constrained cache and additional readers. These are
short diagnostic windows, not a maximum-capacity estimate or a reproduction
of PlanetScale's TIN results.

## Protocol

The extension image is unchanged from the ranked-parameter fix:
`sha256:4130e2a2027f9146f45d02350794a0da9ede40c029193b990919e0ad3542b7db`.
The extension source fingerprint is
`46c805ff821b9d5726106a74e8b0d0e0b61f8f52c9f616728c7cd89c4eac7eab`.
Each run uses a new PostgreSQL 18.6 container and data volume, native ARM64,
four CPUs, no swap, 1 GiB shared buffers, 16 MiB work_mem and 512 MiB
maintenance_work_mem. The host has 128 GiB RAM; Docker has approximately
15.7 GiB. Other local services remain running.

The existing normalized Wikipedia corpus contains 1,000,000 documents;
its CSV SHA-256 is
`fce2bf1c30c5450764dbc9d97d07a50d49f44bd9a5d05bcfafd063f8b37018a5`.
The pinned published trace has 302 records and 906 AND/OR/phrase forms.
The driver, seed and image are held fixed. Each run requests 15 seconds of
warmup and 120 seconds of measured closed-loop traffic, without updates.
Memory caps and client counts are varied independently.

Membership checks cover the usual 1,000-row sample while recording all 906
full-corpus counts. Ranked runs additionally compare all 906 top-10 score
multisets against exhaustive scoring on the full million rows. These checks
are outside timing. They do not replace the separate strict 5,000-document
Lead comparison described in [the verification budget](lead-verification-budget.md).

Resource samples are taken about every two seconds. Measurement deltas must
use only snapshots whose collection began and ended within the driver's
exported measurement interval; the reported covered duration excludes gaps
at each boundary. Build samples and explicit before/after snapshots remain
separate. Memory high-water marks are container-lifetime values; sampled
build peaks are lower bounds on the peak during that phase. Linux pressure
stall metrics are unavailable on this Docker kernel. VM block-device reads
are not proof of physical macOS SSD reads: the host may cache the VM disk.

## Reproduction

Prepare the pinned driver and build the image as described in
[published-trace.md](published-trace.md), then run each configuration serially:

```sh
python3 benchmarks/tin.py run \
  --dataset "$HOME/Library/Application Support/LeadBenchmarks/datasets/wikipedia-1000000" \
  --rows 1000000 --image stannum-bench:ranked-parameters-final \
  --engines stannum --workload topk --memory 8g --shared-buffers 1GB \
  --clients 2 --warmup 15 --seconds 120 \
  --output benchmarks/results/million-topk-8g-c2-r1
```

Use unique output directories and change `--memory`, `--clients` or
`--workload` for the other configurations. The wrapper owns the shared timing
lock; do not wrap this command in a second acquisition.

## Retained unsuccessful attempt

`million-topk-8g-c2` was interrupted before index construction or traffic.
The first sampler required pressure-stall files, which this Docker kernel
does not provide. Its artifacts retain the errors. The sampler was corrected
to represent unsupported metrics as null, and the run restarted in a fresh
container as `million-topk-8g-c2-r1`. No timing from the interrupted attempt
is included in results.

## Completed observations

The 8 GiB, two-client ranked run (`million-topk-8g-c2-r1`) completed all
906 timed query forms: **314.1 QPS, 12.840 ms p95, 30.800 ms p99**. Index
construction took 191.05 seconds. The index occupied 2,763,325,440 bytes;
total relation storage was 4,733,779,968 bytes. All 906 full-corpus ranked
score-multiset checks passed against exhaustive same-engine evaluation;
the separate Lead gate remains the semantic oracle. This is one diagnostic
run, not a repeated speedup or a maximum-capacity result.

The resource summary uses 59 complete samples spanning 118.95 seconds,
with 0.79 seconds omitted at the start and 0.21 seconds at the end. Within
that covered interval it recorded 221.67 CPU seconds, no CPU throttling,
no OOM events, and 3,088,384 bytes of cgroup block-device reads. Sampled
query-phase memory peaked at 4,700,557,312 bytes; sampled build memory
peaked at 4,871,729,152 bytes. The container-lifetime high-water mark was
5,083,041,792 bytes. These are cgroup memory values, including cache,
not solely PostgreSQL working memory or process RSS.

The 2 GiB attempt (`million-topk-2g-c2-r1`) failed during index construction
and its container inspection recorded `OOMKilled: true`. No query timing
was completed. This establishes a build-memory failure under that setup,
not that an already-built million-document index cannot serve queries
with a 2 GiB cap. A follow-up must separate build memory from query memory
before drawing that conclusion.

Run `python3 benchmarks/tin.py report --output <run-directory>` to derive
`resource-summary.json` from retained samples. Missing or insufficient
samples remain explicit rather than being reported as zero resource use.
