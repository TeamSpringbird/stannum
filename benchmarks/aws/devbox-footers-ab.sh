# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay A/B of a format change. SRC1 (base)
# replays the dump in S3 (page layout version 6); SRC2 (candidate) re-encodes
# it on the box with its own `tnsreencode` into /data/dump150m-v8 and replays
# that. Interleaved (A B for score, B A for full_score), one round, three
# styles, under perf stat: about 50 minutes of box time.
#   devbox.sh --branch BASE --branch CANDIDATE --dump --cmd-file benchmarks/aws/devbox-footers-ab.sh
set -u
D=$DATA/dump150m
D2=/data/dump150m-v8
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
(cd $SRC1 && cargo build --release -p bench --bin tnsreplay) > $OUT/build-1.log 2>&1 || { echo "build 1 failed"; tail -30 $OUT/build-1.log; exit 1; }
echo "built SRC1 $(cat $SRC1/.devbox-commit)"
(cd $SRC2 && cargo build --release -p bench --bin tnsreplay --bin tnsreencode) > $OUT/build-2.log 2>&1 || { echo "build 2 failed"; tail -30 $OUT/build-2.log; exit 1; }
echo "built SRC2 $(cat $SRC2/.devbox-commit)"
$SRC2/target/release/tnsreencode --in $D --out $D2 > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode failed"; tail -5 $OUT/reencode.err; exit 1; }
echo "re-encoded: $(tail -1 $OUT/reencode.txt | cut -c1-160)"; df -h /data | tail -1
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B; do bin=$(eval echo \$$x); dump=$D; [ $x = B ] && dump=$D2
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $dump --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for r in 1; do
  for mode in score full; do
    for st in conjunction disjunction phrase; do
      order="A B"; [ $mode = full ] && order="B A"
      extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=150; }
      for x in $order; do bin=$(eval echo \$$x); dump=$D; [ $x = B ] && dump=$D2
        tag=r$r-$x-$mode-$st
        perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $dump --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
          --style $st --limit $limit --repeat 3 $extra > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
        echo "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt) | $(grep -A1 'pages pinned per query' $OUT/replay-$tag.txt | tail -1)"
      done
    done
  done
done
