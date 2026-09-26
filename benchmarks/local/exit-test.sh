#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Backends killed mid-walk on Linux (postgres/tests/exit_during_walk.py): builds an image of the working tree,
# serves it as container stannum-exit on 127.0.0.1:28963, runs the test, removes the container.
#   exit-test.sh [test args...]      STANNUM_EXIT_IMAGE reuses a built image; PYTHON needs psycopg (default /tmp/stannum-venv)
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd); IMG=${STANNUM_EXIT_IMAGE:-}
if [ -z "$IMG" ]; then IMG=stannum-bench:exit-$(date +%s); python3 "$ROOT/benchmarks/tin.py" build-image --image $IMG --base postgres:18-trixie --output /tmp/stannum-ordinal-poc/image-${IMG#*:}; fi
docker rm -f stannum-exit >/dev/null 2>&1 || true; trap 'docker rm -f stannum-exit >/dev/null 2>&1' EXIT
docker run -d --name stannum-exit -p 127.0.0.1:28963:5432 -e POSTGRES_PASSWORD=postgres $IMG postgres -c shared_buffers=256MB -c maintenance_work_mem=1GB -c max_wal_size=4GB -c log_min_messages=log >/dev/null
for i in $(seq 1 90); do docker exec stannum-exit pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break; sleep 2; done
"${PYTHON:-/tmp/stannum-venv/bin/python}" "$ROOT/postgres/tests/exit_during_walk.py" --port 28963 --container stannum-exit "$@"
