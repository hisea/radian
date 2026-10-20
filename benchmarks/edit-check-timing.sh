#!/bin/bash
# Measure incremental `cargo check` time after one trivial edit.
#
# Usage: bash benchmarks/edit-check-timing.sh [output-dir]
#
# - Debug profile, worktree-local target/ (CARGO_TARGET_DIR must be unset or
#   inside the worktree).
# - Warms once with `cargo check --workspace --all-targets`.
# - For each file below, makes a trivial content-changing edit (append a
#   `// timing` comment) before EVERY timed run, then times
#   `cargo check -p <crate>` and `cargo check --workspace` (5 runs each,
#   median/max) and `cargo clippy -p <crate>` (3 runs, median/max).
# - Verifies the edited crate actually rebuilt by grepping the
#   `Checking <crate>` line in cargo's output (`Compiling <crate>` counts:
#   clippy's first cold run prints Compiling for the edited crate).
# - Records `uptime` load before each series.
# - Restores every edited file before exiting (trap).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

OUTDIR="${1:-/tmp/edit-check-timing}"
mkdir -p "$OUTDIR"

if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
  case "$CARGO_TARGET_DIR" in
    "$ROOT"/*) echo "CARGO_TARGET_DIR=$CARGO_TARGET_DIR (inside worktree, ok)" ;;
    *) echo "refusing: CARGO_TARGET_DIR=$CARGO_TARGET_DIR is outside the worktree" >&2; exit 1 ;;
  esac
else
  echo "CARGO_TARGET_DIR unset (default target/ inside worktree, ok)"
fi

FILES=(
  "crates/radian-permissions/src/shell.rs:radian-permissions"
  "crates/radian-coding/src/read.rs:radian-coding"
  "crates/radian-host/src/context.rs:radian-host"
  "crates/radian-tui/src/startup.rs:radian-tui"
  "crates/agent-core/src/turn.rs:agent-core"
)

restore() {
  for entry in "${FILES[@]}"; do
    f="${entry%%:*}"
    git checkout -q -- "$f" || true
  done
}
trap restore EXIT

# Run a cargo command, saving combined output to $log and elapsed seconds to $elapsed.
# Exits nonzero if cargo fails.
timed_cargo() {
  local elapsed="$1"; local log="$2"; shift 2
  python3 - "$elapsed" "$log" "$@" <<'PY'
import subprocess, sys, time
elapsed_file, log_file, *cmd = sys.argv[1:]
with open(log_file, "wb") as log:
    s = time.monotonic()
    p = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT)
    e = time.monotonic()
open(elapsed_file, "w").write(f"{e - s:.3f}\n")
sys.exit(p.returncode)
PY
}

# median and max of args, printed as "median max"
stats() {
  python3 - "$@" <<'PY'
import statistics, sys
xs = sorted(float(a) for a in sys.argv[1:])
print(f"{statistics.median(xs):.3f} {max(xs):.3f}")
PY
}

touch_edit() {
  local file="$1" tag="$2"
  echo "// timing $tag $(date +%s%N 2>/dev/null || date +%s)" >> "$file"
}

series() {
  local file="$1" crate="$2" label="$3" runs="$4"
  shift 4
  # remaining args: cargo command
  local i elapsed log dur
  local times=()
  echo "--- $label ($crate), $runs runs: $* ---"
  echo "uptime before $label $crate: $(uptime)" | tee -a "$OUTDIR/uptime.log"
  for i in $(seq 1 "$runs"); do
    touch_edit "$file" "$crate-$label-$i"
    elapsed="$(mktemp)"; log="$(mktemp)"
    timed_cargo "$elapsed" "$log" "$@"
    dur="$(cat "$elapsed")"
    rm -f "$elapsed"
    if ! grep -qE "(Checking|Compiling) $crate" "$log"; then
      echo "WARN: no 'Checking/Compiling $crate' line in $* run $i (see $log)" >&2
      tail -n 5 "$log" >&2 || true
    fi
    echo "$label $crate run $i: ${dur}s"
    echo "$dur" >> "$OUTDIR/${crate}.${label}.txt"
    cp "$log" "$OUTDIR/${crate}.${label}.run${i}.log"
    rm -f "$log"
    times+=("$dur")
  done
  read -r med mx <<< "$(stats "${times[@]}")"
  echo "=> $label $crate median ${med}s max ${mx}s"
}

echo "warming target dir once..."
echo "uptime before warm: $(uptime)" | tee "$OUTDIR/uptime.log"
timed_cargo "$OUTDIR/warm.elapsed" "$OUTDIR/warm.log" cargo check --workspace --all-targets
echo "warm done in $(cat "$OUTDIR/warm.elapsed")s"

for entry in "${FILES[@]}"; do
  f="${entry%%:*}"
  c="${entry##*:}"
  series "$f" "$c" "check-p" 5 cargo check -p "$c"
  series "$f" "$c" "check-workspace" 5 cargo check --workspace
  series "$f" "$c" "clippy-p" 3 cargo clippy -p "$c"
done

echo
echo "summary (median max, seconds):"
for entry in "${FILES[@]}"; do
  c="${entry##*:}"
  for label in check-p check-workspace clippy-p; do
    read -r med mx <<< "$(stats $(cat "$OUTDIR/$c.$label.txt"))"
    printf "%-18s %-15s median %8ss max %8ss\n" "$c" "$label" "$med" "$mx"
  done
done

echo
echo "raw times and logs in $OUTDIR"
