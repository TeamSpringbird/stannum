#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

set -euo pipefail
BENCH_ROOT=/opt/stannum-benchmark
OUT="$BENCH_ROOT/repo/benchmarks/results/aws-crossover"
export OUT
NAME=stannum-crossover
VOLUME=stannum-crossover-data
export PGHOST=127.0.0.1 PGPORT=28929 PGUSER=postgres PGPASSWORD=postgres PGDATABASE=benchmark
export PGOPTIONS='-c statement_timeout=3600000 -c jit=off'
unset PGSERVICE PGSERVICEFILE || true
mkdir -p "$OUT/protocol"
cp benchmarks/count{_probe,_crossover,_crossover_report,_oracle}.py "$OUT/protocol/"
cp benchmarks/aws/crossover.sh benchmarks/aws/crossover_campaign.py "$OUT/protocol/"
cp docs/benchmarks/count-crossover/100k-selection.json "$OUT/protocol/frozen-rule.json"
git rev-parse HEAD > "$OUT/protocol/commit.txt"
git diff > "$OUT/protocol/source.patch"
apt-get update
apt-get install -y --no-install-recommends linux-tools-common "linux-tools-$(uname -r)"
perf --version > "$OUT/perf-version.txt"
sysctl -w kernel.perf_event_paranoid=-1
python3 -m venv --system-site-packages "$BENCH_ROOT/venv"
"$BENCH_ROOT/venv/bin/pip" install 'psycopg[binary]==3.3.6'
export PATH="$BENCH_ROOT/venv/bin:$PATH"
python3 - <<'PY'
import boto3,os
from pathlib import Path
root=Path('/opt/stannum-benchmark/datasets/wikipedia');root.mkdir(parents=True,exist_ok=True)
prefix='f487fbaaf5039a7b92e1de4efb40e0f7c6fcdb86/wikipedia/'
s3=boto3.client('s3')
for name in ('data.csv','data-manifest.json','queries.json','source.json','verification.json'):
    s3.download_file('springbird-dev-stannum-corpus-cache-860510875764',prefix+name,str(root/name))
PY
python3 benchmarks/published_dataset.py --corpus wikipedia --output "$BENCH_ROOT/datasets/wikipedia"
python3 benchmarks/tin.py build-image --image stannum-bench:crossover --output "$OUT/image"
if docker inspect "$NAME" >/dev/null 2>&1 || docker volume inspect "$VOLUME" >/dev/null 2>&1; then
  echo 'Refusing existing container/volume ownership collision' >&2; exit 1
fi
cleanup() {
  docker logs "$NAME" > "$OUT/server.log" 2>&1 || true
  docker inspect "$NAME" > "$OUT/container.json" 2>/dev/null || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker volume rm "$VOLUME" >/dev/null 2>&1 || true
}
trap cleanup EXIT
docker volume create "$VOLUME" >/dev/null
docker run -d --name "$NAME" --pid=host --cpus 8 --memory 64g --memory-swap 64g --shm-size 1g \
  -p 127.0.0.1:28929:5432 -v "$VOLUME:/var/lib/postgresql" \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=benchmark stannum-bench:crossover \
  postgres -c shared_buffers=24GB -c maintenance_work_mem=24GB -c work_mem=16MB \
  -c max_parallel_workers=8 -c jit=off -c track_io_timing=on -c autovacuum=off >/dev/null
for attempt in $(seq 1 120); do
  if pg_isready >/dev/null; then break; fi
  sleep 1
done
pg_isready
psql -Xq -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION stannum; CREATE EXTENSION pg_visibility; CREATE TABLE documents(id text NOT NULL,body text NOT NULL) WITH(autovacuum_enabled=false);'
date -u > "$OUT/import-start.txt"
psql -Xq -v ON_ERROR_STOP=1 -c 'COPY documents FROM STDIN WITH(FORMAT csv, HEADER true)' < "$BENCH_ROOT/datasets/wikipedia/data.csv"
psql -XqAt -v ON_ERROR_STOP=1 -c 'SELECT count(*) FROM documents' > "$OUT/row-count.txt"
test "$(cat "$OUT/row-count.txt")" = 5032104
date -u > "$OUT/index-start.txt"
psql -Xq -v ON_ERROR_STOP=1 -c 'CREATE INDEX documents_idx ON documents USING stannum(body);'
date -u > "$OUT/index-finish.txt"
docker update --memory 32g --memory-swap 32g "$NAME" >/dev/null
mkdir -p "$OUT/symfs/usr/lib/postgresql/18/lib" "$OUT/symfs/usr/lib/postgresql/18/bin"
docker cp "$NAME:/usr/lib/postgresql/18/lib/stannum.so" "$OUT/symfs/usr/lib/postgresql/18/lib/"
docker cp "$NAME:/usr/lib/postgresql/18/bin/postgres" "$OUT/symfs/usr/lib/postgresql/18/bin/"
sha256sum "$OUT/symfs/usr/lib/postgresql/18/lib/stannum.so" > "$OUT/library-sha256.txt"
python3 benchmarks/aws/crossover_campaign.py
