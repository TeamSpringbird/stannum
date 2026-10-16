# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# A devbox.sh --cmd-file: the x86-64 loader on real hardware
# (docs/RELEASING.md#x86-64-builds-and-the-loader). Installs PostgreSQL
# PG_MAJOR (18) from PGDG and cargo-pgrx, builds the loader with both builds
# (script/install-release, with PGO=1 by default: each build with its
# committed profile, script/pgo-check's tally per build in install.log),
# then checks, as an unprivileged user (the server refuses root):
#
# - postgres/tests/test_loader.py: the real CPU's choice against /proc/cpuinfo;
# - postgres/tests/loader_shim.py: which build runs under each loader setting
#   (auto, forced baseline, AVX-512 hidden, forced v4, unknown, not preloaded),
#   the maintenance launcher through the loader, and the same answers;
# - the conformance suite under the build the CPU picks and under the baseline;
# - KERNELS=1 (default): the engine and segment tests of an x86-64-v4 build
#   (as install-release builds it, through script/rustc-cpu-level) at each
#   forced kernel level (scalar, avx2, avx512).
#
#   devbox.sh --branch B --type c7i.2xlarge --cmd-file benchmarks/aws/devbox-loader.sh
#
# Results in $OUT/loader; progress after every step.
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
PG_MAJOR=${PG_MAJOR:-18} PGO=${PGO:-1} KERNELS=${KERNELS:-1}
L=$OUT/loader
mkdir -p "$L"
cd "$SRC1" || exit 1
BIN=/usr/lib/postgresql/$PG_MAJOR/bin
LIBDIR=/usr/lib/postgresql/$PG_MAJOR/lib

# --- PostgreSQL, cargo-pgrx ----------------------------------------------------
{
  apt-get -qq install -y postgresql-common &&
    /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y &&
    apt-get -qq install -y --no-install-recommends "postgresql-$PG_MAJOR" "postgresql-server-dev-$PG_MAJOR" \
      libclang-dev libssl-dev python3-psycopg python3-yaml
} > "$L/apt.log" 2>&1 || { step "loader: apt failed"; save; exit 1; }
systemctl stop postgresql > /dev/null 2>&1 || true
cargo install cargo-pgrx --version 0.19.1 --locked > "$L/cargo-pgrx.log" 2>&1 &&
  cargo pgrx init "--pg$PG_MAJOR=$BIN/pg_config" >> "$L/cargo-pgrx.log" 2>&1 ||
  { step "loader: cargo-pgrx failed"; save; exit 1; }
step "loader: PostgreSQL $($BIN/postgres --version | awk '{print $3}'), cargo-pgrx ready"

# --- the loader and both builds --------------------------------------------------
pgo=; [ "$PGO" = 1 ] && pgo=1
CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none \
  script/install-release --pg-config "$BIN/pg_config" --pgo "$pgo" > "$L/install.log" 2>&1 ||
  { step "loader: install-release failed: $(tail -n 3 "$L/install.log" | tr '\n' ' ')"; save; exit 1; }
ls -la "$LIBDIR"/stannum* > "$L/libs.txt"
(cd "$LIBDIR" && sha256sum stannum*.so) >> "$L/libs.txt"
for level in x86-64 x86-64-v4; do
  log=target/install-release/pg$PG_MAJOR/build-$level.log
  cp "$log" "$L/build-$level.log"
  echo "$level: $(grep -c 'hash mismatch' "$log") functions whose profile did not apply (hash mismatch)"
done > "$L/pgo-check.txt"
grep -E '^(== |PGO profile|  changed|  gone)' "$L/install.log" >> "$L/pgo-check.txt"
step "loader: installed: $(grep -c . "$L/libs.txt") lines; $(grep '^== loader' "$L/install.log"); $(grep 'PGO profile' "$L/install.log" | tr '\n' ' ')"
save

# --- the checks, as an unprivileged user -----------------------------------------
id pgtest > /dev/null 2>&1 || useradd -m pgtest
d=$SRC1; while [ "$d" != / ]; do chmod a+rx "$d"; d=$(dirname "$d"); done
chmod -R a+rX "$SRC1"
as_pg() { runuser -u pgtest -- env HOME=/home/pgtest PATH="$BIN:/usr/local/bin:/usr/bin:/bin" "$@"; }
cc -std=c11 -O1 -o /work/cpu_probe postgres/loader/cpu_probe.c && /work/cpu_probe choose --real > "$L/cpu.txt" 2>&1
grep -o -w -E 'avx512[a-z0-9_]+' /proc/cpuinfo | sort -u | tr '\n' ' ' >> "$L/cpu.txt"
python3 postgres/tests/test_loader.py -v > "$L/test_loader.txt" 2>&1
step "loader: test_loader exit $? ($(tail -n 1 "$L/test_loader.txt")); cpu $(tr '\n' ' ' < "$L/cpu.txt" | cut -c1-60)"
(cd /tmp && as_pg python3 "$SRC1/postgres/tests/loader_shim.py") > "$L/loader_shim.txt" 2>&1
step "loader: loader_shim exit $? $(grep -o '"status": "[a-z]*"' "$L/loader_shim.txt") $(grep -o '"cases": {[^}]*}' "$L/loader_shim.txt")"
save
as_pg env STANNUM_PYTHON=python3 script/test-all conformance > "$L/conformance-auto.txt" 2>&1
step "loader: conformance (auto) exit $? $(grep -E '^== test-all' "$L/conformance-auto.txt")"
as_pg env STANNUM_PYTHON=python3 STANNUM_LOADER_LEVEL=x86-64 script/test-all conformance > "$L/conformance-baseline.txt" 2>&1
step "loader: conformance (baseline) exit $? $(grep -E '^== test-all' "$L/conformance-baseline.txt")"
save

# --- the kernels in an x86-64-v4 build ---------------------------------------------
if [ "$KERNELS" = 1 ]; then
  for l in scalar avx2 avx512; do
    RUSTC_WORKSPACE_WRAPPER=$SRC1/script/rustc-cpu-level STANNUM_RUSTC_TARGET_CPU=x86-64-v4 \
      STANNUM_KERNELS=$l STANNUM_EXPECT_KERNELS=$l \
      cargo test --release -p engine -p segment --target-dir /work/t-kernels-v4 > "$L/kernels-v4-$l.txt" 2>&1
    step "loader: v4 build, kernels $l: exit $? $(grep -E '^test result' "$L/kernels-v4-$l.txt" | awk '{p += $4; f += $6} END {print p " passed, " f " failed"}')"
  done
  save
fi
step "LOADER-DONE"
