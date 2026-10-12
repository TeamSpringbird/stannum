# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay A/B of two stacked format changes.
# SRC1 (base) replays the dump in S3 (page layout version 6); SRC2 re-encodes
# it on the box with its own `tnsreencode` (6 to 8) and replays that; SRC3
# re-encodes SRC2's output with its own (8 to 9, GRID_DENSITY, default 64)
# and replays that. Interleaved (A B C for score, C B A for full_score), one
# round, three styles, under perf stat: about 80 minutes of box time, and
# three dumps on the volume (--disk 150).
#   devbox.sh --branch BASE --branch V8 --branch V9 --dump --disk 150 \
#     --cmd-file benchmarks/aws/devbox-pgrid-ab.sh [--env GRID_DENSITY=42]
set -u
D=$DATA/dump150m
D2=/data/dump150m-v8
D3=/data/dump150m-v9
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
for i in 1 2 3; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay"; [ $i -gt 1 ] && bins="$bins --bin tnsreencode"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  echo "built SRC$i $(cat $src/.devbox-commit)"
done
$SRC2/target/release/tnsreencode --in $D --out $D2 > $OUT/reencode-v8.txt 2> $OUT/reencode-v8.err || { echo "re-encode 6 to 8 failed"; tail -5 $OUT/reencode-v8.err; exit 1; }
echo "re-encoded to 8: $(tail -1 $OUT/reencode-v8.txt | cut -c1-160)"
$SRC3/target/release/tnsreencode --in $D2 --out $D3 --from 8 --grid-density ${GRID_DENSITY:-64} --jobs 4 \
  > $OUT/reencode-v9.txt 2> $OUT/reencode-v9.err || { echo "re-encode 8 to 9 failed"; tail -5 $OUT/reencode-v9.err; exit 1; }
echo "re-encoded to 9: $(grep '^areas together' $OUT/reencode-v9.txt)"; df -h /data | tail -1
A=$SRC1/target/release/tnsreplay; B=$SRC2/target/release/tnsreplay; C=$SRC3/target/release/tnsreplay
dump_of() { case $1 in A) echo $D ;; B) echo $D2 ;; C) echo $D3 ;; esac; }
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in A B C; do bin=$(eval echo \$$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $(dump_of $x) --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for r in 1; do
  for mode in score full; do
    for st in conjunction disjunction phrase; do
      order="A B C"; [ $mode = full ] && order="C B A"
      extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=150; }
      for x in $order; do bin=$(eval echo \$$x)
        tag=r$r-$x-$mode-$st
        perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $(dump_of $x) --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
          --style $st --limit $limit --repeat 3 $extra > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
        echo "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt) | $(grep -A1 'pages pinned per query' $OUT/replay-$tag.txt | tail -1)"
      done
    done
  done
done
