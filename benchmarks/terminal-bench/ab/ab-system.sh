#!/bin/bash
# System-prompt A/B: one radian binary, `<role>` default vs `<role>` from a file, same run.
# Usage: bash benchmarks/terminal-bench/ab/ab-system.sh <label> <radian-bin-name> <role-file>
# Same 10 tasks and model as ab.sh; -k ${AB_K:-3}, -n ${AB_N:-3} per arm.
# Output: $HOME/tb2/jobs/<label>/{base,concise}/ and $HOME/tb2/jobs/<label>/done; the binary
# is $HOME/tb2/bin/<name>. PYTHONPATH comes from this script's own checkout.
set -euo pipefail
label=${1:?label}; b=${2:?bin}; role=${3:?role file}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')
export RADIAN_BENCH_API_KEY="$KEY"; unset KEY
out=$HOME/tb2/jobs/$label; mkdir -p "$out"; cp "$role" "$out/role.txt"
M=opencode-go/deepseek-v4.1-flash
TASKS=(llm-inference-batching-scheduler largest-eigenval write-compressor reshard-c4-data torch-tensor-parallelism
       password-recovery break-filter-js-from-html portfolio-optimization gpt2-codegolf merge-diff-arc-agi-task)
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
COMMON=(-d terminal-bench@2.0 -m $M -n "${AB_N:-3}" -k "${AB_K:-3}" -y "${I[@]}")
BIN=$HOME/tb2/bin/$b; SHA=$(cut -d' ' -f1 "$BIN.sha256")
( while true; do echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $(cut -d' ' -f1-3 /proc/loadavg)" >> "$out/loadavg.log"; sleep 30; done ) &
sampler=$!
pids=()
# The caller may have RADIAN_AB_SYSTEM_FILE exported; the base arm must not inherit it.
for arm in base concise; do
  ( if [ $arm = concise ]; then export RADIAN_AB_SYSTEM_FILE="$out/role.txt"; else unset RADIAN_AB_SYSTEM_FILE; fi
    PYTHONPATH=$PP harbor run "${COMMON[@]}" -a radian_system:RadianSystem \
      --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high -o "$out/$arm" > "$out/$arm.log" 2>&1 && rc=0 || rc=$?
    echo "$arm rc=$rc" >> "$out/done" ) &
  pids+=($!)
done
for p in "${pids[@]}"; do wait "$p" || true; done
kill "$sampler"
echo "all done: $out"
