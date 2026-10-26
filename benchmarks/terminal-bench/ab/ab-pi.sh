#!/bin/bash
# Relaunch only the pi leg of an ab run (same arguments as ab.sh); writes its done line even on failure.
# Usage: bash benchmarks/terminal-bench/ab/ab-pi.sh <label>
# Output: $HOME/tb2/jobs/<label>/pi/; the host keeps its binaries and jobs under $HOME/tb2.
# PYTHONPATH comes from this script's own checkout.
label=${1:?label}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
export PATH="$HOME/.local/bin:$PATH"
KEY=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser(\"~/.radian/credentials/auth.json\")))[\"opencode-go\"][\"key\"])")
export OPENCODE_API_KEY="$KEY"; unset KEY
out=$HOME/tb2/jobs/$label
TASKS=(llm-inference-batching-scheduler largest-eigenval write-compressor reshard-c4-data torch-tensor-parallelism
       password-recovery break-filter-js-from-html portfolio-optimization gpt2-codegolf merge-diff-arc-agi-task)
I=(); for t in "${TASKS[@]}"; do I+=(-i "$t"); done
mv "$out/pi" "$out/pi.failed-start" 2>/dev/null; mv "$out/pi.log" "$out/pi.failed-start.log" 2>/dev/null
PYTHONPATH=$PP harbor run -d terminal-bench@2.0 -m opencode-go/deepseek-v4.1-flash -n "${AB_N:-2}" -k 2 -y "${I[@]}" \
  -a pi_opencode_go:PiOpencodeGo --ak version=1.4.0 --ak thinking=high -o "$out/pi" > "$out/pi.log" 2>&1
echo "pi rc=$?" >> "$out/done"
