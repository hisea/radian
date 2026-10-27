#!/bin/bash
# System-prompt A/B: one radian binary, `<role>` default vs `<role>` from a file, same run.
# Usage: bash benchmarks/terminal-bench/ab/ab-system.sh <label> <radian-bin-name> <role-file>
# Same task set (AB_TASKS, default core-2026-10) and model as ab.sh; -k ${AB_K:-3},
# -n ${AB_N:-3} per arm. The list the run uses is written to <label>/tasks.txt.
# AB_CAPTURE=1 captures both arms' model requests (AB_CAPTURE_PORT, default
# 18080; see README.md); unset, the commands below are exactly what this script
# has always run.
# Output: $HOME/tb2/jobs/<label>/{base,concise}/ and $HOME/tb2/jobs/<label>/done;
# the binary is $HOME/tb2/bin/<name>. PYTHONPATH comes from this script's own checkout.
# AB_TEST_SKIP_LOADAVG is test-only: tests/test_ab_scripts.py sets it so the
# sampler (which reads /proc/loadavg) does not start.
set -euo pipefail
label=${1:?label}; b=${2:?bin}; role=${3:?role file}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY"; unset KEY
TASK_LIST=$(python3 "$HERE/task_set.py" resolve "${AB_TASKS:-core-2026-10}" "$HERE/tasks") || exit 1
TASKS=()
while IFS= read -r task; do TASKS+=("$task"); done <<< "$TASK_LIST"
out=$HOME/tb2/jobs/$label; mkdir -p "$out"; cp "$role" "$out/role.txt"
printf '%s\n' "${TASKS[@]}" > "$out/tasks.txt"
M=opencode-go/deepseek-v4.1-flash
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
COMMON=(-d terminal-bench@2.0 -m $M -n "${AB_N:-3}" -k "${AB_K:-3}" -y "${I[@]}")
BIN=$HOME/tb2/bin/$b; SHA=$(cut -d' ' -f1 "$BIN.sha256")

# The request capture: one mitmdump per arm (base, concise), from AB_CAPTURE_PORT.
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
# The caller may have RADIAN_AB_SYSTEM_FILE exported; the base arm must not inherit it.
for arm in base concise; do
  # See ab.sh: ${AK[@]+...} is the form that is safe under `set -u` on bash 3.2.
  AK=()
  if [ -n "$CAPTURE" ]; then
    capture_start "$out" "$arm" $((CAPTURE_PORT + index)) "$HERE/capture_addon.py"
    AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
  fi
  ( if [ $arm = concise ]; then export RADIAN_AB_SYSTEM_FILE="$out/role.txt"; else unset RADIAN_AB_SYSTEM_FILE; fi
    PYTHONPATH=$PP harbor run "${COMMON[@]}" -a radian_system:RadianSystem \
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
