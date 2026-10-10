#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Benchmarks v2 on the stack's instance (i7i.8xlarge or i8g.8xlarge) in one
# session, from the repository checkout at /opt/stannum-benchmark/repo:
#
#   bash benchmarks/aws/v2-campaign.sh
#
# TIN v1.0.6's published setup (docs/benchmarks.md, "Matching TIN v1.0.6's
# published setup"): Stannum with score and full_score on the 150M Stack
# Exchange corpus, the four scenarios, each reading of "8 vCPUs" the host's
# topology distinguishes, then the report beside the published numbers. No
# ParadeDB: its numbers are PlanetScale's published ones. The session exists to
# confirm the local results, so it is built to be short: one database, every
# run back to back, evidence uploaded after each campaign.
#
# Environment (all optional):
#   DRY_RUN=1            print every command instead of running it
#   STANNUM_SNAPSHOT     s3://bucket/key.tar of a v2 Stannum database saved by a
#                        local build (mock-build.sh 150m); restored instead of
#                        building here, and no corpus download (recommended: see
#                        the cost table in docs/benchmarks.md)
#   STANNUM_TARGET_CPUS  extra x86-64 builds measured after the runtime-dispatch
#                        baseline, e.g. "x86-64-v3 x86-64-v4" (x86 hosts only)
#   BENCH_BUCKET         the stack's ArtifactBucket output: evidence goes there
#   V2_SECONDS           seconds measured per run (600)
#   V2_LAYOUTS           CPU readings for the baseline ("siblings distinct-cores")
#   V2_VARIANT_LAYOUTS   CPU readings for the target-CPU builds ("siblings")
#   V2_VARIANT_SCORES    scorings for the target-CPU builds ("score")
#   V2_SCENARIOS         ("conjunction disjunction phrase mixed")
set -euo pipefail
BENCH_ROOT=${BENCH_ROOT:-/opt/stannum-benchmark}
REPO=${REPO:-$BENCH_ROOT/repo}
DATASET=$BENCH_ROOT/datasets/stackexchange
DRIVER=$REPO/benchmarks/results/aws-driver
RESULTS=$REPO/benchmarks/results
EVIDENCE=$BENCH_ROOT/evidence
DB=$BENCH_ROOT/db-v2-stannum
SECONDS_PER_RUN=${V2_SECONDS:-600}
read -r -a LAYOUTS <<< "${V2_LAYOUTS:-siblings distinct-cores}"
read -r -a VARIANT_LAYOUTS <<< "${V2_VARIANT_LAYOUTS:-siblings}"
read -r -a VARIANT_SCORES <<< "${V2_VARIANT_SCORES:-score}"
read -r -a SCENARIOS <<< "${V2_SCENARIOS:-conjunction disjunction phrase mixed}"
read -r -a TARGET_CPUS <<< "${STANNUM_TARGET_CPUS:-}"
ARCH=$(uname -m)
if [ "$ARCH" != x86_64 ] && [ ${#TARGET_CPUS[@]} -gt 0 ]; then
  echo "STANNUM_TARGET_CPUS is for x86-64 hosts; this host is $ARCH" >&2; exit 2
fi
run() {
  if [ -n "${DRY_RUN:-}" ]; then printf '%q ' "$@"; echo; else "$@"; fi
}
# Rerunnable: what an earlier invocation finished (driver, images, database, completed
# runs) is kept, so a second pass only adds what is missing.
have() { [ -z "${DRY_RUN:-}" ] && [ -e "$1" ]; }
log() { echo "== $* $(date -u +%H:%M:%SZ)"; }
if [ -n "${DRY_RUN:-}" ]; then echo "cd $REPO"; else cd "$REPO"; fi
run mkdir -p "$EVIDENCE"
# The topology every pinned run reads, kept beside the results.
run sh -c "lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE > $EVIDENCE/lscpu-e.txt; lscpu > $EVIDENCE/lscpu.txt; docker info > $EVIDENCE/docker.txt; uname -a > $EVIDENCE/kernel.txt"

export_evidence() {  # export_evidence NAME: results and logs, never databases or CSVs
  [ -n "${BENCH_BUCKET:-}" ] || return 0
  run sh -c "tar -czf $BENCH_ROOT/$1.tar.gz --exclude='*/input.csv' -C $REPO/benchmarks results -C $BENCH_ROOT evidence && python3 -c \"import boto3; boto3.client('s3').upload_file('$BENCH_ROOT/$1.tar.gz', '$BENCH_BUCKET', '$1.tar.gz')\""
}

log driver
if ! have "$DRIVER/k6"; then
  run python3 benchmarks/tin.py --driver "$DRIVER" prepare
  run python3 benchmarks/tin.py --driver "$DRIVER" build
fi

build_image() {  # build_image VARIANT
  local args=()
  [ "$1" = baseline ] || args=(--target-cpu "$1")
  have "$RESULTS/aws-image-$1/image.json" || run python3 benchmarks/tin.py build-image \
    --image "stannum-bench:v2-$1" --output "$RESULTS/aws-image-$1" ${args[@]+"${args[@]}"}
  run sh -c "docker run --rm --entrypoint postgres stannum-bench:v2-$1 --version | tee $EVIDENCE/postgres-version-$1.txt | grep -q 'PostgreSQL) 18.6'"
}
log image baseline
build_image baseline
# The CPU-level images build while the database is restored or built.
VARIANTS_PID=
if [ ${#TARGET_CPUS[@]} -gt 0 ]; then
  if [ -n "${DRY_RUN:-}" ]; then
    for cpu in "${TARGET_CPUS[@]}"; do build_image "$cpu"; done
  else
    ( for cpu in "${TARGET_CPUS[@]}"; do build_image "$cpu"; done ) > "$EVIDENCE/variant-images.log" 2>&1 &
    VARIANTS_PID=$!
  fi
fi

if ! have "$DB/snapshot.json"; then
  run mkdir -p "$DB"
  if [ -n "${STANNUM_SNAPSHOT:-}" ]; then
    log restore "$STANNUM_SNAPSHOT"
    run sh -c "python3 -c \"import boto3,sys;from boto3.s3.transfer import TransferConfig as T;b,k='$STANNUM_SNAPSHOT'[5:].split('/',1);boto3.client('s3').download_fileobj(b,k,sys.stdout.buffer,Config=T(multipart_chunksize=128*1024*1024,max_concurrency=16))\" | tar -C '$DB' -xf -"
  else
    log corpus
    run python3 benchmarks/published_dataset.py --corpus stackexchange --output "$DATASET"
    log build
    run python3 benchmarks/tin.py --driver "$DRIVER" run --source-manifest "$RESULTS/aws-image-baseline/source.json" \
      --published-corpus stackexchange --dataset "$DATASET" --rows 150000000 --validation-rows 1000 \
      --validation-queries 10 --ranked-validation-queries 2 --engines stannum --workload topk --style mixed \
      --profile v2 --cpu-layout "${LAYOUTS[0]}" --warmup 10 --seconds 120 --setup-timeout-seconds 43200 \
      --save-database "$DB" --image stannum-bench:v2-baseline --output "$RESULTS/aws-v2-build"
    # The CSV prefix the build imported (79 GiB) is not evidence.
    run rm -f "$RESULTS/aws-v2-build/stannum/input.csv"
  fi
fi
run cp "$DB/snapshot.json" "$EVIDENCE/database-snapshot.json"

campaign() {  # campaign VARIANT LAYOUT... -- SCORE...
  local variant=$1; shift
  local layouts=()
  while [ "$1" != -- ]; do layouts+=("$1"); shift; done; shift
  log campaign "$variant"
  run python3 benchmarks/v2.py campaign --driver "$DRIVER" --image "stannum-bench:v2-$variant" \
    --source-manifest "$RESULTS/aws-image-$variant/source.json" --database "$DB" \
    --output "$RESULTS/aws-v2-$variant" --layouts "${layouts[@]}" --scenarios "${SCENARIOS[@]}" \
    --score-functions "$@" --seconds "$SECONDS_PER_RUN" || echo "campaign $variant had failed runs"
  export_evidence "v2-$variant"
}
campaign baseline "${LAYOUTS[@]}" -- score full_score
if [ -n "$VARIANTS_PID" ]; then wait "$VARIANTS_PID"; fi
for cpu in ${TARGET_CPUS[@]+"${TARGET_CPUS[@]}"}; do
  campaign "$cpu" "${VARIANT_LAYOUTS[@]}" -- "${VARIANT_SCORES[@]}"
done
log "campaign finished"
