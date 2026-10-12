#!/usr/bin/env bash
# Startup overhead: /bin/true bare vs through the bwrap launcher, 20 runs each.
set -euo pipefail
[ "$(uname -s)" = Linux ] || exit 1
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="${ROOT:-/tmp/radian-linux-sandbox}"
WS="$ROOT/ws"
export WS ROOT SANDBOX=on NET=off RO_EXTRA="$ROOT/outside:$WS/wt/.git"
python3 - "$HERE/sbwrap.sh" "$HERE/llwrap.sh" <<'PY'
import statistics, subprocess, sys, time
launcher, ll = sys.argv[1], sys.argv[2]
import os
runs = 20

def timed(cmd):
    t0 = time.perf_counter()
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return (time.perf_counter() - t0) * 1000.0

bare = [timed(["/bin/true"]) for _ in range(runs)]
sandboxed = [timed([launcher, "/bin/true"]) for _ in range(runs)]
landlocked = [timed([ll, "/bin/true"]) for _ in range(runs)]

def line(name, xs):
    xs = sorted(xs)
    p95 = xs[int(0.95 * (len(xs) - 1))]
    print(f"{name}: median={statistics.median(xs):.2f}ms p95={p95:.2f}ms n={len(xs)}")

line("bare /bin/true        ", bare)
line("bwrap launcher + true ", sandboxed)
line("landlock wrapper+true ", landlocked)
print(f"bwrap overhead (median diff)    : {statistics.median(sandboxed) - statistics.median(bare):.2f}ms")
print(f"landlock overhead (median diff) : {statistics.median(landlocked) - statistics.median(bare):.2f}ms")
PY
