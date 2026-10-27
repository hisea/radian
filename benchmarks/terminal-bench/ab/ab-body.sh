#!/bin/bash
# Request-body A/B: one radian binary, no extra body vs `RADIAN_EXTRA_BODY` from a file, same
# run (ADR 0026 D13). Usage: bash benchmarks/terminal-bench/ab/ab-body.sh <label> <radian-bin-name> <body-file>
# The body file must hold one JSON object. Same task set (AB_TASKS, default ab3) and model as
# ab.sh; -k ${AB_K:-3}, -n ${AB_N:-3} per arm. The list the run uses is written to
# <label>/tasks.txt and the body file to <label>/body.json. AB_CAPTURE=1 captures both arms'
# model requests (AB_CAPTURE_PORT, default 18080; see README.md); unset, the commands below
# are exactly what this script has always run.
# Output: $HOME/tb2/jobs/<label>/{base,body}/ and $HOME/tb2/jobs/<label>/done;
# the binary is $HOME/tb2/bin/<name>. PYTHONPATH comes from this script's own checkout.
# AB_TEST_SKIP_LOADAVG is test-only: tests/test_ab_scripts.py sets it so the sampler (which
# reads /proc/loadavg) does not start.
set -euo pipefail
label=${1:?label}; b=${2:?bin}; body=${3:?body file}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY"; unset KEY
TASK_LIST=$(python3 "$HERE/task_set.py" resolve "${AB_TASKS:-ab3}" "$HERE/tasks") || exit 1
TASKS=()
while IFS= read -r task; do TASKS+=("$task"); done <<< "$TASK_LIST"
out=$HOME/tb2/jobs/$label; mkdir -p "$out"; cp "$body" "$out/body.json"
printf '%s\n' "${TASKS[@]}" > "$out/tasks.txt"
M=opencode-go/deepseek-v4.1-flash
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
COMMON=(-d terminal-bench@2.0 -m $M -n "${AB_N:-3}" -k "${AB_K:-3}" -y "${I[@]}")
BIN=$HOME/tb2/bin/$b; SHA=$(cut -d' ' -f1 "$BIN.sha256")

# The request capture: one mitmdump per arm (base, body), from AB_CAPTURE_PORT.
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
pids=()
index=0
# The caller may have RADIAN_AB_BODY_FILE exported; the base arm must not inherit it.
for arm in base body; do
  # See ab.sh: ${AK[@]+...} is the form that is safe under `set -u` on bash 3.2.
  AK=()
  if [ -n "$CAPTURE" ]; then
    capture_start "$out" "$arm" $((CAPTURE_PORT + index)) "$HERE/capture_addon.py"
    AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
  fi
  ( if [ $arm = body ]; then export RADIAN_AB_BODY_FILE="$out/body.json"; else unset RADIAN_AB_BODY_FILE; fi
    PYTHONPATH=$PP harbor run "${COMMON[@]}" -a radian_body:RadianBody \
      --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high \
      ${AK[@]+"${AK[@]}"} -o "$out/$arm" > "$out/$arm.log" 2>&1 && rc=0 || rc=$?
    echo "$arm rc=$rc" >> "$out/done" ) &
  pids+=($!)
  index=$((index + 1))
done
for p in "${pids[@]}"; do wait "$p" || true; done
if [ -n "$CAPTURE" ]; then capture_stop_all; fi
if [ -n "$sampler" ]; then kill "$sampler"; fi
echo "all done: $out"
