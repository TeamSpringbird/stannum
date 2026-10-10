#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Benchmarks v2 on the stack's instance (i7i.8xlarge or i8g.8xlarge), from the
# repository checkout at /opt/stannum-benchmark/repo:
#
#   bash benchmarks/aws/v2-campaign.sh
#
# TIN v1.0.6's published setup (docs/benchmarks.md, "Matching TIN v1.0.6's
# published setup"): Stannum (score and full_score) and ParadeDB 0.26.0 on the
# 150M Stack Exchange corpus, the four scenarios, each reading of "8 vCPUs" the
# host's topology distinguishes, then the comparison report.
#
# Environment (all optional):
#   DRY_RUN=1            print every command instead of running it
#   STANNUM_TARGET_CPUS  extra x86-64 image builds measured after the runtime-dispatch
#                        baseline, e.g. "x86-64-v3 x86-64-v4" (x86 hosts only)
#   STANNUM_SNAPSHOT     s3://bucket/key.tar of a saved v2 Stannum database (else built here)
#   PARADEDB_SNAPSHOT    s3://bucket/key.tar of a saved ParadeDB 0.26.0 database (else built here)
#   V2_SECONDS           seconds measured per run (600)
#   V2_LAYOUTS           CPU readings ("siblings distinct-cores")
#   V2_SCENARIOS         ("conjunction disjunction phrase mixed")
set -euo pipefail
BENCH_ROOT=${BENCH_ROOT:-/opt/stannum-benchmark}
REPO=${REPO:-$BENCH_ROOT/repo}
DATASET=$BENCH_ROOT/datasets/stackexchange
DRIVER=$REPO/benchmarks/results/aws-driver
RESULTS=$REPO/benchmarks/results
EVIDENCE=$BENCH_ROOT/evidence
SECONDS_PER_RUN=${V2_SECONDS:-600}
read -r -a LAYOUTS <<< "${V2_LAYOUTS:-siblings distinct-cores}"
read -r -a SCENARIOS <<< "${V2_SCENARIOS:-conjunction disjunction phrase mixed}"
read -r -a TARGET_CPUS <<< "${STANNUM_TARGET_CPUS:-}"
ARCH=$(uname -m)
if [ "$ARCH" != x86_64 ] && [ ${#TARGET_CPUS[@]} -gt 0 ]; then
  echo "STANNUM_TARGET_CPUS is for x86-64 hosts; this host is $ARCH" >&2; exit 2
fi
run() {
  if [ -n "${DRY_RUN:-}" ]; then printf '%q ' "$@"; echo; else "$@"; fi
}
# Rerunnable: what an earlier invocation finished (driver, images, databases, completed
# runs) is kept, so a second pass with STANNUM_TARGET_CPUS adds only the new builds.
have() { [ -z "${DRY_RUN:-}" ] && [ -e "$1" ]; }
if [ -n "${DRY_RUN:-}" ]; then echo "cd $REPO"; else cd "$REPO"; fi
run mkdir -p "$EVIDENCE"
# The topology every pinned run reads, kept beside the results.
run sh -c "lscpu -e=CPU,CORE,SOCKET,NODE,ONLINE > $EVIDENCE/lscpu-e.txt; lscpu > $EVIDENCE/lscpu.txt; docker info > $EVIDENCE/docker.txt; uname -a > $EVIDENCE/kernel.txt"
run python3 benchmarks/published_dataset.py --corpus stackexchange --output "$DATASET"
if ! have "$DRIVER/k6"; then
  run python3 benchmarks/tin.py --driver "$DRIVER" prepare
  run python3 benchmarks/tin.py --driver "$DRIVER" build
fi

# The runtime-dispatch baseline first, then any CPU-level builds (x86 only).
IMAGES=(baseline ${TARGET_CPUS[@]+"${TARGET_CPUS[@]}"})
for variant in "${IMAGES[@]}"; do
  args=()
  [ "$variant" = baseline ] || args=(--target-cpu "$variant")
  have "$RESULTS/aws-image-$variant/image.json" || run python3 benchmarks/tin.py build-image \
    --image "stannum-bench:v2-$variant" --output "$RESULTS/aws-image-$variant" ${args[@]+"${args[@]}"}
  run sh -c "docker run --rm --entrypoint postgres stannum-bench:v2-$variant --version | tee $EVIDENCE/postgres-version-$variant.txt | grep -q 'PostgreSQL) 18.6'"
done

# The saved databases every run starts from: restored, or built once here with v2 sizing.
restore() {  # restore S3_URL DIR
  run mkdir -p "$2"
  run sh -c "python3 -c \"import boto3,sys;from boto3.s3.transfer import TransferConfig as T;b,k='$1'[5:].split('/',1);boto3.client('s3').download_fileobj(b,k,sys.stdout.buffer,Config=T(multipart_chunksize=128*1024*1024,max_concurrency=8))\" | tar -C '$2' -xf -"
}
build_database() {  # build_database ENGINE DIR
  run python3 benchmarks/tin.py --driver "$DRIVER" run --source-manifest "$RESULTS/aws-image-baseline/source.json" \
    --published-corpus stackexchange --dataset "$DATASET" --rows 150000000 --validation-rows 1000 \
    --validation-queries 10 --ranked-validation-queries 2 --engines "$1" --workload topk --style mixed \
    --profile v2 --cpu-layout "${LAYOUTS[0]}" --warmup 10 --seconds 120 --setup-timeout-seconds 43200 \
    --save-database "$2" --image stannum-bench:v2-baseline --output "$RESULTS/aws-v2-build-$1"
}
STANNUM_DB=$BENCH_ROOT/db-v2-stannum
PARADEDB_DB=$BENCH_ROOT/db-v2-paradedb
if ! have "$STANNUM_DB/snapshot.json"; then
  if [ -n "${STANNUM_SNAPSHOT:-}" ]; then restore "$STANNUM_SNAPSHOT" "$STANNUM_DB"; else build_database stannum "$STANNUM_DB"; fi
fi
if ! have "$PARADEDB_DB/snapshot.json"; then
  if [ -n "${PARADEDB_SNAPSHOT:-}" ]; then restore "$PARADEDB_SNAPSHOT" "$PARADEDB_DB"; else build_database paradedb "$PARADEDB_DB"; fi
fi

for variant in "${IMAGES[@]}"; do
  engines=(stannum paradedb)
  # ParadeDB is the calibration anchor; once per campaign is enough.
  [ "$variant" = baseline ] || engines=(stannum)
  run python3 benchmarks/v2.py campaign --driver "$DRIVER" --dataset "$DATASET" \
    --image "stannum-bench:v2-$variant" --source-manifest "$RESULTS/aws-image-$variant/source.json" \
    --stannum-database "$STANNUM_DB" --paradedb-database "$PARADEDB_DB" \
    --output "$RESULTS/aws-v2-$variant" --layouts "${LAYOUTS[@]}" --scenarios "${SCENARIOS[@]}" \
    --engines "${engines[@]}" --seconds "$SECONDS_PER_RUN" || echo "campaign $variant had failed runs"
done
for variant in "${IMAGES[@]}"; do
  run python3 benchmarks/v2.py report "$RESULTS/aws-v2-$variant" --anchor "$RESULTS/aws-v2-baseline"
done
