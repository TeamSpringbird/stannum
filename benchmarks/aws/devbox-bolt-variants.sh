# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: build-only options for the 150M replay on one x86
# box, measured against each other (docs/benchmarks.md#bolt-and-cpu-levels).
#   devbox.sh --branch bench/bolt-variants --dump --hours 3 --cmd-file benchmarks/aws/devbox-bolt-variants.sh
# Builds tnsreplay five ways from one source: plain; PGO; x86-64-v3 + PGO;
# x86-64-v4 + PGO (each level trains its own replay profile, as
# script/pgo-build replay does); and PGO + BOLT (script/bolt-build: LBR if
# the box has it, else BOLT's instrumentation). Every build links with
# --emit-relocs (extra sections, the same code). Then replays the first
# $LIMIT trace records (400 per style) under perf stat, every build in each
# of 2 rounds, the order reversed in round 2, plus a load-only run per build
# to subtract the dump's setup. Every replay checks its answers (--expect).
set -u
eval "$(head -1 /root/devbox.sh)" # RUN BUCKET PREFIX: for progress lines
step() {
  echo "== $(date -u +%H:%M:%S) $*"
  echo "$(date -u +%H:%M:%S) $*" >> /work/progress.txt
  /usr/local/bin/aws s3 cp --only-show-errors /work/progress.txt "s3://$BUCKET/$PREFIX/progress.txt" || true
}
# A spot box can be reclaimed mid-run: after each step, $OUT so far goes up
# as the results devbox.sh fetches (the final upload replaces it).
save() {
  tar -C /work -czf /work/partial.tar.gz out &&
    /usr/local/bin/aws s3 cp --only-show-errors /work/partial.tar.gz "s3://$BUCKET/$PREFIX/results.tar.gz" || true
}
D=$DATA/dump150m T=$DATA/trace.tsv E=$DATA/pg-ranked.tsv
LIMIT=${LIMIT:-1200} TRAIN_LIMIT=${TRAIN_LIMIT:-1200}
FV=CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
RELOCS="-C link-arg=-Wl,--emit-relocs"
B=/work/bin
mkdir -p $B

# --- tools: llvm-profdata of the toolchain, BOLT from apt.llvm.org ----------
rustup component add llvm-tools > /dev/null 2>&1
PROFDATA="$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-linux-gnu/bin/llvm-profdata"
[ -x "$PROFDATA" ] || { step "no llvm-profdata"; exit 1; }
llvm=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
curl -fsSL https://apt.llvm.org/llvm-snapshot.gpg.key > /etc/apt/trusted.gpg.d/apt.llvm.org.asc
bolt=
for v in $llvm $((llvm - 1)) $((llvm + 1)); do
  for repo in llvm-toolchain-noble-$v llvm-toolchain-noble; do
    echo "deb http://apt.llvm.org/noble/ $repo main" > /etc/apt/sources.list.d/llvm.list
    apt-get -qq update >> $OUT/bolt-apt.log 2>&1 && apt-get -qq install -y bolt-$v >> $OUT/bolt-apt.log 2>&1 &&
      [ -x /usr/lib/llvm-$v/bin/llvm-bolt ] && { bolt=$v; break 2; }
  done
done
if [ -z "$bolt" ]; then
  rm -f /etc/apt/sources.list.d/llvm.list; apt-get -qq update >> $OUT/bolt-apt.log 2>&1
  for v in 20 19 18; do apt-get -qq install -y bolt-$v >> $OUT/bolt-apt.log 2>&1 && { bolt=$v; break; }; done
fi
step "rustc LLVM $llvm, BOLT ${bolt:-none} ($(/usr/lib/llvm-$bolt/bin/llvm-bolt --version 2>&1 | grep -m1 -i version))"
# LBR (branch stacks) if the VM exposes it, else BOLT's instrumentation.
if perf record -q -e cycles:u -j any,u -o /work/lbr-probe.data -- true > $OUT/lbr-probe.txt 2>&1; then MODE=lbr; else MODE=instrument; fi
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,iTLB-load-misses:u,task-clock
perf stat -e $EV -- true > $OUT/events-probe.txt 2>&1 && ! grep -q 'not supported' $OUT/events-probe.txt ||
  EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
step "BOLT mode $MODE; events $EV"

# --- builds -------------------------------------------------------------------
build() { # name flags
  (env "$FV=$2" cargo build --release -p bench --bin tnsreplay --target-dir /work/t-$1) > $OUT/build-$1.log 2>&1 ||
    { step "build $1 failed"; tail -20 $OUT/build-$1.log; return 1; }
  cp /work/t-$1/release/tnsreplay $B/$1
  step "built $1 ($(grep -ci 'mismatch\|out of date\|no profile' $OUT/build-$1.log) profile warnings)"
  save
}
train() { # binary log
  local bin=$1 log=$2
  $bin --dump $D --trace $T --expect $E --limit $TRAIN_LIMIT --repeat 1 > $OUT/$log.txt 2> $OUT/$log.err
  local st=$?
  step "trained $log: exit $st, $(grep expect: $OUT/$log.txt)"
  return $st
}
build plain "$RELOCS"
for L in base v3 v4; do
  cpu=; [ $L != base ] && cpu="-C target-cpu=x86-64-$L"
  build gen-$L "$cpu -C profile-generate=/work/pgo-raw-$L" || continue
  train $B/gen-$L train-pgo-$L || continue
  "$PROFDATA" merge --sparse -o /work/pgo-$L.profdata /work/pgo-raw-$L
  name=pgo-$L; [ $L = base ] && name=pgo
  build $name "$cpu -C profile-use=/work/pgo-$L.profdata $RELOCS"
  rm -rf /work/t-gen-$L $B/gen-$L
done
if [ -n "$bolt" ] && [ -x $B/pgo ]; then
  # The training (a shell function, so through bash -c) runs the copy that
  # bolt-build swaps for its instrumented one.
  cp $B/pgo $B/pgo-bolt-in
  export -f train step; export D T E TRAIN_LIMIT OUT BUCKET PREFIX
  script/bolt-build $B/pgo-bolt-in --out $B/pgo-bolt --mode $MODE --profile /work/bolt-profile -- \
    bash -c "train $B/pgo-bolt-in train-bolt" > $OUT/bolt-build.log 2>&1
  step "BOLT exit $? $(grep -m1 '^wrote' $OUT/bolt-build.log)"
  rm -f $B/pgo-bolt-in
fi
for b in $B/*; do size -A $b | awk -v b=$(basename $b) '$1 ~ /^\.text|^\.bolt|^\.rodata$/ {print b, $1, $2}'; done > $OUT/sizes.txt
builds=$(cd $B && for b in plain pgo pgo-v3 pgo-v4 pgo-bolt; do [ -x $b ] && echo $b; done | tr '\n' ' ')
rev=$(echo $builds | tr ' ' '\n' | tac | tr '\n' ' ')
step "measuring: $builds"

# --- measurement --------------------------------------------------------------
for b in $builds; do
  perf stat -x, -e $EV -o $OUT/perf-load-$b.csv -- $B/$b --dump $D --trace $T --limit 1 --repeat 1 > $OUT/replay-load-$b.txt 2> $OUT/replay-load-$b.err
done
step "load-only runs done"; save
for r in 1 2; do
  order=$builds; [ $r = 2 ] && order=$rev
  for b in $order; do
    tag=r$r-$b
    perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $B/$b --dump $D --trace $T --expect $E \
      --limit $LIMIT --repeat 3 > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
    step "$tag exit $? $(grep expect: $OUT/replay-$tag.txt) | $(grep -E '^(conjunction|disjunction|phrase) +[0-9]+ +[0-9.]+ +[0-9.]+ +[0-9.]+$' $OUT/replay-$tag.txt | awk '{printf "%s %s ", substr($1,1,4), $5}')"
    save
  done
done
