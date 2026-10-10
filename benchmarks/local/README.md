# Local rehearsal of the published workloads when the index does not fit in memory

The AWS runs at 150,000,000 rows read a 47 GB index through 24 GB of shared
buffers in a 32 GB container on NVMe. These scripts rehearse the same regime on a
laptop, either at full scale or with the 15,000,000-row prefix, whose index is
likewise larger than its 2 GB of shared buffers in a 5 GB container, with eight
clients on eight CPUs.

Two things make a Docker Desktop container behave like the instance:

- The VM's disk is served from the host's page cache at memory speed, so reads are
  capped at what the instance's NVMe delivered inside its container (about 20,000
  reads and 400 MB a second) through `STANNUM_DOCKER_RUN_ARGS`, which `tin.py`
  passes to `docker run`.
- Copying the saved database and validating it leave the index in the VM's global
  page cache, uncharged to the container, so the runner drops that cache the
  moment the measurement phase starts.

## Environment

Every script reads its paths from the environment and stops with a message
naming any that is missing. Install the Python packages from
`benchmarks/requirements.txt` first.

| Variable | Meaning |
| --- | --- |
| `STANNUM_MOCK` | Directory holding the saved database in `db/`; runs are written to `runs/` |
| `STANNUM_DRIVER` | Prepared benchmark driver (`python3 benchmarks/tin.py --driver DIR prepare`) |
| `STANNUM_DATASET` | The published StackExchange dataset directory |
| `STANNUM_SOURCE` | Optional `source.json` pinning the image's provenance, so the working tree may move on during a run |
| `STANNUM_DOCKER_RUN_ARGS` | Optional; replaces the NVMe read caps |
| `PGPASSWORD` | Password for the containers' `postgres` user; required by `mock-container.sh`, otherwise random per run |

## Scripts

- `mock-build.sh [PROFILE] IMAGE` builds the database of that profile
  (default the 15M prefix) once and saves it to `$STANNUM_MOCK/db`. A saved
  database is only valid for the segment format its image wrote. It waits up
  to `STANNUM_IMAGE_WAIT_SECONDS` (3600) for the image. `STANNUM_ENGINE=paradedb`
  builds ParadeDB 0.26.0's database instead.
- `workload.sh PROFILE IMAGE LABEL STYLE UPDATES [SECONDS]` runs a workload from
  that copy and prints one line: QPS, latency, index MiB per query (TIN's
  MB/query), disk read per query and CPU. `run150m.sh IMAGE LABEL STYLE UPDATES
  [SECONDS]` is shorthand for the `150m` profile. `report.py` prints the line and
  can be rerun on an existing run directory. `STANNUM_ENGINE`,
  `STANNUM_SCORE_FUNCTION` and `STANNUM_CPU_LAYOUT` select ParadeDB, full
  scoring and the CPU reading.

Profiles:

| Profile | Rows | Server |
| --- | ---: | --- |
| `150m` | 150M | benchmarks v2, TIN v1.0.6's setup: 64g, 24GB shared buffers, 8 pinned CPUs (`tin.py --profile v2`) |
| `150m-legacy` | 150M | AWS r5 to r8: 64g build, 32g queries, 8 CPUs by quota (`--profile legacy`) |
| `mock15m` | 15M | 5g (12g build), 2GB shared buffers, 8 CPUs by quota |
| `smoke1m` | 1M | v2's pinning and settings, memory scaled down (2g, 512MB shared buffers) |

The full v2 campaign (every scenario, Stannum's two scorings and ParadeDB,
then the comparison with the published numbers) is `benchmarks/v2.py
campaign`; see "Matching TIN v1.0.6's published setup" in
[docs/benchmarks.md](../../docs/benchmarks.md) for its commands.
- `mock-probe.py IMAGE LABEL [N]` reports pages touched, candidates scored, disk
  read and time per query, with the cache dropped before each.
- `attrib.py IMAGE [WARM] [STEADY]` reconciles read counters per query style.
- `mock-container.sh start NAME IMAGE PORT` keeps a server up on the saved
  database for probing by hand; `mock-container.sh stop NAME` removes it.

Pages touched and candidates scored transfer to the full corpus directly; QPS is
relative.

## A fresh 150M database in TIN's shape (`TNS1`)

A database saved by an image of an earlier segment format does not load into
a `TNS1` build (page layout version 6); build one afresh. The build takes
hours (`STN3`'s took five at 150M rows on AWS) and its container gets 64 GB,
so raise the OrbStack VM first (that restarts the VM and bounces its other
containers) and lower it again before measuring. These commands are the
legacy setup's (TNS1 was measured that way); for v2 the queries get 64 GB
too, so keep the VM at its 100 GiB and use the `150m` profile.

```sh
# The branch's commit in a checkout of its own, so the working tree may move
# on meanwhile (build-image records the checkout's files and commit, so it
# needs a git checkout, not an archive).
SHA=$(git rev-parse --short tinshape/phase-c)
SRC=/Users/uri/stannum-lab/local150m-tns1/src-$SHA
git worktree add --detach "$SRC" "$SHA"

# The benchmark driver (k6 with the PostgreSQL extension), once.
python3 "$SRC/benchmarks/tin.py" --driver /Users/uri/stannum-lab/driver prepare
python3 "$SRC/benchmarks/tin.py" --driver /Users/uri/stannum-lab/driver build

# The image, native to the host (ARM): a few minutes.
IMAGE=stannum-bench:tns1-$SHA
(cd "$SRC" && python3 benchmarks/tin.py build-image --image "$IMAGE" \
    --base postgres:18-trixie --output /Users/uri/stannum-lab/local150m-tns1/image-$SHA)

export STANNUM_MOCK=/Users/uri/stannum-lab/local150m-tns1
export STANNUM_DRIVER=/Users/uri/stannum-lab/driver
export STANNUM_DATASET="$HOME/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange"
export STANNUM_SOURCE=$STANNUM_MOCK/image-$SHA/source.json

# Build and save: import, CREATE INDEX, VACUUM ANALYZE and validation, then
# the data volume copied to $STANNUM_MOCK/db.
orb config set memory_mib 81920 && orb stop && orb start
(cd "$SRC" && bash benchmarks/local/mock-build.sh 150m-legacy "$IMAGE")

# Measure from the saved database, the VM back near the instance's size.
orb config set memory_mib 40960 && orb stop && orb start
(cd "$SRC" && bash benchmarks/local/workload.sh 150m-legacy "$IMAGE" tns1 mixed 0 600)
(cd "$SRC" && bash benchmarks/local/workload.sh 150m-legacy "$IMAGE" tns1 conjunction-phrase 0 600)
(cd "$SRC" && bash benchmarks/local/workload.sh 150m-legacy "$IMAGE" tns1 disjunction 100 600)
```

Each run prints its line and keeps its results under
`$STANNUM_MOCK/runs/tns1-<style>-u<updates>/`; the build's log and report are
`$STANNUM_MOCK/build.log` and `$STANNUM_MOCK/build/`. The third run is the
published disjunction-updates workload: 100 updates a second beside the
queries.
