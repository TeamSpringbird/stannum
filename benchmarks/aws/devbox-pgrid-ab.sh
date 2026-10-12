# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the 150M replay A/B of two stacked format changes.
# SRC1 (base) replays the dump in S3 (page layout version 6); SRC2 re-encodes
# it on the box with its own `tnsreencode` (6 to 8) and replays that; SRC3
# re-encodes SRC2's output with its own (8 to 9) once per grid density in
# DENSITIES (default "64") and replays each. Interleaved (A B C.. for score,
# the reverse for full_score), one round, three styles, under perf stat, with
# progress and results so far uploaded after each step (a spot reclaim or
# the hard limit keeps what was done). Box time: about 55 minutes plus 25
# per density; the volume holds every dump (--disk 130, 170 for two
# densities).
#   devbox.sh --branch BASE --branch V8 --branch V9 --dump --disk 170 --hours 3 \
#     --cmd-file benchmarks/aws/devbox-pgrid-ab.sh --env DENSITIES="64 42"
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
DENSITIES=${DENSITIES:-64}
D=$DATA/dump150m
D2=/data/dump150m-v8
EV=instructions:u,cycles:u,branch-misses:u,L1-icache-load-misses:u,task-clock
for i in 1 2 3; do
  src=$(eval echo \$SRC$i); bins="--bin tnsreplay"; [ $i -gt 1 ] && bins="$bins --bin tnsreencode"
  (cd $src && cargo build --release -p bench $bins) > $OUT/build-$i.log 2>&1 || { echo "build $i failed"; tail -30 $OUT/build-$i.log; exit 1; }
  step "built SRC$i $(cat $src/.devbox-commit)"
done
$SRC2/target/release/tnsreencode --in $D --out $D2 > $OUT/reencode-v8.txt 2> $OUT/reencode-v8.err || { echo "re-encode 6 to 8 failed"; tail -5 $OUT/reencode-v8.err; exit 1; }
step "re-encoded to 8: $(tail -1 $OUT/reencode-v8.txt | cut -c1-160)"
# The variants: A base, B version 8, then one per density (C, E, F, ...).
names="A B"
dump_A=$D; dump_B=$D2; bin_A=$SRC1/target/release/tnsreplay; bin_B=$SRC2/target/release/tnsreplay
letters="C E F G"
for d in $DENSITIES; do
  x=${letters%% *}; letters=${letters#* }
  out=/data/dump150m-v9-d$d
  $SRC3/target/release/tnsreencode --in $D2 --out $out --from 8 --grid-density $d --jobs 4 \
    > $OUT/reencode-v9-d$d.txt 2> $OUT/reencode-v9-d$d.err || { echo "re-encode 8 to 9 failed"; tail -5 $OUT/reencode-v9-d$d.err; exit 1; }
  step "re-encoded to 9 at density $d ($x): $(grep '^areas together' $OUT/reencode-v9-d$d.txt)"
  eval "dump_$x=$out; bin_$x=$SRC3/target/release/tnsreplay"
  names="$names $x"
done
df -h /data | tail -1; save
reversed=$(echo $names | tr ' ' '\n' | tac | tr '\n' ' ')
# load-only baselines: one query, one repeat (setup cost inside the perf totals)
for x in $names; do bin=$(eval echo \$bin_$x); dump=$(eval echo \$dump_$x)
  perf stat -x, -e $EV -o $OUT/perf-load-$x.csv -- $bin --dump $dump --trace $DATA/trace.tsv --style conjunction --limit 1 --repeat 1 > $OUT/replay-load-$x.txt 2> $OUT/replay-load-$x.err
done
for r in 1; do
  for mode in score full; do
    for st in conjunction disjunction phrase; do
      order=$names; [ $mode = full ] && order=$reversed
      extra=""; limit=300; [ $mode = full ] && { extra="--full"; limit=150; }
      for x in $order; do bin=$(eval echo \$bin_$x); dump=$(eval echo \$dump_$x)
        tag=r$r-$x-$mode-$st
        perf stat -x, -e $EV -o $OUT/perf-$tag.csv -- $bin --dump $dump --trace $DATA/trace.tsv --expect $DATA/pg-ranked.tsv \
          --style $st --limit $limit --repeat 3 $extra > $OUT/replay-$tag.txt 2> $OUT/replay-$tag.err
        step "$tag exit $? $(grep -E "^$st " $OUT/replay-$tag.txt | head -1) | $(grep expect: $OUT/replay-$tag.txt) | $(grep -A1 'pages pinned per query' $OUT/replay-$tag.txt | tail -1)"
      done
      save
    done
  done
done
