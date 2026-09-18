# Local Stannum campaigns

The current results, 100k protocol, limitations, and TIN comparison plan are in
[benchmark results and plan](README.md). Benchmarks run locally, not in CI. No artifacts
are uploaded automatically. Code and metadata use `stannum`; historical result
folders and source snapshots retain their original names.

## Quick synthetic check

```sh
export PATH="$(brew --prefix postgresql@18)/bin:$PATH"
python3 benchmarks/campaign.py --build \
  --image stannum-bench:local \
  --output benchmarks/results/stannum-smoke-01 \
  --engines stannum --repetitions 1 --rows 10000 \
  --profiles count mixed --seconds 30 --warmup 5 --clients 2 --write-rate 20
```

Every output directory must be new. Each trial gets a fresh container and volume.
For an existing image omit `--build`; the runner verifies the source fingerprint,
architecture, and recipe. It retains logs before removing its own container and
volume. Builds include the `segment` crate, which is also covered by the source
fingerprint. Use immutable image IDs for comparisons.

The default local engine list is `stannum gin paradedb pg_textsearch`. With count
and mixed profiles, five repetitions produce 35 trials; GIN does not participate
in mixed/ranked BM25 comparisons. `tin` is available only as an explicit adapter
for an externally provisioned PlanetScale TIN installation, not the local image.

## Resource controls and reports

Trials use native Linux ARM64 PostgreSQL 18.6, four VM CPUs (cpuset 0–3), 4 GiB
memory, no swap, and 1 GiB shared-memory capacity. The memory limit includes shared
memory. Data lives in fresh Docker named volumes. The native macOS clients use
loopback TCP and run outside the server's resource budget.

Settings include 1 GiB shared buffers, 16 MiB work memory, 512 MiB maintenance
memory, enabled autovacuum, fsync, synchronous commit, and full-page writes. See
`campaign.py` for the complete settings. These are ceilings, not CPU reservations;
background containers, host activity, and VM scheduling can affect timing.

The runner rotates engine order and uses the same seed per repetition across
engines. Warmup warms shared caches; timed readers open new sessions. Host and VM
caches are not forcibly purged. `caffeinate` prevents idle sleep during the
campaign. Avoid overlapping traffic or heavy builds. Campaign and paired runners
use separate locks, so their mutual exclusion requires coordination.

The runner records before/after correctness, ranked membership/order checks,
query plans, raw pgbench transaction logs, build time, index bytes, write latency
and throughput, source/build identity, and container resource counters. All trials
are retained, including failed ones. `report.md`, `aggregate.json`, and
`history.csv` contain the summaries. Rebuild a report without rerunning traffic:

```sh
python3 benchmarks/campaign.py --report-only --output benchmarks/results/stannum-smoke-01
python3 benchmarks/run.py history benchmarks/results/stannum-smoke-01 > benchmarks/results/stannum-smoke-01/history.csv
```

Use medians as the main local trend and retain min/max and dispersion. Five samples
allow a trimmed mean per metric; failures are never trimmed away. An incomplete
group is not a valid repeated-run speed comparison. Ranked throughput does not
establish equal relevance or BM25 semantics across engines.

## Wikipedia corpus

`dataset.py` prepares checksummed nested samples from the English November 2023
Wikimedia Wikipedia dataset. The source revision, seed, normalization, attribution,
and query membership oracle are recorded in each manifest. It downloads all 41
source shards (about 11.6 GB compressed). Preparation needs **pyarrow 23.0.1**;
the runners use Python's standard library.

```sh
python3 -m venv benchmarks/results/data-venv
benchmarks/results/data-venv/bin/pip install pyarrow==23.0.1
benchmarks/results/data-venv/bin/python benchmarks/dataset.py \
  --output "$HOME/Library/Application Support/StannumBenchmarks/datasets"
```

Existing frozen datasets under `LeadBenchmarks/datasets` remain usable and should
not be modified. Each sample includes `documents.csv`, `attribution.csv`,
`matches.csv`, and `manifest.json`. Downloads resume; partial output datasets
require inspection before retrying. Preparation materializes both 100k and 1m
samples, but the **evaluation scope is 100k only**.

Run the 100k series with an already built and verified image:

```sh
caffeinate -i python3 benchmarks/baselines.py \
  --datasets "$HOME/Library/Application Support/LeadBenchmarks/datasets" \
  --output "$HOME/Library/Application Support/StannumBenchmarks/campaigns/wiki-100k-01" \
  --image stannum-bench:local
```

`--sizes` defaults to `100000`; only an explicit `--sizes 1000000` launches the
million-document evaluation. Measurement windows are five minutes at 100k and
thirty minutes at 1m. The series freezes the harness and image ID. Each scale has
five repetitions per supported local engine/profile. A failed trial is retained
and later trials continue. `status.json` and `campaign.json` record progress.
Detached processes survive terminal closure, not reboot. A completion watcher,
`notify.py`, can request local macOS notifications; delivery depends on system
settings. It does not post to chat.

## Paired Stannum builds

`paired.py` compares two Stannum images in alternating order with fresh volumes,
identical settings, and a shared seed within each pair:

```sh
python3 benchmarks/paired.py \
  --output benchmarks/results/paired-stannum-01 \
  --original-image ORIGINAL_IMAGE_ID --original-source ORIGINAL_SOURCE_JSON \
  --fork-image CANDIDATE_IMAGE_ID --fork-source CANDIDATE_SOURCE_JSON \
  --profile mixed --rows 10000 --repetitions 5 --seconds 30 --warmup 5
```

Both images must use the Stannum package and current recipe/provenance labels.
This tool does not mix historical `tin`-named Lead binaries with renamed Stannum.
Use archived protocols to inspect historical artifacts. Aggregate paired speedups
are withheld until every planned pair passes correctness, protocol equality, and
OOM checks. A 10k synthetic result is an iteration check; use the frozen 100k corpus
for the current comparison baseline.

## Historical 100k comparison provenance

The [Lead-to-Stannum results](README.md) were measured on September 17, 2026.
The Stannum candidate was commit `3906c288ec2142133439cec4b31364d89b2515a3`
plus uncommitted changes, before the extension rename. Reproduction requires the
retained source snapshot, per-file hashes, patch, and image; the commit alone is
insufficient. These measurements do not describe the current rebased build.

Original campaign: `wikipedia-v1-20260917/wikipedia-100000`.
Candidate: `lead-current-100k-20260917-230423/campaign`.
These names deliberately preserve the historical artifact identity.

```text
Original image:
sha256:028b8e940aa33846aeb537e9097532e76e42b58dc3a03b32b2f8cba453728532
Candidate image:
sha256:1f12be2ffd865eb18a94fb21a91ece6b0ca82530e95cdd1bb57fe8f18a8dfd8c
```

The development machine retains these folders beneath
`~/Library/Application Support/LeadBenchmarks/campaigns/`, including manifests,
source snapshots, plans, logs, resource counters, and comparison reports. They are
local evidence, not downloadable public artifacts. Generated corpora and raw
results are excluded from Git. Keep legacy folders and frozen protocols unchanged;
new Stannum builds use new output folders and image tags.

The frozen corpus uses dataset revision
`b04c8d1ceb2f5cd4588862100d08de323dccfbaa` and seed 1729. Its normalized
100k bodies contain 278,979,934 bytes before the mutable suffix. Normalization
concatenates title/text, keeps lowercase ASCII letter sequences, and caps each
document at 8,192 tokens. Trials used 30 seconds of warmup and 300 seconds of
measurement with two readers and one writer requested at 20 updates/second.
The writer toggles a reserved suffix without changing tested query memberships.
