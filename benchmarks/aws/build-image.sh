#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Build the benchmark image on the stack's instance from a pinned commit:
#
#   build-image.sh COMMIT        (or STANNUM_COMMIT=COMMIT build-image.sh)
#
# COMMIT must be a full 40-character SHA so the recorded provenance names
# exactly one tree. The first AWS comparison pinned
# ab1e6db87e7c5bafbfc5c121ac66d879d48bbd3b. STANNUM_REPOSITORY overrides the
# clone URL.
#
# The image is built for the host's own architecture (amd64 on i7i, arm64 on
# i8g): run() refuses emulated images. On x86-64, STANNUM_TARGET_CPU
# (x86-64-v3 or x86-64-v4) builds for that CPU level; unset, the default, builds
# the baseline whose SIMD kernels pick AVX2 or AVX-512 at run time. It is a
# Dockerfile build argument, never RUSTFLAGS (docs/architecture/tin-shape.md).
# On arm64, STANNUM_BASE (default postgres:18-trixie) replaces the pinned
# ParadeDB base, which is published for amd64 only.
set -euo pipefail
COMMIT=${1:-${STANNUM_COMMIT:-}}
if ! [[ $COMMIT =~ ^[0-9a-f]{40}$ ]]; then
  echo "usage: $0 COMMIT (a full 40-character commit SHA, or set STANNUM_COMMIT)" >&2
  exit 2
fi
case $(uname -m) in
  x86_64) PLATFORM=linux/amd64; BASE=${STANNUM_BASE:-} ;;
  aarch64|arm64)
    PLATFORM=linux/arm64; BASE=${STANNUM_BASE:-postgres:18-trixie}
    if [ -n "${STANNUM_TARGET_CPU:-}" ]; then
      echo "STANNUM_TARGET_CPU is an x86-64 build argument; this host is $(uname -m)" >&2; exit 2
    fi ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac
export PLATFORM BASE STANNUM_TARGET_CPU=${STANNUM_TARGET_CPU:-}
REPOSITORY=${STANNUM_REPOSITORY:-https://github.com/TeamSpringbird/stannum.git}
until test -f /opt/stannum-benchmark/bootstrap-ready; do sleep 5; done
cd /opt/stannum-benchmark
git clone "$REPOSITORY" source
cd source
git checkout --detach "$COMMIT"
mkdir -p /opt/stannum-benchmark/build
python3 - <<'BUILD'
import json, os, sys, subprocess
from pathlib import Path
sys.path.insert(0,'benchmarks')
import run
out=Path('/opt/stannum-benchmark/build')
p=run.provenance(out)
(out/'source.json').write_text(json.dumps(p,indent=2))
recipe=run.digest(Path('benchmarks/Dockerfile').read_bytes()+Path('benchmarks/Dockerfile.dockerignore').read_bytes())
extra=[]
if os.environ['BASE']:
    extra+=['--build-arg','BASE='+os.environ['BASE']]
if os.environ['STANNUM_TARGET_CPU']:
    extra+=['--build-arg','STANNUM_TARGET_CPU='+os.environ['STANNUM_TARGET_CPU']]
subprocess.run(['docker','build','--platform',os.environ['PLATFORM'],'-f','benchmarks/Dockerfile','--build-arg','STANNUM_COMMIT='+p['commit'],'--build-arg','STANNUM_SOURCE_SHA256='+p['source_sha256'],'--build-arg','RECIPE_SHA256='+recipe,*extra,'-t','stannum-bench:aws','.'],check=True)
(out/'image.json').write_text(subprocess.check_output(['docker','image','inspect','stannum-bench:aws'],text=True))
BUILD
