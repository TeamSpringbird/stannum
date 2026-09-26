#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A persistent server on the saved mock database, for probing by hand:
#
#   mock-container.sh start NAME IMAGE PORT [extra postgres -c args...]
#   mock-container.sh stop NAME
#
# start needs STANNUM_MOCK (the directory holding db/) and PGPASSWORD, which
# becomes the container's postgres password; connect with
# PGHOST=127.0.0.1 PGPORT=PORT PGUSER=postgres PGDATABASE=benchmark.
# STANNUM_CONTAINER_MEMORY (5g) and STANNUM_SHARED_BUFFERS (2GB) size the server;
# STANNUM_DOCKER_RUN_ARGS overrides the NVMe read caps, as in workload.sh.
set -euo pipefail
usage() {
  echo "usage: $0 start NAME IMAGE PORT [postgres args...] | $0 stop NAME" >&2
  exit 2
}
[ $# -ge 2 ] || usage
cmd=$1; NAME=$2
remove() { docker rm -f "$NAME" >/dev/null 2>&1 || true; docker volume rm "$NAME-data" >/dev/null 2>&1 || true; }
case $cmd in
  stop) remove; exit 0 ;;
  start) [ $# -ge 4 ] || usage ;;
  *) usage ;;
esac
IMG=$3; PORT=$4; shift 4
: "${STANNUM_MOCK:?set STANNUM_MOCK to the directory holding the saved database (db/)}"
: "${PGPASSWORD:?set PGPASSWORD to the password the container should give the postgres user}"
DB=$STANNUM_MOCK/db
[ -d "$DB" ] || { echo "no saved database at $DB" >&2; exit 1; }
read -r -a CAPS <<< "${STANNUM_DOCKER_RUN_ARGS:---device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb}"
MEM=${STANNUM_CONTAINER_MEMORY:-5g}; SB=${STANNUM_SHARED_BUFFERS:-2GB}
remove
docker volume create "$NAME-data" >/dev/null
docker run --rm -v "$DB:/from:ro" -v "$NAME-data:/to" alpine sh -c 'cp -a /from/. /to/ && rm -f /to/*/docker/postmaster.pid'
# The password reaches docker through the environment, never the command line.
POSTGRES_PASSWORD=$PGPASSWORD docker run -d --name "$NAME" --cpus 8 "${CAPS[@]}" --memory "$MEM" --memory-swap "$MEM" --shm-size 1g \
  -p "127.0.0.1:$PORT:5432" -v "$NAME-data:/var/lib/postgresql" -e POSTGRES_PASSWORD -e POSTGRES_DB=benchmark \
  "$IMG" postgres -c shared_buffers="$SB" -c work_mem=16MB -c max_parallel_workers=8 -c jit=off -c track_io_timing=on -c autovacuum=off "$@" >/dev/null
for _ in $(seq 1 90); do
  if docker exec "$NAME" pg_isready -U postgres >/dev/null 2>&1; then echo ready; exit 0; fi
  sleep 2
done
echo timeout >&2
exit 1
