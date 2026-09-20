#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
# See LICENSE in the repository root for license terms.
# Run only after the benchmark sweep has finished on the owned temporary host.
set -euo pipefail
BENCH_ROOT=/opt/stannum-benchmark
OUT="$BENCH_ROOT/repo/benchmarks/results/aws-count-probe"
NAME=stannum-count-profile
VOLUME=stannum-count-profile-data
export PGHOST=127.0.0.1 PGPORT=28929 PGUSER=postgres PGPASSWORD=postgres PGDATABASE=benchmark
export PGOPTIONS='-c statement_timeout=3600000 -c jit=off'
unset PGSERVICE PGSERVICEFILE || true
mkdir -p "$OUT/protocol"
cp benchmarks/count_probe.py "$OUT/protocol/"
cp benchmarks/count-probe/queries.json "$OUT/protocol/"
git rev-parse HEAD > "$OUT/protocol/commit.txt"
git diff > "$OUT/protocol/source.patch"
# Keep enough time for build, import, index and verified evidence export.
python3 - <<'PY'
from pathlib import Path
assert float(Path('/proc/uptime').read_text().split()[0]) < 8*3600-75*60, 'Less than 75 minutes remain before expiry'
PY
apt-get update
apt-get install -y --no-install-recommends linux-tools-common "linux-tools-$(uname -r)"
perf --version > "$OUT/perf-version.txt"
sysctl -w kernel.perf_event_paranoid=-1
python3 benchmarks/tin.py build-image --image stannum-bench:count-probe --output "$OUT/image"
python3 benchmarks/published_dataset.py --corpus wikipedia --output "$BENCH_ROOT/datasets/wikipedia"
cleanup() {
  docker logs "$NAME" > "$OUT/server.log" 2>&1 || true
  docker inspect "$NAME" > "$OUT/container.json" 2>/dev/null || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
# Never adopt an unrelated container or volume with the same name.
if docker inspect "$NAME" >/dev/null 2>&1 || docker volume inspect "$VOLUME" >/dev/null 2>&1; then
  echo 'Probe container or volume already exists; refusing to reuse it' >&2
  exit 1
fi
trap cleanup EXIT
docker volume create "$VOLUME" >/dev/null
docker run -d --name "$NAME" --pid=host --cpus 8 --memory 64g --memory-swap 64g --shm-size 1g \
  -p 127.0.0.1:28929:5432 -v "$VOLUME:/var/lib/postgresql" \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=benchmark stannum-bench:count-probe \
  postgres -c shared_buffers=24GB -c maintenance_work_mem=24GB -c work_mem=16MB \
  -c max_parallel_workers=8 -c jit=off -c track_io_timing=on >/dev/null
for attempt in $(seq 1 120); do
  if pg_isready >/dev/null; then break; fi
  sleep 1
done
pg_isready
psql -Xq -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION stannum; CREATE TABLE documents(id text NOT NULL,body text NOT NULL);'
date -u > "$OUT/import-start.txt"
psql -Xq -v ON_ERROR_STOP=1 -c 'COPY documents FROM STDIN WITH(FORMAT csv, HEADER true)' < "$BENCH_ROOT/datasets/wikipedia/data.csv"
psql -XqAt -v ON_ERROR_STOP=1 -c 'SELECT count(*) FROM documents' > "$OUT/row-count.txt"
test "$(cat "$OUT/row-count.txt")" = 5032104
date -u > "$OUT/index-start.txt"
psql -Xq -v ON_ERROR_STOP=1 -c 'CREATE INDEX documents_body_stannum_idx ON documents USING stannum(body);'
date -u > "$OUT/index-finish.txt"
docker update --memory 32g --memory-swap 32g "$NAME" >/dev/null
psql -Xq -v ON_ERROR_STOP=1 -c 'VACUUM ANALYZE documents;'
psql -Xq -v ON_ERROR_STOP=1 -c 'CHECKPOINT;'
psql -XqAt -v ON_ERROR_STOP=1 -c 'SELECT json_object_agg(name,setting) FROM pg_settings' > "$OUT/settings.json"
psql -XqAt -v ON_ERROR_STOP=1 -c "SELECT json_agg(s) FROM stannum.segment_info('documents_body_stannum_idx') s" > "$OUT/segments.json"
mkdir -p "$OUT/symfs/usr/lib/postgresql/18/lib" "$OUT/symfs/usr/lib/postgresql/18/bin"
docker cp "$NAME:/usr/lib/postgresql/18/lib/stannum.so" "$OUT/symfs/usr/lib/postgresql/18/lib/"
docker cp "$NAME:/usr/lib/postgresql/18/bin/postgres" "$OUT/symfs/usr/lib/postgresql/18/bin/"
python3 benchmarks/count_probe.py --output "$OUT/paired" --repetitions 7 --profile --symfs "$OUT/symfs"
