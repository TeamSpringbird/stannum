#!/usr/bin/env bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Correctness gates on the release build, on a development machine: install the release build,
# the cluster tests, the conformance check against TIN 1.0.3 and the Lead reference oracle on the
# pgrx server (script/test-all gates; docs/testing.md). Holds the pgrx lock throughout.
#   benchmarks/local/gates.sh
#   STANNUM_GATES   log directory (default /tmp/stannum-gates)
#   LEAD_REF_DIR    Lead checkout for the oracle (default /tmp/stannum-lead-ref; cloned if missing)
#   PGHOST, PGPORT  server for the oracle (default: the pgrx server, started if needed)
#   PG_CONFIG       pg_config of the server (default: pg_config on PATH)
# Exits nonzero if any gate fails.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
LOGS=${STANNUM_GATES:-/tmp/stannum-gates}
mkdir -p "$LOGS"
export LEAD_REF_DIR=${LEAD_REF_DIR:-/tmp/stannum-lead-ref}
exec python3 script/pgrx-lock.py -- script/test-all --logs "$LOGS" gates
