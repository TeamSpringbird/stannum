# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the hyperthread-paired replay A/B
# (docs/benchmarks.md#the-hyperthread-paired-replay). A server's backends
# share each core with a sibling, so a build is gated on what one core
# finishes with both of its hyperthreads busy, beside the single-thread
# cycles. Every arm is one tnsreplay process per round over one load,
# `--cpus C/C,S`: the plan's slices on CPU C alone, then on C and its
# sibling S at once (`--cpus` in bench/src/bin/tnsreplay.rs). Rounds
# rotate the arms' order (reversed every other round), arms over the same
# dump kept adjacent; progress and results so far go up after every step.
#
#   devbox.sh --branch BASE --branch CANDIDATE --dump --type m7i.4xlarge --spot-only \
#     --env ARMS=A:1:dump150m,B:2:dump150m --env COMPARE=B:A --cmd-file benchmarks/aws/devbox-htpair-ab.sh
#
# ARMS: comma-separated NAME:SRC:DUMP[:v3|v4], the tree (SRC1, SRC2, ...),
# the dump directory under $DATA and an optional x86-64 level the whole
# build is compiled for (through CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS:
# a plain RUSTFLAGS would drop .cargo/config.toml's link flag). Each arm
# builds in its own target directory (its sha256 in shasums.txt). COMPARE:
# NAME:BASE pairs for the summary (benchmarks/aws/htpair_summary.py, run on
# the box into summary.txt). ROUNDS (3), PLAN, REPEAT (2), CPU (2: its
# core is measured), WIDE (1: the first arm's first round also runs four
# cores' eight hyperthreads).
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
T=$DATA/trace.tsv
ROUNDS=${ROUNDS:-3} REPEAT=${REPEAT:-2} CPU=${CPU:-2} WIDE=${WIDE:-1} COMPARE=${COMPARE:-}
PLAN=${PLAN:-conjunction:score:160,phrase:score:160,disjunction:score:80,conjunction:full:80,phrase:full:80,disjunction:full:40}
FV=CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
LINK="-C link-arg=-Wl,--unresolved-symbols=ignore-all"

# --- topology: the measured core's two hyperthreads ---------------------------
sib() { tr ',' '\n' < /sys/devices/system/cpu/cpu$1/topology/thread_siblings_list | grep -vx "$1" | head -1; }
S=$(sib $CPU)
[ -n "$S" ] || { step "CPU $CPU has no sibling: $(cat /sys/devices/system/cpu/cpu$CPU/topology/thread_siblings_list)"; exit 1; }
lscpu -e > $OUT/lscpu-e.txt
grep . /sys/devices/system/cpu/cpu*/topology/thread_siblings_list > $OUT/siblings.txt
WIDE_SET=$(for c in $CPU $((CPU + 1)) $((CPU + 2)) $((CPU + 3)); do echo -n "$c,$(sib $c),"; done)
SETS="$CPU/$CPU,$S"
step "core: CPU $CPU + sibling $S; wide set ${WIDE_SET%,}"

# --- builds -------------------------------------------------------------------
IFS=, read -r -a arms <<< "$ARMS"
names=()
for spec in "${arms[@]}"; do
  IFS=: read -r name n dump level <<< "$spec"
  src=$(eval echo \$SRC$n)
  [ -d "$DATA/$dump" ] || { step "arm $name: no dump $DATA/$dump"; exit 1; }
  # LEVEL "v4all": the whole build, dependencies too, through the per-target flags.
  all=; case "${level:-}" in *all) level=${level%all}; all=1 ;; esac
  flags=$LINK; [ -n "${level:-}" ] && flags="-C target-cpu=x86-64-$level $LINK"
  if [ -n "${level:-}" ] && [ -z "$all" ] && [ -x $src/script/rustc-cpu-level ]; then
    # As script/install-release builds a level: the workspace's crates only,
    # through RUSTC_WRAPPER (not hashed into -C metadata, so names match the baseline's).
    (cd $src && RUSTC_WRAPPER=$src/script/rustc-cpu-level STANNUM_RUSTC_ROOT=$(cd $src && pwd -P) \
      STANNUM_RUSTC_TARGET_CPU=x86-64-$level \
      cargo build --release -p bench --bin tnsreplay --target-dir /work/t-$name) > $OUT/build-$name.log 2>&1
  elif [ -n "${level:-}" ]; then
    (cd $src && env "$FV=$flags" cargo build --release -p bench --bin tnsreplay --target-dir /work/t-$name) > $OUT/build-$name.log 2>&1
  else
    (cd $src && cargo build --release -p bench --bin tnsreplay --target-dir /work/t-$name) > $OUT/build-$name.log 2>&1
  fi || { step "build $name failed: $(grep -m1 -E '^error' $OUT/build-$name.log)"; save; continue; }
  cp /work/t-$name/release/tnsreplay /work/tnsreplay-$name
  (cd /work && sha256sum tnsreplay-$name) >> $OUT/shasums.txt
  echo "$name $dump $(cat $src/.devbox-commit) ${level:-base}${all:+-all}" >> $OUT/arms.txt
  names+=("$name")
  step "built $name $(cat $src/.devbox-commit) ${level:-} -> $dump"
done
save

# --- rounds -------------------------------------------------------------------
dump_of() { awk -v n="$1" '$1 == n {print $2}' $OUT/arms.txt; }
dumps=$(awk '{print $2}' $OUT/arms.txt | awk '!seen[$0]++')
for r in $(seq 1 $ROUNDS); do
  order=()
  for d in $dumps; do
    group=($(for x in "${names[@]}"; do [ "$(dump_of $x)" = "$d" ] && echo $x; done))
    k=$(( (r - 1) % ${#group[@]} ))
    group=("${group[@]:$k}" "${group[@]:0:$k}")
    order+=("${group[@]}")
  done
  [ $((r % 2)) = 0 ] && order=($(printf '%s\n' "${order[@]}" | tac))
  for x in "${order[@]}"; do
    sets=$SETS
    [ "$WIDE" = 1 ] && [ $r = 1 ] && [ "$x" = "${names[0]}" ] && sets="$SETS/${WIDE_SET%,}"
    tag=r$r-$x
    /work/tnsreplay-$x --dump $DATA/$(dump_of $x) --trace $T --plan $PLAN --repeat $REPEAT --forget --cpus $sets \
      > $OUT/pinned-$tag.txt 2> $OUT/pinned-$tag.err
    st=$?
    step "$tag exit $st q/Gcyc $(awk '$1 == "pinned" {for (i = 1; i < NF; i++) if ($i == "q/Gcyc") printf "%s/%s %s ", $2, substr($3, 1, 6), $(i + 1)}' $OUT/pinned-$tag.txt)"
    save
  done
done
python3 $SRC1/benchmarks/aws/htpair_summary.py $OUT --compare "$COMPARE" > $OUT/summary.txt 2>&1
step "summary: $(wc -l < $OUT/summary.txt) lines"
step "ALL-DONE"
save
