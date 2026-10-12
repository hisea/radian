#!/usr/bin/env bash
# Builds and runs the whole spike, writing every result under $ROOT/data.
set -uo pipefail
[ "$(uname -s)" = Linux ] || { echo "REFUSING: not Linux"; exit 1; }
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="${ROOT:-/tmp/radian-linux-sandbox}"
cd "$ROOT"
mkdir -p data bin
echo "== environment" | tee data/environment.txt
{ uname -s -r -m; echo "lsm: $(cat /sys/kernel/security/lsm 2>/dev/null)"; bwrap --version; gcc --version | head -1; python3 --version; git --version; } | tee -a data/environment.txt

echo "== build"
gcc -O2 -o bin/llsandbox src/llsandbox.c
gcc -O2 -o bin/ll_probe src/ll_probe.c
echo "built bin/llsandbox bin/ll_probe" | tee data/build.txt
bin/ll_probe | tee data/landlock-probe.txt

run_matrix() { # run_matrix <wrapper> <sandbox on|off> <label> [EXPECT_GIT_RO] [EXPECT_ENV_CLEARED]
  local wrapper="$1" sandbox="$2" label="$3" gitro="${4:-on}" envclear="${5:-on}"
  "$HERE/setup.sh" >/dev/null
  WRAPPER="$wrapper" SANDBOX="$sandbox" EXPECT_GIT_RO="$gitro" EXPECT_ENV_CLEARED="$envclear" \
    "$HERE/matrix.sh" "$label" >"data/matrix-$label.txt" 2>&1
  local rc=$?
  grep -E 'MISMATCH|SUMMARY' "data/matrix-$label.txt"
  return $rc
}

echo "== bwrap"
run_matrix "$HERE/sbwrap.sh" on bwrap-sandbox on
run_matrix "$HERE/sbwrap.sh" off bwrap-control on
echo "== landlock"
run_matrix "$HERE/llwrap.sh" on landlock-sandbox off off
run_matrix "$HERE/llwrap.sh" off landlock-control off off

echo "== combo (bwrap outside + landlock inside)"
run_matrix "$HERE/combo.sh" on combo-sandbox on on
run_matrix "$HERE/combo.sh" off combo-control on on

echo "== mutations"
"$HERE/mutations.sh" | tee data/mutations.txt

echo "== bench"
"$HERE/bench.sh" | tee data/bench.txt

echo "== orphan check"
ps -eo pid,ppid,comm,args | grep -E 'radian-linux-sandbox|bwrap|llsandbox|mcp_echo|listener.py' | grep -v grep | tee data/orphans.txt
echo "(empty above means no orphans)"
