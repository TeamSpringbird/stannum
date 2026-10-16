# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: what the segment count costs a query on x86. Arm A
# is the 150M replay dump as the build left it (14 segments, page layout
# 15), arm B the same dump merged into 4 on the box by tnsmerge --stream
# (the extension's streaming merge), checksummed against the M4's merge.
# One tnsreplay binary replays every style, score and full, in 3
# alternating rounds (A B, B A, A B); it counts the timed passes'
# instructions and cycles itself.
#   DEVBOX_DUMP_PREFIX=replay-dumps/seg4/ devbox.sh --branch loop/seg4 --dump \
#     --type m7i.4xlarge --disk 160 --spot-only --cmd-file benchmarks/aws/devbox-seg4-ab.sh
# Summarize with absum.py OUT/replay- (see the lane's scripts).
set -u
D14=$DATA/dump150m
D4=/work/dump-seg4
PLAN=conjunction:score:400,phrase:score:400,disjunction:score:200,conjunction:full:200,phrase:full:200,disjunction:full:100
(cd "$SRC1" && cargo build --release -p bench --bin tnsreplay --bin tnsmerge) > "$OUT/build.log" 2>&1 ||
  { echo "build failed"; tail -30 "$OUT/build.log"; exit 1; }
echo "built $(cat "$SRC1/.devbox-commit")"
(cd "$D14" && sha256sum ./*.segment | sort -k2) > "$OUT/dump150m.sha256"
/usr/bin/time -v "$SRC1/target/release/tnsmerge" --in "$D14" --out "$D4" --segments 4 --jobs 4 --stream \
  > "$OUT/merge4.txt" 2> "$OUT/merge4.err"
echo "merge exit $?"
grep -h -E "<-|Maximum resident|Elapsed" "$OUT/merge4.txt" "$OUT/merge4.err"
(cd "$D4" && sha256sum ./*.segment | sort -k2) > "$OUT/dump-seg4.sha256"
cat "$OUT/dump-seg4.sha256"
BIN=$SRC1/target/release/tnsreplay
for r in 1 2 3; do
  order="A B"; [ $r = 2 ] && order="B A"
  for x in $order; do
    dump=$D14; [ $x = B ] && dump=$D4
    "$BIN" --dump "$dump" --trace "$DATA/trace.tsv" --expect "$DATA/pg-ranked.tsv" --repeat 3 --plan "$PLAN" \
      > "$OUT/replay-r$r-$x.txt" 2> "$OUT/replay-r$r-$x.err"
    echo "r$r $x exit $? $(grep -E '^expect' "$OUT/replay-r$r-$x.txt" | head -1)"
  done
done
