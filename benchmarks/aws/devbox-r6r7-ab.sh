# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: a four-way 150M replay A/B of scorer and walk
# changes over page layout version 9. SRC4's `tnsreencode` re-encodes the
# version-6 dump in S3 into /data/dump150m-v9 (grid density 64), which SRC1
# (A) to SRC4 (D) all replay: disjunctions under score (300 queries) and
# full_score (150) in two interleaved rounds (A B C D, then D C B A), with
# memory-stall counters on full_score; then one round of full_score
# conjunctions and phrases (150) and score conjunctions (300). Under perf
# stat, with progress and results so far uploaded after each step. Box
# time: about 100 minutes.
#   devbox.sh --branch A --branch B --branch C --branch D --dump --disk 130 \
#     --hours 3 --spot-only --cmd-file benchmarks/aws/devbox-r6r7-ab.sh
set -u
eval "$(head -1 /root/devbox.sh)" # RUN BUCKET PREFIX: for progress lines
step() {
  echo "== $(date -u +%H:%M:%S) $*"
  echo "$(date -u +%H:%M:%S) $*" >> /work/progress.txt
  /usr/local/bin/aws s3 cp --only-show-errors /work/progress.txt "s3://$BUCKET/$PREFIX/progress.txt" || true
}
save() {
  tar -C /work -czf /work/partial.tar.gz out &&
    /usr/local/bin/aws s3 cp --only-show-errors /work/partial.tar.gz "s3://$BUCKET/$PREFIX/results.tar.gz" || true
}
D=/data/dump150m-v9
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
# Memory-level parallelism: cycles stalled on an L2 miss, and L1D misses
# outstanding per cycle (their sum) and cycles with any outstanding.
MEM=cycle_activity.stalls_l2_miss:u,l1d_pend_miss.pending:u,l1d_pend_miss.pending_cycles:u
perf stat -x, -e $EV,$MEM -o /dev/null -- true 2> /dev/null || MEM=""
for i in 1 2 3 4; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay"; [ $i = 4 ] && bins="$bins --bin tnsreencode"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  step "built SRC$i $(cat $src/.devbox-commit)"
done
$SRC4/target/release/tnsreencode --in $DATA/dump150m --out $D --from 6 --jobs 4 \
  > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode 6 to 9 failed"; tail -5 $OUT/reencode.err; exit 1; }
step "re-encoded to 9: $(grep '^areas together' $OUT/reencode.txt)"
df -h /data | tail -1
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
C=$SRC3/target/release/tnsreplay; E=$SRC4/target/release/tnsreplay
bin_of() { [ $1 = D ] && echo $E || eval echo \$$1; }
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B C D; do bin=$(bin_of $x)
  perf stat -x, -e $EV${MEM:+,$MEM} -o $OUT/perf-load-$x.csv -- $bin --dump $D --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
run() { # round variant mode style limit
  local r=$1 x=$2 mode=$3 st=$4 limit=$5 bin extra="" tag ev=$EV
  bin=$(bin_of $x); [ $mode = full ] && { extra="--full"; ev=$EV${MEM:+,$MEM}; }
  tag=r$r-$x-$mode-$st
  perf stat -x, -e $ev -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
    --style $st --limit $limit --repeat 3 $extra --per-query $OUT/pq-$tag.tsv --out $OUT/out-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
  step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
}
same() { # round mode style
  local o=""
  for x in B C D; do
    o="$o $x:$(cmp -s $OUT/out-r$1-A-$2-$3.tsv $OUT/out-r$1-$x-$2-$3.tsv && echo same || echo DIFFERENT)"
  done
  step "r$1 $2 $3 outputs vs A$o"
}
for r in 1 2; do
  order="A B C D"; [ $r = 2 ] && order="D C B A"
  for mode in score full; do
    limit=300; [ $mode = full ] && limit=150
    for x in $order; do run $r $x $mode disjunction $limit; done
    same $r $mode disjunction
    save
  done
done
for st in conjunction phrase; do
  for x in A B C D; do run 1 $x full $st 150; done
  same 1 full $st
  save
done
for x in A B C D; do run 1 $x score conjunction 300; done
same 1 score conjunction
save
