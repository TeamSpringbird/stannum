# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay of up to three trees (SRC1 base,
# SRC2 and SRC3 candidates) under perf stat, one round each of score
# disjunctions and full_score disjunctions, with a load-only baseline per
# tree (the dump's load is in every count). The short form of
# devbox-simd-ab.sh, for a box kept to a quarter hour.
#   devbox.sh --branch BASE --branch CANDIDATE [--branch CANDIDATE2] --dump --cmd-file benchmarks/aws/devbox-simd-ab2.sh
set -u
D=$DATA/dump150m
EV=instructions:u,cycles:u,branch-misses:u
bins=()
for s in 1 2 3; do
  src=$(eval echo \${SRC$s:-})
  [ -n "$src" ] || continue
  (cd $src && cargo build --release -p bench --bin tnsreplay) > $OUT/build-$s.log 2>&1 || { echo "build $s failed"; tail -30 $OUT/build-$s.log; exit 1; }
  echo "built SRC$s $(cat $src/.devbox-commit)"
  bins+=("$s:$src/target/release/tnsreplay")
done
run() { # run TAG BIN ARGS...
  local tag=$1 bin=$2; shift 2
  perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $D --trace $DATA/trace.tsv "$@" > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
  echo "$tag exit $? | $(grep expect: $OUT/replay-$tag.txt) | $(grep -E 'instructions|cycles|branch' $OUT/perf-$tag.csv | awk -F, '{printf " %s=%.3fG", $3, $1/1e9}')"
}
E="--expect $DATA/pg-ranked.tsv"
for b in "${bins[@]}"; do s=${b%%:*}; bin=${b#*:}
  run load-$s $bin --style conjunction --limit 1 --repeat 1
done
for b in "${bins[@]}"; do s=${b%%:*}; bin=${b#*:}
  run disjunction-$s $bin $E --style disjunction --limit 400 --repeat 3
done
for b in "${bins[@]}"; do s=${b%%:*}; bin=${b#*:}
  run full-disjunction-$s $bin --style disjunction --limit 150 --repeat 2 --full --out $OUT/full-$s.out
done
for s in 2 3; do [ -f $OUT/full-$s.out ] && { cmp $OUT/full-1.out $OUT/full-$s.out && echo "full outputs 1 vs $s: same" || echo "full outputs 1 vs $s: DIFFERENT"; }; done
