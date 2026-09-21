# Reusable AWS database snapshots

The snapshot cache preserves the complete PostgreSQL cluster and its exact Docker
runtime image. It avoids rebuilding the image, importing the corpus, and building
the index when repeating a baseline. It does not permit swapping in a newer
Stannum binary without a separate index-format compatibility validation.

Capture runs before measurements: VACUUM ANALYZE, CHECKPOINT, fingerprint, clean
shutdown, full-volume archive, restart, image export. It rejects external
symlinks/tablespaces and shared volumes. `pg_controldata` must report `shut down`.
A separate container then restores into a new volume and checks PostgreSQL and
extension versions, row count, relation sizes, visibility maps, segment metadata,
a 1,000-row content/TID fingerprint, and a search count. Its temporary resources
are deleted. Only after this succeeds is the snapshot manifest published to S3.

This follows PostgreSQL's [file-system backup requirements](https://www.postgresql.org/docs/18/backup-file.html):
stop the server and retain the entire cluster, including WAL. A clean restart
clears PostgreSQL shared buffers; filesystem caches may remain warm. Run the same
warmup/measurement protocol for restored and newly built baselines. Snapshot
validation is not a replacement for the campaign's query correctness checks.

The private corpus cache bucket also stores `postgres-snapshots/<unique-id>/`.
It has a 30-day expiration rule and one-day incomplete multipart cleanup. The host
role can read/write only that snapshot prefix in addition to the existing corpus
read grant. Temporary EC2 teardown preserves this cache. Archives and manifests
contain a disposable benchmark cluster, not production credentials or data.

To capture during a fresh campaign, set a unique prefix before running
`benchmarks/aws/crossover.sh`:

```sh
export SNAPSHOT_CAPTURE_PREFIX=postgres-snapshots/wikipedia-<date>-<unique-id>
bash benchmarks/aws/crossover.sh
```

To restore a published baseline on a fresh host:

```sh
export SNAPSHOT_RESTORE_PREFIX=postgres-snapshots/wikipedia-<date>-<unique-id>
bash benchmarks/aws/crossover.sh
```

Choose one mode. Restore verifies archive sizes/SHA-256, corpus identity, and
archive paths, loads the archived image by its exact content ID, and refuses an
existing destination volume. The campaign records the original snapshot source
commit/image in `image/restored-snapshot.json`; the current checkout identifies
the harness, not a newly compiled extension. The restored database fingerprint
is checked before proceeding. Neither restore nor capture changes the query
strategy or enables the experimental selector.

`database_snapshot.py` also exposes `capture`, `publish`, `fetch`, `restore`, and
`check` commands for manual workflows. Capture records compressed byte counts,
capture time, and local restore/check time; publish and fetch record transfer
times. Restore/check time includes hashing, image loading, extraction, server
startup, and validation. It excludes S3 download time. These measurements decide
whether retaining snapshots is worthwhile.

The current r4 run was already launched at bba903d when snapshotting was requested.
A recorded gate was installed before the campaign Python process started, leaving
the original campaign file intact. The hook captures and publishes the baseline
before invoking that original campaign; its source hash and receipt are retained
with the evidence. Timed query code and the measured extension remain unchanged.
