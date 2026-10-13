# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: array containers (page layout 11) against layout
# 10 on x86, with --forget. SRC1 (A, base) re-encodes the version-6 dump in
# S3 into /data/dump150m-v9 and adds the directory lengths
# (--keep-records) into /data/dump150m-v10; SRC2 (B, candidate) merges
# that alone into /data/dump150m-v11, short Elias-Fano lists becoming
# arrays. Then SRC2's array kernel tests at every level, `tnsef arrays` on
# segment 1 of the layout-10 dump, and conjunctions, disjunctions and
# phrases under score (300 queries) and full_score (150), two rounds (A B,
# then B A), under perf stat, outputs compared. Box time: about 80 minutes.
#   devbox.sh --branch BASE --branch CAND --dump --disk 150 --hours 2.5 \
#     --spot-only --cmd-file benchmarks/aws/devbox-arrays-ab.sh
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
D9=/data/dump150m-v9
D10=/data/dump150m-v10
D11=/data/dump150m-v11
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
for i in 1 2; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay --bin tnsreencode"; [ $i = 2 ] && bins="$bins --bin tnsef"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  step "built SRC$i $(cat $src/.devbox-commit)"
done
grep -m1 -o 'avx512[a-z]*\|ssse3' /proc/cpuinfo | sort -u | tr '\n' ' ' > $OUT/cpu-flags.txt
(cd $SRC2 && cargo test --release -p engine -- array && cargo test --release -p segment -- short_group_lists merge) \
  > $OUT/tests.log 2>&1
step "array tests exit $? $(grep -c 'test result: ok' $OUT/tests.log) suites ok"
$SRC1/target/release/tnsreencode --in $DATA/dump150m --out $D9 --from 6 --jobs 4 \
  > $OUT/reencode-9.txt 2> $OUT/reencode-9.err || { echo "re-encode 6 to 9 failed"; tail -5 $OUT/reencode-9.err; save; exit 1; }
step "re-encoded to 9: $(grep '^areas together' $OUT/reencode-9.txt)"
$SRC1/target/release/tnsreencode --in $D9 --out $D10 --from 9 --keep-records --jobs 2 \
  > $OUT/reencode-10.txt 2> $OUT/reencode-10.err || { echo "re-encode 9 to 10 failed"; tail -5 $OUT/reencode-10.err; save; exit 1; }
step "re-encoded to 10: $(grep '^areas together' $OUT/reencode-10.txt)"
rm -rf $D9
# Two at a time: a merge holds a few copies of its segment (61 GiB with the page cache).
$SRC2/target/release/tnsreencode --in $D10 --out $D11 --from 10 --jobs 2 \
  > $OUT/reencode-11.txt 2> $OUT/reencode-11.err || { echo "re-encode 10 to 11 failed"; tail -5 $OUT/reencode-11.err; dmesg | tail -5 > $OUT/dmesg.txt; save; exit 1; }
step "re-encoded to 11: $(grep '^areas together' $OUT/reencode-11.txt)"
df -h /data | tail -1
$SRC2/target/release/tnsef sample --dump $D10 --out /work/ef.sample --segments 1 > $OUT/tnsef-sample.log 2>&1
$SRC2/target/release/tnsef arrays --sample /work/ef.sample > $OUT/tnsef-arrays.txt 2> $OUT/tnsef-arrays.err
step "tnsef arrays exit $?"
rm -f /work/ef.sample
save
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
dump() { [ $1 = B ] && echo $D11 || echo $D10; }
for x in A B; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $(dump $x) --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 --forget > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
run() { # round arm mode style limit
  local r=$1 x=$2 mode=$3 st=$4 limit=$5 bin extra="" tag
  bin=$(eval echo \$$x); [ $mode = full ] && extra="--full"
  tag=r$r-$x-$mode-$st
  perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $(dump $x) --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
    --style $st --limit $limit --repeat 3 --forget $extra --per-query $OUT/pq-$tag.tsv --out $OUT/out-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
  step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
}
for r in 1 2; do
  order="A B"; [ $r = 2 ] && order="B A"
  for mode in score full; do
    limit=300; [ $mode = full ] && limit=150
    for st in conjunction disjunction phrase; do
      for x in $order; do run $r $x $mode $st $limit; done
      step "r$r $mode $st outputs A/B $(cmp -s $OUT/out-r$r-A-$mode-$st.tsv $OUT/out-r$r-B-$mode-$st.tsv && echo same || echo DIFFERENT)"
      save
    done
  done
done
save
