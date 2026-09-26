#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

set -euo pipefail
while systemctl is-active --quiet stannum-image-build; do sleep 10; done
test -f /opt/stannum-benchmark/build/image.json
docker run -d --name stannum-benchmark --shm-size=3g -p 127.0.0.1:5432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_DB=stannum_bench_aws -v stannum-benchmark-data:/var/lib/postgresql stannum-bench:aws postgres -c shared_buffers=2GB -c work_mem=2MB -c jit=off
for attempt in $(seq 1 120); do
 if docker exec stannum-benchmark pg_isready -h 127.0.0.1 -U postgres; then
  docker exec stannum-benchmark psql -h 127.0.0.1 -U postgres -d stannum_bench_aws -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION stannum;' -c 'SELECT version();' > /opt/stannum-benchmark/postgres-ready.txt
  exit 0
 fi
 sleep 2
done
exit 1
