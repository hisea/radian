#!/bin/bash
# Terminal-Bench A/B: radian variants vs pi 1.4.0 vs opencode 1.19.4, same model, thinking high, same run.
# Usage: bash benchmarks/terminal-bench/ab/ab.sh <label> <radian-bin-name>...
#   (names under $HOME/tb2/bin, e.g. radian-<sha>; the host keeps its binaries and jobs there)
# The task set is AB_TASKS (default ab3; see README.md), -k ${AB_K:-2}, and
# -n ${AB_N:-2} per agent. The list the run uses is written to <label>/tasks.txt.
# AB_CAPTURE=1 records every arm's model requests through one mitmdump per arm
# (AB_CAPTURE_PORT, default 18080; see README.md); unset, the commands below are
# exactly what this script has always run.
# loadavg is sampled every 30 s into $HOME/tb2/jobs/<label>/loadavg.log. Output:
# $HOME/tb2/jobs/<label>/<agent>/, one line per finished agent in
# $HOME/tb2/jobs/<label>/done. PYTHONPATH comes from this script's own checkout.
# AB_TEST_SKIP_LOADAVG is test-only: tests/test_ab_scripts.py sets it so the
# sampler (which reads /proc/loadavg) does not start.
set -euo pipefail
label=${1:?label}; shift
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY" OPENCODE_API_KEY="$KEY"; unset KEY
TASK_LIST=$(python3 "$HERE/task_set.py" resolve "${AB_TASKS:-ab3}" "$HERE/tasks") || exit 1
TASKS=()
while IFS= read -r task; do TASKS+=("$task"); done <<< "$TASK_LIST"
out=$HOME/tb2/jobs/$label; mkdir -p "$out"
printf '%s\n' "${TASKS[@]}" > "$out/tasks.txt"
M=opencode-go/deepseek-v4.1-flash
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
COMMON=(-d terminal-bench@2.0 -m $M -n "${AB_N:-2}" -k "${AB_K:-2}" -y "${I[@]}")

# The request capture: one mitmdump per arm, in arm order, from AB_CAPTURE_PORT.
source "$HERE/capture.sh"
CAPTURE=${AB_CAPTURE:-}
CAPTURE_PORT=${AB_CAPTURE_PORT:-18080}
if [ -n "$CAPTURE" ]; then
  capture_require_mitmdump
  trap capture_stop_all EXIT
fi

sampler=
if [ -z "${AB_TEST_SKIP_LOADAVG:-}" ]; then
  ( while true; do echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $(cut -d' ' -f1-3 /proc/loadavg)" >> "$out/loadavg.log"; sleep 30; done ) &
  sampler=$!
fi
# A failed harbor run still writes its done line: set -e would end the subshell before the echo.
pids=()
index=0
for b in "$@"; do
  BIN=$HOME/tb2/bin/$b; SHA=$(cut -d' ' -f1 "$BIN.sha256")
  # AK stays empty without AB_CAPTURE: ${AK[@]+...} is the only expansion form
  # that is safe under `set -u` on bash 3.2 as well as bash 5.
  AK=()
  if [ -n "$CAPTURE" ]; then
    capture_start "$out" "$b" $((CAPTURE_PORT + index)) "$HERE/capture_addon.py"
    AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
  fi
  ( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a radian_agent:RadianAgent \
      --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high \
      ${AK[@]+"${AK[@]}"} -o "$out/$b" > "$out/$b.log" 2>&1 && rc=0 || rc=$?
    echo "$b rc=$rc" >> "$out/done" ) &
  pids+=($!)
  index=$((index + 1))
done
AK=()
if [ -n "$CAPTURE" ]; then
  capture_start "$out" pi $((CAPTURE_PORT + index)) "$HERE/capture_addon.py"
  AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
fi
( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a pi_opencode_go:PiOpencodeGo --ak version=1.4.0 --ak thinking=high \
    ${AK[@]+"${AK[@]}"} -o "$out/pi" > "$out/pi.log" 2>&1 && rc=0 || rc=$?; echo "pi rc=$rc" >> "$out/done" ) &
pids+=($!)
AK=()
if [ -n "$CAPTURE" ]; then
  capture_start "$out" opencode $((CAPTURE_PORT + index + 1)) "$HERE/capture_addon.py"
  AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
fi
( PYTHONPATH=$PP harbor run "${COMMON[@]}" -a opencode_go:OpenCodeGo --ak version=1.19.4 --ak variant=high \
    ${AK[@]+"${AK[@]}"} --ak disable_web_search=true -o "$out/opencode" > "$out/opencode.log" 2>&1 && rc=0 || rc=$?; echo "opencode rc=$rc" >> "$out/done" ) &
pids+=($!)
for p in "${pids[@]}"; do wait "$p" || true; done
if [ -n "$CAPTURE" ]; then capture_stop_all; fi
if [ -n "$sampler" ]; then kill "$sampler"; fi
echo "all done: $out"
