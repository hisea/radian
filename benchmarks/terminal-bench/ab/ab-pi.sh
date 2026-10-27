#!/bin/bash
# Relaunch only the pi leg of an ab run (same task set and arguments as ab.sh);
# writes its done line even on failure.
# Usage: bash benchmarks/terminal-bench/ab/ab-pi.sh <label>
# The task set is the label's <label>/tasks.txt when it exists, and AB_TASKS
# (default core-2026-10) otherwise. An AB_TASKS that resolves to a different
# list than the saved file is refused, so one label never mixes sets.
# Output: $HOME/tb2/jobs/<label>/pi/; the host keeps its binaries and jobs under
# $HOME/tb2. PYTHONPATH comes from this script's own checkout.
label=${1:?label}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser(\"~/.radian/credentials/auth.json\")))[\"opencode-go\"][\"key\"])")
export OPENCODE_API_KEY="$KEY"; unset KEY
out=$HOME/tb2/jobs/$label
if [ -f "$out/tasks.txt" ]; then
  TASK_LIST=$(python3 "$HERE/task_set.py" resolve "$out/tasks.txt" "$HERE/tasks") || exit 1
  if [ "${AB_TASKS+set}" = set ]; then
    NAMED=$(python3 "$HERE/task_set.py" resolve "$AB_TASKS" "$HERE/tasks") || exit 1
    if [ "$NAMED" != "$TASK_LIST" ]; then
      echo "ab-pi.sh: AB_TASKS=$AB_TASKS names a different list than $out/tasks.txt" >&2
      exit 1
    fi
  fi
else
  TASK_LIST=$(python3 "$HERE/task_set.py" resolve "${AB_TASKS:-core-2026-10}" "$HERE/tasks") || exit 1
fi
TASKS=()
while IFS= read -r task; do TASKS+=("$task"); done <<< "$TASK_LIST"
mkdir -p "$out"
printf '%s\n' "${TASKS[@]}" > "$out/tasks.txt"
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
mv "$out/pi" "$out/pi.failed-start" 2>/dev/null; mv "$out/pi.log" "$out/pi.failed-start.log" 2>/dev/null
PYTHONPATH=$PP harbor run -d terminal-bench@2.0 -m opencode-go/deepseek-v4.1-flash -n "${AB_N:-2}" -k "${AB_K:-2}" -y "${I[@]}" \
  -a pi_opencode_go:PiOpencodeGo --ak version=1.4.0 --ak thinking=high -o "$out/pi" > "$out/pi.log" 2>&1
echo "pi rc=$?" >> "$out/done"
