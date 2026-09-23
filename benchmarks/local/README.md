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

`mock-build.sh` builds the database once and saves it; `mock-run.sh IMAGE LABEL
STYLE UPDATES [SECONDS]` runs a workload from that copy and reports QPS, latency,
disk read per query and CPU; `mock-probe.py IMAGE LABEL [N]` reports pages touched,
candidates scored, disk read and time per query with the cache dropped before each.
Pages touched and candidates scored transfer to the full corpus directly; QPS is
relative. Paths under `/tmp/stannum-ordinal-poc` are this machine's.
