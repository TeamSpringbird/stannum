#!/bin/bash
# Build the 15M mock database once under AWS-like proportions, saving it for load-database runs.
set -u
export PATH=/opt/homebrew/bin:$PATH
cd /Users/uri/.t3/worktrees/lead/t3code-bf0ed789
IMG=${STANNUM_IMAGE:-stannum-bench:arm64-a1e6a95}
until docker images --format "{{.Repository}}:{{.Tag}}" | grep -qx "$IMG"; do sleep 20; done
D=/tmp/stannum-ordinal-poc/local-driver
DS="$HOME/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange"
R=${STANNUM_MOCK:-/tmp/stannum-ordinal-poc/mock15m}; mkdir -p $R; rm -rf $R/db $R/build
echo "== build $(date)"
python3 benchmarks/tin.py --driver "$D" run ${STANNUM_SOURCE:+--source-manifest "$STANNUM_SOURCE"} --published-corpus stackexchange --dataset "$DS" --rows 15000000 \
  --validation-rows 1000 --validation-queries 3 --ranked-validation-queries 1 --engines stannum --workload topk --style mixed \
  --warmup 10 --seconds 120 --clients 8 --cpus 8 --memory 5g --build-memory 12g --shared-buffers 2GB --maintenance-work-mem 4GB \
  --setup-timeout-seconds 14400 --save-database $R/db --image $IMG --output $R/build > $R/build.log 2>&1
echo "exit $? $(date)"
python3 - "$R" <<'PY'
import json,sys
R=sys.argv[1]
m=json.load(open(R+'/build/manifest.json')); j=m['jobs'][0]
print('status', m.get('status'), j.get('status'), (j.get('error') or '')[:200])
print('import', round(j.get('import_seconds',0)), 'build', round(j.get('index_build_seconds',0)), 'segments', len(j.get('segments_after_build',[])), 'sizes', j.get('sizes'))
c=json.load(open(R+'/build/comparison.json'))[0]
print('baseline mixed (5g/2GB):', round(c['completed']/120,1), 'QPS p50', c['p50_ms'], 'p99', c['p99_ms'], {f:(v['completed'],v['p50_ms']) for f,v in c['families'].items()})
PY
echo "MOCK-BUILD-DONE $(date)"
