#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

set -eu
until test -f /opt/stannum-benchmark/bootstrap-ready; do sleep 5; done
cd /opt/stannum-benchmark
git clone https://github.com/TeamSpringbird/stannum.git source
cd source
git checkout --detach ab1e6db87e7c5bafbfc5c121ac66d879d48bbd3b
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
