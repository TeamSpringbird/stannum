#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Build a database once under AWS-like proportions and save it for workload.sh
# runs of the same profile:
#
#   mock-build.sh [mock15m|150m|150m-legacy|smoke1m] IMAGE
#
# The profile (default mock15m) is workload.sh's. 150m is benchmarks v2, TIN
# v1.0.6's setup: the build and the queries get 64g, 8 pinned CPUs and 8
# parallel maintenance workers, so raise the Docker VM's memory first;
# 150m-legacy is AWS r5-r8's (a 64g build, 32g for queries, unpinned).
#
# IMAGE (or STANNUM_IMAGE) is the benchmark image; a saved database is only
# valid for the segment format that image writes. If the image is still being
# built elsewhere, the script waits up to STANNUM_IMAGE_WAIT_SECONDS (default
# 3600) for it to appear. Required environment: STANNUM_MOCK (output directory;
# db/ and build/ inside it are replaced), STANNUM_DRIVER, STANNUM_DATASET.
# STANNUM_SOURCE optionally pins the image's source.json, so the working tree
# may move on while the build runs. STANNUM_ENGINE=paradedb builds ParadeDB
# 0.26.0's database instead (the benchmarker's bm25 index; IMAGE is still the
# Stannum image the harness checks, the server runs tin.py's pinned ParadeDB image).
set -euo pipefail
PROFILE=mock15m
case ${1:-} in
  mock15m|150m|150m-legacy|smoke1m) PROFILE=$1; shift ;;
esac
IMG=${1:-${STANNUM_IMAGE:-}}
if [ -z "$IMG" ] || [ $# -gt 1 ]; then
  echo "usage: $0 [mock15m|150m|150m-legacy|smoke1m] IMAGE (or set STANNUM_IMAGE); needs STANNUM_MOCK, STANNUM_DRIVER, STANNUM_DATASET" >&2
  exit 2
fi
case $PROFILE in
  mock15m)     ROWS=15000000; CHECKS=(--validation-queries 3 --ranked-validation-queries 1)
               SIZES=(--clients 8 --cpus 8 --memory 5g --build-memory 12g --shared-buffers 2GB --maintenance-work-mem 4GB)
               TIMEOUT=14400 ;;
  150m)        ROWS=150000000; CHECKS=(--validation-queries 10 --ranked-validation-queries 2)
               SIZES=(--profile v2); TIMEOUT=43200 ;;
  150m-legacy) ROWS=150000000; CHECKS=(--validation-queries 10 --ranked-validation-queries 2)
               SIZES=(--profile legacy); TIMEOUT=43200 ;;
  smoke1m)     ROWS=1000000; CHECKS=(--validation-queries 6 --ranked-validation-queries 2)
               SIZES=(--profile v2 --memory 2g --build-memory 4g --shared-buffers 512MB --maintenance-work-mem 1GB --shm-size 1g)
               TIMEOUT=3600 ;;
esac
ENGINE=${STANNUM_ENGINE:-stannum}
case $ENGINE in stannum|paradedb) ;; *) echo "STANNUM_ENGINE must be stannum or paradedb" >&2; exit 2 ;; esac
: "${STANNUM_MOCK:?set STANNUM_MOCK to the directory that will hold the saved database}"
: "${STANNUM_DRIVER:?set STANNUM_DRIVER to the prepared benchmark driver directory}"
: "${STANNUM_DATASET:?set STANNUM_DATASET to the published StackExchange dataset directory}"
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

deadline=$((SECONDS + ${STANNUM_IMAGE_WAIT_SECONDS:-3600}))
until docker images --format '{{.Repository}}:{{.Tag}}' | grep -qx "$IMG"; do
  if [ $SECONDS -ge $deadline ]; then echo "image $IMG did not appear; giving up" >&2; exit 1; fi
  sleep 20
done
R=$STANNUM_MOCK
mkdir -p "$R"; rm -rf "$R/db" "$R/build"
echo "== build $PROFILE $ENGINE $(date)"
status=0
python3 benchmarks/tin.py --driver "$STANNUM_DRIVER" run ${STANNUM_SOURCE:+--source-manifest "$STANNUM_SOURCE"} \
  --published-corpus stackexchange --dataset "$STANNUM_DATASET" --rows "$ROWS" \
  --validation-rows 1000 "${CHECKS[@]}" --engines "$ENGINE" --workload topk --style mixed \
  --warmup 10 --seconds 120 "${SIZES[@]}" \
  --setup-timeout-seconds "$TIMEOUT" --save-database "$R/db" --image "$IMG" --output "$R/build" > "$R/build.log" 2>&1 || status=$?
echo "exit $status $(date)"
python3 benchmarks/local/report.py build "$R/build" 120
echo "MOCK-BUILD-DONE $(date)"
exit $status
