#!/bin/bash
# Terminal-Bench A/B: radian variants vs pi 1.4.0 vs opencode 1.19.4, same model, thinking high, same run.
# Usage: bash benchmarks/terminal-bench/ab/ab.sh <label> <radian-bin-name>...
#   (names under $HOME/tb2/bin, e.g. radian-<sha>; the host keeps its binaries and jobs there)
# 10 tasks x 2 attempts each (-k 2), -n ${AB_N:-2} per agent. loadavg is sampled every 30 s into
# $HOME/tb2/jobs/<label>/loadavg.log. Output: $HOME/tb2/jobs/<label>/<agent>/, one line per finished
# agent in $HOME/tb2/jobs/<label>/done. PYTHONPATH comes from this script's own checkout.
set -euo pipefail
label=${1:?label}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY" OPENCODE_API_KEY="$KEY"; unset KEY
out=$HOME/tb2/jobs/$label; mkdir -p "$out"
M=opencode-go/deepseek-v4.1-flash
TASKS=(llm-inference-batching-scheduler largest-eigenval write-compressor reshard-c4-data torch-tensor-parallelism
       password-recovery break-filter-js-from-html portfolio-optimization gpt2-codegolf merge-diff-arc-agi-task)
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
COMMON=(-d terminal-bench@2.0 -m $M -n "${AB_N:-2}" -k 2 -y "${I[@]}")

( while true; do echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $(cut -d' ' -f1-3 /proc/loadavg)" >> "$out/loadavg.log"; sleep 30; done ) &
sampler=$!
# A failed harbor run still writes its done line: set -e would end the subshell before the echo.
pids=()
for b in "$@"; do
  BIN=$HOME/tb2/bin/$b; SHA=$(cut -d' ' -f1 "$BIN.sha256")
  ( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a radian_agent:RadianAgent \
      --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high -o "$out/$b" > "$out/$b.log" 2>&1 && rc=0 || rc=$?
    echo "$b rc=$rc" >> "$out/done" ) &
  pids+=($!)
done
( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a pi_opencode_go:PiOpencodeGo --ak version=1.4.0 --ak thinking=high \
    -o "$out/pi" > "$out/pi.log" 2>&1 && rc=0 || rc=$?; echo "pi rc=$rc" >> "$out/done" ) &
pids+=($!)
( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a opencode_go:OpenCodeGo --ak version=1.19.4 --ak variant=high \
    --ak disable_web_search=true -o "$out/opencode" > "$out/opencode.log" 2>&1 && rc=0 || rc=$?; echo "opencode rc=$rc" >> "$out/done" ) &
pids+=($!)
for p in "${pids[@]}"; do wait "$p" || true; done
kill "$sampler"
echo "all done: $out"
