#!/bin/bash
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A workload on the local mock from the saved database: mock15m-run.sh IMAGE_TAG LABEL STYLE UPDATES [SECONDS]
set -u
export PATH=/opt/homebrew/bin:$PATH
# The Docker VM disk is served from the host cache at RAM speed: cap reads at what the
# i7i.8xlarge NVMe delivered inside the container (13.8M reads and 241 GB in 688 s).
export STANNUM_DOCKER_RUN_ARGS="${STANNUM_DOCKER_RUN_ARGS:---device-read-iops /dev/vdb:20000 --device-read-bps /dev/vdb:400mb}"
IMG=$1; LABEL=$2; STYLE=$3; UPDATES=$4; SECONDS_=${5:-180}
cd /Users/uri/.t3/worktrees/lead/t3code-bf0ed789
D=/tmp/stannum-ordinal-poc/local-driver
DS="$HOME/Library/Application Support/LeadBenchmarks/datasets/planetscale-stackexchange"
R=${STANNUM_MOCK:-/tmp/stannum-ordinal-poc/mock15m}/runs/$LABEL-$STYLE-u$UPDATES; rm -rf $R $R.log; mkdir -p $(dirname $R)
# Validation warms the VM's global page cache with pages that stay cached, uncharged, after
# the harness restarts the container: drop the cache the moment the measurement phase starts,
# so the run reads the index cold except for its own 2 GB of shared buffers, as on AWS.
( for i in $(seq 1 3000); do f=$(ls $R/stannum/resources.jsonl 2>/dev/null); if [ -n "$f" ] && tail -1 "$f" | grep -q driver-warmup-and-measurement; then docker run --rm --privileged alpine sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'; echo "dropped caches at measurement start $(date)" >> $R.log; break; fi; sleep 1; done ) &
python3 benchmarks/tin.py --driver "$D" run ${STANNUM_SOURCE:+--source-manifest "$STANNUM_SOURCE"} --published-corpus stackexchange --dataset "$DS" --rows 15000000 \
  --validation-rows 1000 --validation-queries 2 --ranked-validation-queries 1 --engines stannum --workload topk --style $STYLE --updates $UPDATES \
  --warmup 10 --seconds $SECONDS_ --clients 8 --cpus 8 --memory 5g --build-memory 12g --shared-buffers 2GB --maintenance-work-mem 4GB \
  --setup-timeout-seconds 14400 --load-database ${STANNUM_MOCK:-/tmp/stannum-ordinal-poc/mock15m}/db --image $IMG --output $R > $R.log 2>&1
python3 - "$R" "$LABEL" "$STYLE" "$SECONDS_" <<'PY'
import json,sys
R,label,style,secs=sys.argv[1:]; secs=float(secs)
m=json.load(open(R+'/manifest.json')); j=m['jobs'][0]
c=json.load(open(R+'/comparison.json'))[0]
rows=[r for r in (json.loads(l) for l in open(R+'/stannum/resources.jsonl')) if 'counters' in r]
meas=[r for r in rows if r.get('phase')=='driver-warmup-and-measurement']
# Counters reset when the harness restarts the container; keep the longest monotonic stretch.
usage=lambda r:int(r['counters']['cpu.stat'].split()[1])
bounds=[0]+[i for i in range(1,len(meas)) if usage(meas[i])<usage(meas[i-1])]+[len(meas)]
lo,hi=max(zip(bounds,bounds[1:]),key=lambda b:b[1]-b[0])
meas=meas[lo:hi]
def io(r):
    t=0
    for line in r['counters'].get('io.stat','').splitlines():
        for kv in line.split()[1:]:
            k,v=kv.split('='); t+= int(v) if k=='rbytes' else 0
    return t
cpu=lambda r:int(r['counters']['cpu.stat'].split()[1])
gb=(io(meas[-1])-io(meas[0]))/2**30 if len(meas)>1 else float('nan'); dt=meas[-1]['finished']-meas[0]['finished'] if len(meas)>1 else 1
print(f"{label} {style} u{style and sys.argv[3]}: {j.get('status')} {c['completed']/secs:.1f} QPS p50 {c['p50_ms']:.0f} p95 {c['p95_ms']:.0f} p99 {c['p99_ms']:.0f} ms | disk read {gb:.1f} GB ({gb*1024/c['completed']:.1f} MB/query) cpu {(cpu(meas[-1])-cpu(meas[0]))/1e6/dt:.2f} cores | "+' '.join(f"{f} p50 {v['p50_ms']:.0f}" for f,v in c['families'].items()))
PY
