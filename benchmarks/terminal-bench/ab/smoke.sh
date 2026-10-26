#!/bin/bash
# One task, one radian trial: checks harbor + docker + key on this host.
# Usage: bash benchmarks/terminal-bench/ab/smoke.sh <binary-name> [label]
#   <binary-name> is a file under $HOME/tb2/bin with its .sha256 sidecar; label defaults to smoke.
# Output: $HOME/tb2/jobs/<label>/; the host keeps its binaries and jobs under $HOME/tb2.
# PYTHONPATH comes from this script's own checkout.
export PATH="$HOME/.local/bin:$PATH"
b=${1:?binary name under $HOME/tb2/bin}
label=${2:-smoke}
HERE=$(cd "$(dirname "$0")" && pwd)
TB_DIR=$(cd "$HERE/.." && pwd)
PP=$TB_DIR:$HERE
KEY=$(python3 -c "import json,os;print(json.load(open(os.path.expanduser(\"~/.radian/credentials/auth.json\")))[\"opencode-go\"][\"key\"])")
export RADIAN_BENCH_API_KEY="$KEY"; unset KEY
BIN=$HOME/tb2/bin/$b; SHA=$(cut -d" " -f1 "$BIN.sha256")
out=$HOME/tb2/jobs/$label; mkdir -p "$out"
PYTHONPATH=$PP harbor run -d terminal-bench@2.0 -m opencode-go/deepseek-v4.1-flash -n 1 -k 1 -y \
  -i merge-diff-arc-agi-task -a radian_agent:RadianAgent --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high \
  -o "$out/radian" > "$out/radian.log" 2>&1
echo "rc=$?" > "$out/done"
