#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Build the 15M mock database once under AWS-like proportions and save it for
# workload.sh mock15m runs:
#
#   mock-build.sh [IMAGE]
#
# IMAGE (or STANNUM_IMAGE) is the benchmark image; a saved database is only
# valid for the segment format that image writes. If the image is still being
# built elsewhere, the script waits up to STANNUM_IMAGE_WAIT_SECONDS (default
# 3600) for it to appear. Required environment: STANNUM_MOCK (output directory;
# db/ and build/ inside it are replaced), STANNUM_DRIVER, STANNUM_DATASET.
# STANNUM_SOURCE optionally pins the image's source.json, so the working tree
# may move on while the build runs.
set -euo pipefail
IMG=${1:-${STANNUM_IMAGE:-}}
if [ -z "$IMG" ] || [ $# -gt 1 ]; then
  echo "usage: $0 IMAGE (or set STANNUM_IMAGE); needs STANNUM_MOCK, STANNUM_DRIVER, STANNUM_DATASET" >&2
  exit 2
fi
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
echo "== build $(date)"
status=0
python3 benchmarks/tin.py --driver "$STANNUM_DRIVER" run ${STANNUM_SOURCE:+--source-manifest "$STANNUM_SOURCE"} \
  --published-corpus stackexchange --dataset "$STANNUM_DATASET" --rows 15000000 \
  --validation-rows 1000 --validation-queries 3 --ranked-validation-queries 1 --engines stannum --workload topk --style mixed \
  --warmup 10 --seconds 120 --clients 8 --cpus 8 --memory 5g --build-memory 12g --shared-buffers 2GB --maintenance-work-mem 4GB \
  --setup-timeout-seconds 14400 --save-database "$R/db" --image "$IMG" --output "$R/build" > "$R/build.log" 2>&1 || status=$?
echo "exit $status $(date)"
python3 benchmarks/local/report.py build "$R/build" 120
echo "MOCK-BUILD-DONE $(date)"
exit $status
