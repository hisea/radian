#!/bin/bash
# One task, one radian trial: checks harbor + docker + key on this host.
# Usage: bash benchmarks/terminal-bench/ab/smoke.sh <binary-name> [label]
#   <binary-name> is a file under $HOME/tb2/bin with its .sha256 sidecar; label defaults to smoke.
# AB_CAPTURE=1 captures radian's model requests (AB_CAPTURE_PORT, default 18080;
# see README.md); unset, the command below is exactly what this script has always run.
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
AK=()
if [ -n "${AB_CAPTURE:-}" ]; then
  source "$HERE/capture.sh"
  capture_require_mitmdump
  trap capture_stop_all EXIT
  capture_start "$out" radian "${AB_CAPTURE_PORT:-18080}" "$HERE/capture_addon.py"
  AK=(--ak "capture_port=$AB_CAPTURE_LAST_PORT" --ak "capture_ca=$AB_CAPTURE_LAST_CA")
fi
PYTHONPATH=$PP harbor run -d terminal-bench@2.0 -m opencode-go/deepseek-v4.1-flash -n 1 -k 1 -y \
  -i merge-diff-arc-agi-task -a radian_agent:RadianAgent --ak binary_path="$BIN" --ak binary_sha256="$SHA" --ak thinking=high \
  ${AK[@]+"${AK[@]}"} -o "$out/radian" > "$out/radian.log" 2>&1
rc=$?
if [ -n "${AB_CAPTURE:-}" ]; then capture_stop_all; fi
echo "rc=$rc" > "$out/done"
