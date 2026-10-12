# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# The devbox's side of benchmarks/aws/devbox.sh, run as root through SSM.
# devbox.sh prepends the run's variables:
#   RUN BUCKET PREFIX SOURCES ("name=sha ...") DUMP (0/1) DUMP_PREFIX
#   ENVS (base64 of KEY=VALUE lines) CMD (base64)
# It installs the toolchain and perf, unpacks the uploaded sources (git
# archives: no credentials on the box), optionally syncs the replay dump,
# runs CMD with OUT, DATA and SRC1.. set, and uploads $OUT as results.tar.gz.
# Progress lines go to s3://$BUCKET/$PREFIX/progress.txt as they happen.

set -uo pipefail
export HOME=/root DEBIAN_FRONTEND=noninteractive
W=/work
OUT=$W/out
mkdir -p "$OUT"
exec > >(tee -a "$OUT/box.log") 2>&1
S3="s3://$BUCKET/$PREFIX"
AWS=/usr/local/bin/aws

phase() {
  echo "== $(date -u +%H:%M:%S) $*"
  echo "$(date -u +%H:%M:%S) $*" >> "$W/progress.txt"
  [ -x "$AWS" ] && "$AWS" s3 cp --only-show-errors "$W/progress.txt" "$S3/progress.txt" || true
}

finish() {
  local status=$1
  echo "$status" > "$OUT/exit-status"
  phase "finished: cmd exit $status"
  tar -C "$W" -czf "$W/results.tar.gz" out
  "$AWS" s3 cp --only-show-errors "$W/results.tar.gz" "$S3/results.tar.gz"
  echo "$status" | "$AWS" s3 cp --only-show-errors - "$S3/DONE"
  exit 0
}

phase "box up: $(nproc) vCPU, $(free -g | awk '/Mem:/{print $2}') GiB"
for i in 1 2 3 4 5; do
  apt-get -qq update && apt-get -qq install -y build-essential pkg-config unzip curl \
    linux-tools-common "linux-tools-$(uname -r)" > "$OUT/apt.log" 2>&1 && break
  sleep 10
done
curl -fsSL https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip -o /tmp/awscli.zip &&
  unzip -q -o /tmp/awscli.zip -d /tmp && /tmp/aws/install --update > /dev/null || { echo "aws cli install failed"; exit 1; }
"$AWS" configure set default.s3.max_concurrent_requests 32
phase "packages installed"

sysctl -q -w kernel.perf_event_paranoid=-1 kernel.kptr_restrict=0
{
  lscpu
  grep -m1 '^flags' /proc/cpuinfo
  uname -a
  perf --version
  perf stat -e instructions,cycles,branch-misses,L1-icache-load-misses true 2>&1
} > "$OUT/machine.txt" 2>&1

n=0
for pair in $SOURCES; do
  n=$((n + 1)); name=${pair%%=*}; sha=${pair#*=}
  mkdir -p "$W/$name"
  "$AWS" s3 cp --only-show-errors "$S3/src-$name.tar.gz" - | tar -xz -C "$W/$name" ||
    { phase "source $name download failed"; finish 98; }
  echo "$sha" > "$W/$name/.devbox-commit"
  export "SRC$n=$W/$name"
  echo "SRC$n=$W/$name ($sha)" >> "$OUT/sources.txt"
done
phase "sources unpacked: $SOURCES"

channel=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$SRC1/rust-toolchain.toml")
curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none > /dev/null 2>&1
. "$HOME/.cargo/env"
rustup toolchain install "${channel:-stable}" --profile minimal > /dev/null 2>&1 ||
  { phase "rustup failed"; finish 97; }
phase "rust $(cd "$SRC1" && rustc --version)"

if [ "$DUMP" = 1 ]; then
  mkdir -p /data
  "$AWS" s3 sync --only-show-errors "s3://$BUCKET/$DUMP_PREFIX" /data ||
    { phase "dump sync failed"; finish 96; }
  phase "dump synced: $(du -sh /data | cut -f1)"
fi
export DATA=/data OUT

echo "$ENVS" | base64 -d > "$W/env.sh"
set -a; . "$W/env.sh"; set +a
echo "$CMD" | base64 -d > "$W/cmd.sh"
cp "$W/cmd.sh" "$OUT/cmd.sh"
phase "running cmd"
(cd "$SRC1" && bash -o pipefail "$W/cmd.sh") > "$OUT/cmd.log" 2>&1
finish $?
