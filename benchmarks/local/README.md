# Local rehearsal of the published workloads when the index does not fit in memory

The AWS campaign at 150,000,000 rows runs a 246 GB index against 24 GB of shared
buffers in a 32 GB container on NVMe. These scripts rehearse the same regime on a
laptop with the 15,000,000-row prefix: a 21 GB index against 2 GB of shared buffers
in a 5 GB container, ten segments, eight clients on eight CPUs.

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

- `mock-build.sh IMAGE` builds the 15M database once and saves it to
  `$STANNUM_MOCK/db`. A saved database is only valid for the segment format its
  image wrote. It waits up to `STANNUM_IMAGE_WAIT_SECONDS` (3600) for the image.
- `workload.sh PROFILE IMAGE LABEL STYLE UPDATES [SECONDS]` runs a workload from
  that copy and prints one line: QPS, latency, disk read per query and CPU.
  `PROFILE` is `mock15m` or `150m`; the latter sizes the container like the
  instance for a full-corpus database. `run150m.sh IMAGE LABEL STYLE UPDATES
  [SECONDS]` is shorthand for the `150m` profile. `report.py` prints the line and
  can be rerun on an existing run directory.
- `mock-probe.py IMAGE LABEL [N]` reports pages touched, candidates scored, disk
  read and time per query, with the cache dropped before each.
- `attrib.py IMAGE [WARM] [STEADY]` reconciles read counters per query style.
- `mock-container.sh start NAME IMAGE PORT` keeps a server up on the saved
  database for probing by hand; `mock-container.sh stop NAME` removes it.

Pages touched and candidates scored transfer to the full corpus directly; QPS is
relative.
