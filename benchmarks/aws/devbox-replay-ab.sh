# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay A/B of two trees (SRC1 base, SRC2
# candidate) under perf stat, interleaved, 2 rounds, plus tnsef if SRC2 has it.
#   devbox.sh --branch BASE --branch CANDIDATE --dump --cmd-file benchmarks/aws/devbox-replay-ab.sh
set -u
D=$DATA/dump150m
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
for s in 1 2; do
  src=$(eval echo \$SRC$s)
  (cd $src && cargo build --release -p bench --bin tnsreplay $( [ -f bench/src/bin/tnsef.rs ] && echo --bin tnsef )) > $OUT/build-$s.log 2>&1 || { echo "build $s failed"; tail -30 $OUT/build-$s.log; exit 1; }
  echo "built SRC$s $(cat $src/.devbox-commit)"
done
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
# microbenchmark (ef-simd only): Elias-Fano decode, portable vs each level on this CPU
if [ -x $SRC2/target/release/tnsef ]; then
  $SRC2/target/release/tnsef sample --dump $D --out /work/ef.sample --segments 1 > $OUT/tnsef-sample.log 2>&1
  $SRC2/target/release/tnsef time --sample /work/ef.sample > $OUT/tnsef-time.txt 2> $OUT/tnsef-time.err
  echo "tnsef time exit $?"; cat $OUT/tnsef-time.txt
fi
# load-only baseline: one query, one repeat (setup cost inside the perf totals)
for x in A B; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $D --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for r in 1 2; do
  for st in conjunction disjunction; do
    order="A B"; [ $r = 2 ] && order="B A"
    for x in $order; do bin=$(eval echo \$$x)
      tag=r$r-$x-$st
      perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
        --style $st --limit 400 --repeat 3 > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
      echo "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt)"
    done
  done
done
