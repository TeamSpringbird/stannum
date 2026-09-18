# Run a local benchmark

Use this guide to measure a Stannum build. The [results summary](README.md)
explains what has been measured and which comparisons are still planned.

You need Docker with native ARM64 Linux support, Python 3, and PostgreSQL 18
client tools (`psql` and `pgbench`) on `PATH`. The campaign builds the extension
in a container and creates a fresh database volume for each trial.

## Smoke test

Run from the repository root:

```sh
python3 benchmarks/campaign.py --build \
  --image stannum-bench:local \
  --output benchmarks/results/smoke-01 \
  --engines stannum --profiles count mixed \
  --repetitions 1 --rows 10000 --seconds 30 --warmup 5 \
  --clients 2 --write-rate 20
```

This synthetic run checks the setup; it is not the 100k benchmark. Use a new
output directory for every campaign. Avoid other benchmark traffic or heavy
builds during measurements.

## Wikipedia corpus

Prepare the corpus once, or reuse an existing checksummed dataset:

```sh
python3 -m venv benchmarks/results/data-venv
benchmarks/results/data-venv/bin/pip install pyarrow==23.0.1
benchmarks/results/data-venv/bin/python benchmarks/dataset.py \
  --output benchmarks/results/datasets
```

Preparation downloads about 11.6 GB of source shards and produces nested 100k
and 1m samples. We evaluate only the 100k sample. Each dataset retains its source
revision, seed, normalization, attribution, checksums, and expected match sets.

```sh
python3 benchmarks/campaign.py --build \
  --image stannum-bench:local \
  --output benchmarks/results/wiki-100k-01 \
  --dataset benchmarks/results/datasets/wikipedia-100000 --rows 100000 \
  --engines stannum --profiles count mixed --repetitions 5 \
  --seconds 300 --warmup 30 --clients 2 --write-rate 20 \
  --statement-timeout-ms 1800000
```

The default server budget is four CPUs and 4 GiB memory, with durability and
autovacuum enabled. Clients run outside that budget. Treat these runs as warm-cache
measurements. The writer changes a reserved suffix while preserving expected
query memberships; this does not exercise changing match sets or insert/delete
workloads.

## Read the results

Open `report.md` in the output directory. The campaign also retains individual
runs, correctness checks, query plans, source fingerprints, logs, and resource
counters. Failed trials remain part of the report.

```sh
python3 benchmarks/campaign.py --report-only \
  --output benchmarks/results/wiki-100k-01
```

Pin builds and hold the corpus, settings, client load, and write schedule constant
when comparing results. Alternate build order and compare repeated runs. The
current runner's `stannum` adapter does not run the original `tin`-named Lead
binary; the historical comparison below used its archived harness. A new paired
Lead-to-Stannum campaign still needs that adapter compatibility work.

## Historical 100k comparison provenance

The [reported results](README.md) come from these September 17, 2026 campaigns:

- Lead: `wikipedia-v1-20260917/wikipedia-100000`.
- Stannum: `lead-current-100k-20260917-230423/campaign`, based on `3906c28`
  plus uncommitted changes, before the rename.

The retained source snapshots, patches, manifests, and image identities are needed
to reproduce those builds; a commit hash alone is insufficient. Raw artifacts
remain local. These are historical measurements, not results for the current
checkout.
