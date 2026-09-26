#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Backends killed mid-walk on Linux (postgres/tests/exit_during_walk.py): builds an image of the
# working tree, serves it as container stannum-exit on 127.0.0.1:28963, runs the test, removes the
# container. Needs Docker and a Python with psycopg (docs/testing.md).
#   exit-test.sh [test args...]
#   STANNUM_EXIT_IMAGE   reuse an image built earlier instead of building one
#   STANNUM_PYTHON       interpreter with psycopg installed (default: python3)
#   STANNUM_EXIT_OUTPUT  build directory for the image (default: a temporary directory)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PYTHON=${STANNUM_PYTHON:-python3}
if ! "$PYTHON" -c 'import psycopg' 2>/dev/null; then
    echo "exit-test.sh: $PYTHON cannot import psycopg; set STANNUM_PYTHON to an interpreter that can" >&2
    echo "  (for example: python3 -m venv /tmp/stannum-venv && /tmp/stannum-venv/bin/pip install 'psycopg[binary]')" >&2
    exit 2
fi
IMG=${STANNUM_EXIT_IMAGE:-}
if [ -z "$IMG" ]; then
    IMG=stannum-bench:exit-$(date +%s)
    OUTPUT=${STANNUM_EXIT_OUTPUT:-$(mktemp -d "${TMPDIR:-/tmp}/stannum-exit-image.XXXXXX")}
    python3 "$ROOT/benchmarks/tin.py" build-image --image "$IMG" --base postgres:18-trixie --output "$OUTPUT"
fi
docker rm -f stannum-exit >/dev/null 2>&1 || true
trap 'docker rm -f stannum-exit >/dev/null 2>&1' EXIT
# A throwaway container: its password is not a secret, and it reaches the test through the environment.
docker run -d --name stannum-exit -p 127.0.0.1:28963:5432 -e POSTGRES_PASSWORD=postgres "$IMG" postgres \
    -c shared_buffers=256MB -c maintenance_work_mem=1GB -c max_wal_size=4GB -c log_min_messages=log >/dev/null
for _ in $(seq 1 90); do docker exec stannum-exit pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break; sleep 2; done
PGHOST=127.0.0.1 PGPORT=28963 PGUSER=postgres PGPASSWORD=postgres PGDATABASE=postgres \
    "$PYTHON" "$ROOT/postgres/tests/exit_during_walk.py" --container stannum-exit "$@"
