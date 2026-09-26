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
set -euo pipefail
COMMIT=${1:-${STANNUM_COMMIT:-}}
if ! [[ $COMMIT =~ ^[0-9a-f]{40}$ ]]; then
  echo "usage: $0 COMMIT (a full 40-character commit SHA, or set STANNUM_COMMIT)" >&2
  exit 2
fi
REPOSITORY=${STANNUM_REPOSITORY:-https://github.com/TeamSpringbird/stannum.git}
until test -f /opt/stannum-benchmark/bootstrap-ready; do sleep 5; done
cd /opt/stannum-benchmark
git clone "$REPOSITORY" source
cd source
git checkout --detach "$COMMIT"
mkdir -p /opt/stannum-benchmark/build
python3 - <<'BUILD'
import json, sys, subprocess
from pathlib import Path
sys.path.insert(0,'benchmarks')
import run
out=Path('/opt/stannum-benchmark/build')
p=run.provenance(out)
(out/'source.json').write_text(json.dumps(p,indent=2))
recipe=run.digest(Path('benchmarks/Dockerfile').read_bytes()+Path('benchmarks/Dockerfile.dockerignore').read_bytes())
subprocess.run(['docker','build','--platform','linux/arm64','-f','benchmarks/Dockerfile','--build-arg','STANNUM_COMMIT='+p['commit'],'--build-arg','STANNUM_SOURCE_SHA256='+p['source_sha256'],'--build-arg','RECIPE_SHA256='+recipe,'-t','stannum-bench:aws','.'],check=True)
(out/'image.json').write_text(subprocess.check_output(['docker','image','inspect','stannum-bench:aws'],text=True))
BUILD
