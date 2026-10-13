# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: a two-way 150M replay A/B of a disjunction walk
# change over page layout version 9. SRC2's `tnsreencode` re-encodes the
# version-6 dump in S3 into /data/dump150m-v9 (grid density 64), which SRC1
# (A) and SRC2 (B) both replay: disjunctions under score (300 queries) and
# full_score (150) in two interleaved rounds (A B, then B A), then one round
# of score conjunctions and phrases (unchanged code, for exactness), under
# perf stat, with progress and results so far uploaded after each step.
# Box time: about 50 minutes.
#   devbox.sh --branch A --branch B --dump --disk 130 --hours 2 \
#     --spot-only --cmd-file benchmarks/aws/devbox-r3r4-ab.sh
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
for i in 1 2; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay"; [ $i = 2 ] && bins="$bins --bin tnsreencode"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  step "built SRC$i $(cat $src/.devbox-commit)"
done
$SRC2/target/release/tnsreencode --in $DATA/dump150m --out $D --from 6 --jobs 4 \
  > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode 6 to 9 failed"; tail -5 $OUT/reencode.err; exit 1; }
step "re-encoded to 9: $(grep '^areas together' $OUT/reencode.txt)"
df -h /data | tail -1
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $D --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
run() { # round binary-name mode style limit
  local r=$1 x=$2 mode=$3 st=$4 limit=$5 bin extra="" tag
  bin=$(eval echo \$$x); [ $mode = full ] && extra="--full"
  tag=r$r-$x-$mode-$st
  perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
    --style $st --limit $limit --repeat 3 $extra --per-query $OUT/pq-$tag.tsv --out $OUT/out-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
  step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
}
for r in 1 2; do
  order="A B"; [ $r = 2 ] && order="B A"
  for mode in score full; do
    limit=300; [ $mode = full ] && limit=150
    for x in $order; do run $r $x $mode disjunction $limit; done
    step "r$r $mode disjunction outputs $(cmp -s $OUT/out-r$r-A-$mode-disjunction.tsv $OUT/out-r$r-B-$mode-disjunction.tsv && echo same || echo DIFFERENT)"
    save
  done
done
for st in conjunction phrase; do
  for x in A B; do run 1 $x score $st 300; done
  step "r1 score $st outputs $(cmp -s $OUT/out-r1-A-score-$st.tsv $OUT/out-r1-B-score-$st.tsv && echo same || echo DIFFERENT)"
  save
done
save
