#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Run a published workload from a saved database under AWS-like proportions:
#
#   workload.sh PROFILE IMAGE LABEL STYLE UPDATES [SECONDS]
#
# PROFILE is mock15m (the 15M-row prefix: 5g container, 2GB shared buffers) or
# 150m (the full corpus: 32g container, 24GB shared buffers). SECONDS defaults
# to 180. Required environment:
#   STANNUM_MOCK     directory holding the saved database in db/; runs go to runs/
#   STANNUM_DRIVER   prepared benchmark driver (tin.py --driver)
#   STANNUM_DATASET  the published StackExchange dataset directory
# Optional: STANNUM_SOURCE (a source.json to pin the image's provenance) and
# STANNUM_DOCKER_RUN_ARGS (defaults to the NVMe read caps below).
set -euo pipefail
usage() {
  echo "usage: $0 mock15m|150m IMAGE LABEL STYLE UPDATES [SECONDS]" >&2
  echo "  needs STANNUM_MOCK, STANNUM_DRIVER and STANNUM_DATASET; see the script's header" >&2
  exit 2
}
[ $# -ge 5 ] && [ $# -le 6 ] || usage
PROFILE=$1; IMG=$2; LABEL=$3; STYLE=$4; UPDATES=$5; SECONDS_=${6:-180}
case $PROFILE in
  mock15m) ROWS=15000000; CHECKS=(--validation-queries 2 --ranked-validation-queries 1)
           SIZES=(--memory 5g --build-memory 12g --shared-buffers 2GB --maintenance-work-mem 4GB) ;;
  150m)    ROWS=150000000; CHECKS=(--validation-queries 10 --ranked-validation-queries 2)
           SIZES=(--memory 32g --build-memory 64g --shared-buffers 24GB --maintenance-work-mem 24GB) ;;
  *) echo "unknown profile '$PROFILE' (mock15m or 150m)" >&2; usage ;;
esac
[[ $UPDATES =~ ^[0-9]+$ ]] || { echo "UPDATES must be a whole number, not '$UPDATES'" >&2; usage; }
: "${STANNUM_MOCK:?set STANNUM_MOCK to the directory holding the saved database (db/)}"
: "${STANNUM_DRIVER:?set STANNUM_DRIVER to the prepared benchmark driver directory}"
: "${STANNUM_DATASET:?set STANNUM_DATASET to the published StackExchange dataset directory}"
[ -d "$STANNUM_MOCK/db" ] || { echo "no saved database at $STANNUM_MOCK/db; run mock-build.sh first" >&2; exit 1; }
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

# The Docker VM disk is served from the host cache at RAM speed: cap reads at what the
# i7i.8xlarge NVMe delivered inside the container (13.8M reads and 241 GB in 688 s).
export STANNUM_DOCKER_RUN_ARGS="${STANNUM_DOCKER_RUN_ARGS:---device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb}"
R=$STANNUM_MOCK/runs/$LABEL-$STYLE-u$UPDATES
rm -rf "$R" "$R.log"; mkdir -p "$(dirname "$R")"

# Validation warms the VM's global page cache with pages that stay cached, uncharged, after
# the harness restarts the container: drop the cache the moment the measurement phase starts,
# so the run reads the index cold except for its own shared buffers, as on AWS.
(
  for _ in $(seq 1 3000); do
    if [ -f "$R/stannum/resources.jsonl" ] && tail -n 1 "$R/stannum/resources.jsonl" | grep -q driver-warmup-and-measurement; then
      docker run --rm --privileged alpine sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'
      echo "dropped caches at measurement start $(date)" >> "$R.log"
      exit 0
    fi
    sleep 1
  done
) &
WATCHER=$!
trap 'kill "$WATCHER" 2>/dev/null || true' EXIT

status=0
python3 benchmarks/tin.py --driver "$STANNUM_DRIVER" run ${STANNUM_SOURCE:+--source-manifest "$STANNUM_SOURCE"} \
  --published-corpus stackexchange --dataset "$STANNUM_DATASET" --rows "$ROWS" \
  --validation-rows 1000 "${CHECKS[@]}" --engines stannum --workload topk --style "$STYLE" --updates "$UPDATES" \
  --warmup 10 --seconds "$SECONDS_" --clients 8 --cpus 8 "${SIZES[@]}" \
  --setup-timeout-seconds 14400 --load-database "$STANNUM_MOCK/db" --image "$IMG" --output "$R" >> "$R.log" 2>&1 || status=$?
python3 benchmarks/local/report.py workload "$R" "$LABEL" "$STYLE" "$UPDATES" "$SECONDS_"
exit $status
