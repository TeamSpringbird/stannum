# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: a three-way 150M replay A/B of walk changes over
# page layout version 9. SRC3's `tnsreencode` re-encodes the version-6 dump
# in S3 into /data/dump150m-v9 (grid density 64), which SRC1 (A), SRC2 (B)
# and SRC3 (C) all replay: score (300 queries) and full_score (150), three
# styles, interleaved (A B C for score, C B A for full_score), then the
# dense-pair tail of full_score (queries 2, 76, 105, 148, 175, 192), under
# perf stat, with progress and results so far uploaded after each step.
# Box time: about 90 minutes.
#   devbox.sh --branch A --branch B --branch C --dump --disk 130 --hours 3 \
#     --spot-only --cmd-file benchmarks/aws/devbox-walk-ab.sh
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
for i in 1 2 3; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay"; [ $i = 3 ] && bins="$bins --bin tnsreencode"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  step "built SRC$i $(cat $src/.devbox-commit)"
done
$SRC3/target/release/tnsreencode --in $DATA/dump150m --out $D --from 6 --jobs 4 \
  > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode 6 to 9 failed"; tail -5 $OUT/reencode.err; exit 1; }
step "re-encoded to 9: $(grep '^areas together' $OUT/reencode.txt)"
df -h /data | tail -1
grep -E '^(2|76|105|148|175|192):' $DATA/trace.tsv > $OUT/tail-trace.tsv
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay; C=$SRC3/target/release/tnsreplay
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B C; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $D --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for mode in score full; do
  for st in conjunction disjunction phrase; do
    order="A B C"; [ $mode = full ] && order="C B A"
    extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=150; }
    for x in $order; do bin=$(eval echo \$$x)
      tag=r1-$x-$mode-$st
      perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
        --style $st --limit $limit --repeat 3 $extra --per-query $OUT/pq-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
      step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
    done
    save
  done
done
for st in conjunction disjunction phrase; do
  for x in C B A; do bin=$(eval echo \$$x)
    tag=tail-$x-$st
    perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $OUT/tail-trace.tsv \
      --style $st --repeat 3 --full --out $OUT/out-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
    step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | out $(cmp -s $OUT/out-$tag.tsv $OUT/out-tail-C-$st.tsv && echo same || echo DIFFERENT)"
  done
done
save
