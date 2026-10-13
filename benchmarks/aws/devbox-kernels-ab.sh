# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay A/B of kernel changes over page
# layout 9. SRC1 (A, the base) re-encodes the version-6 dump in S3 to 9
# with its own `tnsreencode`; SRC2 (B) and SRC3 (C, optional) are the
# candidates. Then, on the box's AVX-512 CPU:
#   1. the last source's engine and segment tests at every kernel level
#      (STANNUM_EXPECT_KERNELS=avx2,avx512, STANNUM_EXPECT_VBMI=1);
#   2. `tnsef time` (SRC2's) on the version-9 containers of one segment:
#      Elias-Fano decode per member, portable and each level;
#   3. exactness: each candidate over the whole trace with --expect, and
#      every source's --full outputs into OUT (compare them at home);
#   4. the A/B: one round, three styles, score (300 queries) and
#      full_score (150), interleaved (A B C, then C B A), under perf stat.
# Progress and results so far are uploaded after each step. Box time:
# about 90 minutes.
#   devbox.sh --branch BASE --branch CAND [--branch CAND2] --dump --disk 120 --hours 3 \
#     --spot-only --cmd-file benchmarks/aws/devbox-kernels-ab.sh
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
D=$DATA/dump150m
V9=/data/dump150m-v9
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
names="A B"; [ -n "${SRC3:-}" ] && names="A B C"
last=$(echo $names | awk '{print $NF}')
i=0
for x in $names; do
  i=$((i + 1)); src=$(eval echo \$SRC$i); bins="--bin tnsreplay"
  [ $i = 1 ] && bins="$bins --bin tnsreencode"; [ $i = 2 ] && bins="$bins --bin tnsef"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$x.log 2>&1 || { echo "build $x failed"; tail -30 $OUT/build-$x.log; exit 1; }
  eval "src_$x=$src; bin_$x=$src/target/release/tnsreplay"
  step "built $x $(cat $src/.devbox-commit)"
done
# 1. tests (the last source)
src=$(eval echo \$src_$last)
(cd $src && STANNUM_EXPECT_KERNELS=avx2,avx512 STANNUM_EXPECT_VBMI=1 cargo test --release -p engine -p segment) > $OUT/tests.log 2>&1
step "tests $last exit $? $(grep -E '^test result' $OUT/tests.log | awk '{p+=$4; f+=$6} END {print p" passed "f" failed"}')"
# 2. re-encode, microbenchmark
$src_A/target/release/tnsreencode --in $D --out $V9 --from 6 --jobs 4 > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode failed"; tail -5 $OUT/reencode.err; save; exit 1; }
step "re-encoded to 9: $(grep '^areas together' $OUT/reencode.txt)"
df -h /data | tail -1
$src_B/target/release/tnsef sample --dump $V9 --out /work/ef.sample --segments 1 > $OUT/tnsef-sample.log 2>&1
$src_B/target/release/tnsef time --sample /work/ef.sample > $OUT/tnsef-time.txt 2> $OUT/tnsef-time.err
step "tnsef exit $? $(head -3 $OUT/tnsef-time.txt | tr '\n' ' ')"
save
# 3. exactness
for st in conjunction disjunction phrase; do
  for x in $names; do bin=$(eval echo \$bin_$x)
    if [ $x != A ]; then
      $bin --dump $V9 --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv --style $st --repeat 1 > $OUT/expect-$x-$st.txt 2>&1
      step "expect $x $st exit $? $(grep -E '^expect' $OUT/expect-$x-$st.txt | head -1)"
    fi
    $bin --dump $V9 --trace $DATA/trace.tsv --style $st --repeat 1 --full --out $OUT/full-$x-$st.out > $OUT/full-$x-$st.txt 2>&1
    step "full $x $st exit $? $(wc -l < $OUT/full-$x-$st.out) lines $(md5sum < $OUT/full-$x-$st.out | cut -c1-12)"
  done
  save
done
# 4. the A/B
reversed=$(echo $names | tr ' ' '\n' | tac | tr '\n' ' ')
for x in $names; do bin=$(eval echo \$bin_$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $V9 --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for mode in score full; do
  for st in conjunction disjunction phrase; do
    order=$names; [ $mode = full ] && order=$reversed
    extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=150; }
    for x in $order; do bin=$(eval echo \$bin_$x)
      tag=r1-$x-$mode-$st
      perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $V9 --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
        --style $st --limit $limit --repeat 3 $extra > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
      step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
    done
    save
  done
done
