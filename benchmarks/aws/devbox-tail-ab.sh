# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: a three-way 150M replay A/B of a change to the
# walk over page layout version 8. SRC1 (A, the version-6 base) replays the
# dump in S3; SRC2 (B, layout 8) re-encodes it on the box with its own
# `tnsreencode` into /data/dump150m-v8, which SRC2 and SRC3 (C, the
# candidate) replay. Interleaved (B C for score, C B A for full_score), one
# round, three styles (score 300 queries, full_score 200), under perf stat:
# about 80 minutes of box time.
#   devbox.sh --branch BASE --branch V8 --branch CANDIDATE --dump --cmd-file benchmarks/aws/devbox-tail-ab.sh
set -u
D=$DATA/dump150m
D2=/data/dump150m-v8
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
(cd $SRC1 && cargo build --release -p bench --bin tnsreplay) > $OUT/build-1.log 2>&1 || { echo "build 1 failed"; tail -30 $OUT/build-1.log; exit 1; }
echo "built SRC1 $(cat $SRC1/.devbox-commit)"
(cd $SRC2 && cargo build --release -p bench --bin tnsreplay --bin tnsreencode) > $OUT/build-2.log 2>&1 || { echo "build 2 failed"; tail -30 $OUT/build-2.log; exit 1; }
echo "built SRC2 $(cat $SRC2/.devbox-commit)"
(cd $SRC3 && cargo build --release -p bench --bin tnsreplay) > $OUT/build-3.log 2>&1 || { echo "build 3 failed"; tail -30 $OUT/build-3.log; exit 1; }
echo "built SRC3 $(cat $SRC3/.devbox-commit)"
$SRC2/target/release/tnsreencode --in $D --out $D2 > $OUT/reencode.txt 2> $OUT/reencode.err || { echo "re-encode failed"; tail -5 $OUT/reencode.err; exit 1; }
echo "re-encoded: $(tail -1 $OUT/reencode.txt | cut -c1-160)"; df -h /data | tail -1
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay; C=$SRC3/target/release/tnsreplay
dump_of() { if [ "$1" = A ]; then echo $D; else echo $D2; fi; }
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B C; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $(dump_of $x) --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for mode in score full; do
  for st in conjunction disjunction phrase; do
    order="B C"; [ $mode = full ] && order="C B A"
    extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=200; }
    for x in $order; do bin=$(eval echo \$$x)
      tag=r1-$x-$mode-$st
      perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $(dump_of $x) --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
        --style $st --limit $limit --repeat 3 $extra --per-query $OUT/pq-$tag.tsv > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
      echo "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt) | $(grep -A1 'pages pinned per query' $OUT/replay-$tag.txt | tail -1)"
    done
  done
done
