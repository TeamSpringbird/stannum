# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file for the weighing and scoring kernels: the candidate
# tree's engine and segment suites at every kernel level the box has, then
# the 150M replay A/B of SRC1 (base) and SRC2 (candidate) under perf stat,
# interleaved, two rounds: disjunctions and conjunctions at score, and
# disjunctions with --full (fewer queries: a full_score disjunction is four
# times a score one).
#   devbox.sh --branch BASE --branch CANDIDATE --dump --cmd-file benchmarks/aws/devbox-simd-ab.sh
set -u
D=$DATA/dump150m
EV=instructions:u,cycles:u,branch-misses:u,task-clock
for s in 1 2; do
  src=$(eval echo \$SRC$s)
  (cd $src && cargo build --release -p bench --bin tnsreplay) > $OUT/build-$s.log 2>&1 || { echo "build $s failed"; tail -30 $OUT/build-$s.log; exit 1; }
  echo "built SRC$s $(cat $src/.devbox-commit)"
done
(cd $SRC2 && for l in scalar avx2 avx512; do
  STANNUM_KERNELS=$l STANNUM_EXPECT_KERNELS=$l cargo test --release -p engine -p segment --lib > $OUT/tests-$l.log 2>&1
  echo "tests at $l: exit $? $(grep -E '^test result' $OUT/tests-$l.log | tr '\n' ' ')"
done)
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
run() { # run TAG BIN ARGS...
  local tag=$1 bin=$2; shift 2
  perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv "$@" > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
  echo "$tag exit $? $(grep -E 'timed instructions' $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt) | $(grep -E '^(instructions|cycles)' $OUT/perf-$tag.csv | cut -d, -f1,3 | tr '\n' ' ')"
}
E="--expect $DATA/pg-ranked.tsv"
for r in 1 2; do
  order="A B"; [ $r = 2 ] && order="B A"
  for x in $order; do bin=$(eval echo \$$x)
    run r$r-$x-disjunction $bin $E --style disjunction --limit 400 --repeat 3
    run r$r-$x-conjunction $bin $E --style conjunction --limit 400 --repeat 3
    run r$r-$x-full-disjunction $bin --style disjunction --limit 150 --repeat 2 --full --out $OUT/full-$x-$r.out
  done
done
cmp $OUT/full-A-1.out $OUT/full-B-1.out && echo "full outputs: same" || echo "full outputs: DIFFERENT"
# the candidate at the scalar level, for the kernels' own share
STANNUM_KERNELS=scalar run scalar-B-disjunction $B $E --style disjunction --limit 400 --repeat 3
