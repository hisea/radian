#!/bin/bash
# Proxy bookkeeping for AB_CAPTURE: sourced by smoke.sh, ab.sh and ab-system.sh.
# Not run directly.
#
# With AB_CAPTURE unset none of this runs and the scripts execute the commands
# they always executed. With it set, each arm gets one mitmdump (capture_addon.py)
# on its own port, and the arm's agent gets `--ak capture_port` / `--ak capture_ca`
# so the traffic it sends can be compared field by field afterwards
# (capture_check.py). The proxy itself changes no agent configuration.

AB_CAPTURE_PIDS=()
AB_CAPTURE_LAST_CA=
AB_CAPTURE_LAST_PORT=

# Half-second steps: 120 of them is a minute for mitmdump to write its CA.
_AB_CAPTURE_WAIT_STEPS=120

capture_require_mitmdump() {
  if ! command -v mitmdump >/dev/null 2>&1; then
    echo "$(basename "$0"): AB_CAPTURE is set but mitmdump is not on PATH (uv tool install mitmproxy)" >&2
    exit 1
  fi
}

# capture_start <label-dir> <arm> <port> <addon-script>
#
# Starts the arm's proxy, waits for its CA file, and leaves the CA in
# AB_CAPTURE_LAST_CA (and the port in AB_CAPTURE_LAST_PORT) for the caller's
# `--ak capture_ca` / `--ak capture_port`.
capture_start() {
  local out=$1 arm=$2 port=$3 addon=$4
  local dir=$out/capture/$arm
  local jsonl=$out/capture/$arm.jsonl
  local confdir=$dir/confdir
  local ca=$confdir/mitmproxy-ca-cert.pem
  mkdir -p "$out/capture" "$confdir"
  : >"$jsonl"
  # The proxy needs no credential, and a host process holding one is a needless
  # second copy: start it without the arm's keys.
  env -u RADIAN_BENCH_API_KEY -u OPENCODE_API_KEY AB_CAPTURE_OUT="$jsonl" \
    mitmdump --mode regular --listen-host 0.0.0.0 --listen-port "$port" \
    -s "$addon" --set confdir="$confdir" >"$dir/mitmdump.log" 2>&1 &
  local pid=$!
  AB_CAPTURE_PIDS+=("$pid")
  local step=0
  while [ ! -s "$ca" ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "$(basename "$0"): mitmdump for arm $arm exited before writing its CA; see $dir/mitmdump.log" >&2
      exit 1
    fi
    step=$((step + 1))
    if [ "$step" -ge "$_AB_CAPTURE_WAIT_STEPS" ]; then
      echo "$(basename "$0"): mitmdump for arm $arm wrote no CA file within 60 s; see $dir/mitmdump.log" >&2
      exit 1
    fi
    sleep 0.5
  done
  AB_CAPTURE_LAST_CA=$ca
  AB_CAPTURE_LAST_PORT=$port
}

# Stops every proxy this script started. Idempotent: it is called explicitly
# after the arms and again from the EXIT trap, so a failed arm leaves no proxy.
capture_stop_all() {
  local pid
  for pid in ${AB_CAPTURE_PIDS[@]+"${AB_CAPTURE_PIDS[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  for pid in ${AB_CAPTURE_PIDS[@]+"${AB_CAPTURE_PIDS[@]}"}; do
    wait "$pid" 2>/dev/null || true
  done
  AB_CAPTURE_PIDS=()
  return 0
}
