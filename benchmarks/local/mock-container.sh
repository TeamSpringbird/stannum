#!/bin/bash
# A persistent server on the saved mock database, for probing by hand:
#   mock-container.sh start NAME IMAGE PORT [extra postgres -c args...]
#   mock-container.sh stop NAME
# STANNUM_MOCK picks the saved database directory.
set -u
cmd=$1; NAME=$2
DB=${STANNUM_MOCK:-/tmp/stannum-ordinal-poc/mock15m}/db
if [ "$cmd" = stop ]; then docker rm -f $NAME >/dev/null 2>&1; docker volume rm $NAME-data >/dev/null 2>&1; exit 0; fi
IMG=$3; PORT=$4; shift 4
docker rm -f $NAME >/dev/null 2>&1; docker volume rm $NAME-data >/dev/null 2>&1; docker volume create $NAME-data >/dev/null
docker run --rm -v $DB:/from:ro -v $NAME-data:/to alpine sh -c 'cp -a /from/. /to/ && rm -f /to/*/docker/postmaster.pid' || exit 1
docker run -d --name $NAME --cpus 8 --device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb --memory 5g --memory-swap 5g --shm-size 1g \
  -p 127.0.0.1:$PORT:5432 -v $NAME-data:/var/lib/postgresql -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=benchmark \
  $IMG postgres -c shared_buffers=2GB -c work_mem=16MB -c max_parallel_workers=8 -c jit=off -c track_io_timing=on -c autovacuum=off "$@" >/dev/null
for i in $(seq 1 90); do docker exec $NAME pg_isready -U postgres >/dev/null 2>&1 && { echo ready; exit 0; }; sleep 2; done; echo timeout; exit 1
