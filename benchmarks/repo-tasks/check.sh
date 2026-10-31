#!/bin/bash
# The task's crate-scoped checks for one finished run: check.sh <arm> <task> <k>
#
# Runs, in the run's own tree and target directory:
#   cargo build -p radian-tui --bin radian --locked               (when the tree has it)
#   cargo build -p agent-extensions --bin mcp-fixture --locked   (when the tree has it)
#   cargo fmt --all -- --check
#   cargo clippy -p <crate> --all-targets -- -D warnings   (once per listed crate)
#   cargo nextest run -p <crate> --locked --no-fail-fast   (once per listed crate)
#
# The task file lists the crates. Every crate's clippy and tests run even after
# an earlier one failed, so one failing crate cannot hide the other. The result
# is `checks.json`; a failing step makes `checks_ok` false and the exit code 1.
#
# The two builds are setup, not checks, as in scripts/verify.sh: radian-exec's
# tests run the `radian` binary and refuse one older than its sources, and tests
# that start the MCP fixture (radian-host `mcp_cwd`, for one) find it beside the
# test binaries; without the builds they fail whatever the agent changed. Their
# results are `binary_ok` and `fixture_ok` (null when the tree has no such
# binary) and do not enter `checks_ok`.
#
# Environment:
#   REPO_TASKS_RUNS      output root (default: ${XDG_CACHE_HOME:-$HOME/.cache}/radian-repo-tasks/runs)
#   REPO_TASKS_CARGO     cargo binary (default: cargo; tests use a stub)
#   REPO_TASKS_MAX_JOBS  passed to cargo clippy and nextest as `--jobs` (optional;
#                        never to `cargo fmt`, which would forward it to rustfmt)
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
LIB=$HERE/lib.py
die() { echo "check.sh: $*" >&2; exit 2; }

[ $# -eq 3 ] || die "usage: check.sh <arm> <task> <k>"
arm=$1
task=$2
k=$3

RUNS=$(python3 "$LIB" runs-root) || exit 2
CARGO=${REPO_TASKS_CARGO:-cargo}
task_id=$(python3 "$LIB" task "$task" id) || exit 2
crates=$(python3 "$LIB" task "$task" crates) || exit 2
[ -n "$crates" ] || die "task $task lists no crates"

OUT=$RUNS/$arm/$task_id/$k
TREE=$RUNS/trees/$arm-$task_id-$k
[ -d "$TREE" ] || die "no run tree for $arm $task_id $k (run run.sh first)"
mkdir -p "$OUT"
cd "$TREE" || die "cannot enter $TREE"
unset CARGO_TARGET_DIR
# `cargo fmt` forwards anything after `--` to rustfmt, which has no `--jobs`, so
# the option goes to clippy and nextest only.
JOBS=""
if [ -n "${REPO_TASKS_MAX_JOBS:-}" ]; then JOBS="--jobs $REPO_TASKS_MAX_JOBS"; fi

log=$OUT/checks.log
: > "$log"
fmt_rc=0
clippy_rc=()
tests_rc=()
step() {
  echo "=== $*" >> "$log"
  "$@" >> "$log" 2>&1
  local rc=$?
  echo "=== rc=$rc" >> "$log"
  return $rc
}

binary_rc=""
if grep -qs '^name = "radian"' crates/radian-tui/Cargo.toml; then
  step "$CARGO" build $JOBS -p radian-tui --bin radian --locked
  binary_rc=$?
fi
fixture_rc=""
if grep -qs '^name = "mcp-fixture"' crates/agent-extensions/Cargo.toml; then
  step "$CARGO" build $JOBS -p agent-extensions --bin mcp-fixture --locked
  fixture_rc=$?
fi

step "$CARGO" fmt --all -- --check || fmt_rc=1

index=0
for crate in ${crates//,/ }; do
  index=$((index + 1))
  step "$CARGO" clippy $JOBS -p "$crate" --all-targets -- -D warnings
  clippy_rc[$index]=$?
  step "$CARGO" nextest run $JOBS -p "$crate" --locked --no-fail-fast
  tests_rc[$index]=$?
done

python3 - "$OUT/checks.json" "$arm" "$task_id" "$k" "$crates" "$fmt_rc" \
  "${clippy_rc[*]:-}" "${tests_rc[*]:-}" "$fixture_rc" "$binary_rc" <<'EOF'
import json, sys
path, arm, task, k, crates, fmt_rc, clippy, tests, fixture_rc, binary_rc = sys.argv[1:]
names = [name for name in crates.replace(",", " ").split() if name]
clippy_rc = [int(value) for value in clippy.split()] if clippy.strip() else [1] * len(names)
tests_rc = [int(value) for value in tests.split()] if tests.strip() else [1] * len(names)
result = {
    "arm": arm, "task": task, "k": int(k), "crates": names,
    "binary_ok": None if binary_rc == "" else int(binary_rc) == 0,
    "fixture_ok": None if fixture_rc == "" else int(fixture_rc) == 0,
    "fmt_ok": int(fmt_rc) == 0,
    "clippy_ok": {name: rc == 0 for name, rc in zip(names, clippy_rc)},
    "tests_ok": {name: rc == 0 for name, rc in zip(names, tests_rc)},
}
result["checks_ok"] = (
    result["fmt_ok"]
    and all(result["clippy_ok"].values())
    and all(result["tests_ok"].values())
)
json.dump(result, open(path, "w"), indent=1)
print(json.dumps(result))
sys.exit(0 if result["checks_ok"] else 1)
EOF
