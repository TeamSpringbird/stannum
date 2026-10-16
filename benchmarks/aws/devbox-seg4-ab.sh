# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: what the segment count costs a query on x86. Arm A
# is the 150M replay dump as the build left it (14 segments, page layout
# 15), arm B the same dump merged into 4 on the box by tnsmerge --stream
# (the extension's streaming merge), checksummed against the M4's merge.
# One tnsreplay binary replays every style, score and full, in 3
# alternating rounds (A B, B A, A B), checked against the ranked oracle; it
# counts the timed passes' instructions and cycles itself. With a second
# source carrying `tnsreplay --cpus` (loop/htgate), HTPAIR=1 then replays
# both dumps hyperthread-paired (see devbox-htpair-ab.sh): CPU C alone,
# then C and its sibling at once.
#   DEVBOX_DUMP_PREFIX=replay-dumps/seg4/ devbox.sh --branch loop/seg4 [--branch loop/htgate] --dump \
#     --type m7i.4xlarge --disk 160 --spot-only [--env HTPAIR=1] --cmd-file benchmarks/aws/devbox-seg4-ab.sh
# Progress and results so far go up after every step.
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
D14=$DATA/dump150m
D4=/work/dump-seg4
PLAN=${PLAN:-conjunction:score:400,phrase:score:400,disjunction:score:200,conjunction:full:200,phrase:full:200,disjunction:full:100}
HTPAIR=${HTPAIR:-0} CPU=${CPU:-2}
(cd "$SRC1" && cargo build --release -p bench --bin tnsreplay --bin tnsmerge) > "$OUT/build.log" 2>&1 ||
  { step "build failed: $(grep -m1 -E '^error' "$OUT/build.log")"; save; exit 1; }
step "built $(cat "$SRC1/.devbox-commit")"
(cd "$D14" && sha256sum ./*.segment | sort -k2) > "$OUT/dump150m.sha256"
/usr/bin/time -v "$SRC1/target/release/tnsmerge" --in "$D14" --out "$D4" --segments 4 --jobs 4 --stream \
  > "$OUT/merge4.txt" 2> "$OUT/merge4.err"
st=$?
step "merge exit $st $(grep -h -E 'Maximum resident|Elapsed' "$OUT/merge4.err" | tr -s ' \t' ' ' | tr '\n' ';')"
(cd "$D4" && sha256sum ./*.segment | sort -k2) > "$OUT/dump-seg4.sha256"
save
[ $st = 0 ] || exit 1
BIN=$SRC1/target/release/tnsreplay
for r in 1 2 3; do
  order="A B"; [ $r = 2 ] && order="B A"
  for x in $order; do
    dump=$D14; [ $x = B ] && dump=$D4
    "$BIN" --dump "$dump" --trace "$DATA/trace.tsv" --expect "$DATA/pg-ranked.tsv" --repeat 3 --plan "$PLAN" \
      > "$OUT/replay-r$r-$x.txt" 2> "$OUT/replay-r$r-$x.err"
    step "r$r $x exit $? $(grep -E '^expect' "$OUT/replay-r$r-$x.txt" | head -1)"
    save
  done
done
if [ "$HTPAIR" = 1 ] && [ -n "${SRC2:-}" ]; then
  sib() { tr ',' '\n' < /sys/devices/system/cpu/cpu$1/topology/thread_siblings_list | grep -vx "$1" | head -1; }
  S=$(sib $CPU)
  (cd "$SRC2" && cargo build --release -p bench --bin tnsreplay --target-dir /work/t-ht) > "$OUT/build-ht.log" 2>&1 ||
    { step "htpair build failed"; save; exit 1; }
  step "htpair: $(cat "$SRC2/.devbox-commit") CPU $CPU + sibling $S"
  HPLAN=conjunction:score:160,phrase:score:160,disjunction:score:80,conjunction:full:80,phrase:full:80,disjunction:full:40
  for r in 1 2 3; do
    order="A B"; [ $r = 2 ] && order="B A"
    for x in $order; do
      dump=$D14; [ $x = B ] && dump=$D4
      /work/t-ht/release/tnsreplay --dump "$dump" --trace "$DATA/trace.tsv" --plan $HPLAN --repeat 2 --forget \
        --cpus "$CPU/$CPU,$S" > "$OUT/pinned-r$r-$x.txt" 2> "$OUT/pinned-r$r-$x.err"
      step "pinned r$r $x exit $?"
      save
    done
  done
fi
step "ALL-DONE"
save
