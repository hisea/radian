#!/bin/bash
# Terminal-Bench 2.0: radian vs pi 1.4.0 vs opencode 1.19.4, both on opencode-go/deepseek-v4.1-flash, thinking high.
# Usage: bash benchmarks/terminal-bench/ab/run.sh <label> [extra harbor args, e.g. -i fix-git]
# Runs both harnesses at once (-n 4 each) so they share the same API conditions.
# Binary: $RADIAN_BENCH_BINARY (default $HOME/.cache/radian-musl/target/release/radian).
# The host keeps its binaries under $HOME/tb2/bin and writes jobs to $HOME/tb2/jobs/<label>.
# PYTHONPATH comes from this script's own checkout.
set -euo pipefail
label=${1:?label}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
BIN=${RADIAN_BENCH_BINARY:-$HOME/.cache/radian-musl/target/release/radian}
SHA=$(cut -d' ' -f1 "$BIN.sha256")
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY" OPENCODE_API_KEY="$KEY"
unset KEY
out=$HOME/tb2/jobs/$label
mkdir -p "$out/radian" "$out/pi" "$out/opencode"
M=opencode-go/deepseek-v4.1-flash
# A failed harbor run still writes its done line: set -e would end the subshell before the echo.
(
  PYTHONPATH=$PP harbor run -d terminal-bench@2.0 \
    -a radian_agent:RadianAgent -m $M \
    --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high \
    -n 4 -o "$out/radian" -y "$@" > "$out/radian.log" 2>&1 && rc=0 || rc=$?
  echo "radian rc=$rc" >> "$out/done"
) &
(
  PYTHONPATH=$PP harbor run -d terminal-bench@2.0 \
    -a pi_opencode_go:PiOpencodeGo -m $M \
    --ak version=1.4.0 --ak thinking=high \
    -n 4 -o "$out/pi" -y "$@" > "$out/pi.log" 2>&1 && rc=0 || rc=$?
  echo "pi rc=$rc" >> "$out/done"
) &
(
  PYTHONPATH=$PP harbor run -d terminal-bench@2.0 \
    -a opencode_go:OpenCodeGo -m $M \
    --ak version=1.19.4 --ak variant=high --ak disable_web_search=true \
    -n 4 -o "$out/opencode" -y "$@" > "$out/opencode.log" 2>&1 && rc=0 || rc=$?
  echo "opencode rc=$rc" >> "$out/done"
) &
wait
echo "all done: $out"
