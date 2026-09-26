#!/bin/zsh
# Copyright (C) 2026 Ben Weis <ben@springbird.app>
#
# See LICENSE in the repository root for license terms.

# Correctness gates on the release build: install, lifecycle, upgrade check,
# Lead reference oracle, ranked-scan fuzz smoke. Logs go under $STANNUM_GATES (default /tmp/stannum-gates).
# Serialise with other pgrx work: python3 /tmp/stannum-pgrx-lock.py -- zsh benchmarks/local/gates.sh
export PATH=$HOME/.rustup/toolchains/1.96.0-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/opt/postgresql@18/bin:/opt/homebrew/bin:$PATH
cd "$(dirname "$0")/../.."
L=${STANNUM_GATES:-/tmp/stannum-gates}; mkdir -p $L
echo "== install release"; cargo pgrx install --release -p stannum --pg-config $(which pg_config) > $L/install.log 2>&1; echo "install exit $?"
cargo pgrx start pg18 > $L/start.log 2>&1
echo "== lifecycle"; python3 postgres/tests/postings_lifecycle.py > $L/lifecycle.log 2>&1; echo "lifecycle exit $?"; tail -3 $L/lifecycle.log
echo "== extension upgrade"; python3 postgres/tests/extension_upgrade.py > $L/upgrade.log 2>&1; echo "upgrade exit $?"; tail -2 $L/upgrade.log
echo "== reference oracle"; PGHOST=localhost PGPORT=28818 LEAD_REF_DIR=${LEAD_REF_DIR:-/tmp/stannum-lead-ref} script/reference-oracle $L/oracle > $L/oracle.log 2>&1; echo "oracle exit $?"; tail -4 $L/oracle.log
echo "== ranked fuzz smoke"; python3 postgres/tests/ranked_fuzz.py --smoke > $L/fuzz-smoke.log 2>&1; echo "fuzz smoke exit $?"; grep -o '"status": "[a-z]*"' $L/fuzz-smoke.log | sort | uniq -c
echo GATES-DONE
