#!/bin/bash
# One repository-task run: run.sh <arm> <task> <k>
#
# Creates a detached worktree of the task's base sha in a separate clone, runs
# the agent there with the task's brief on stdin (radian) or on the pi command
# line, under a wall-time cap, and saves `change.diff`, the session journal and
# `run.json`.
#
# The repository this script is started in is only read: the worktree is made in
# the clone (`REPO_TASKS_CLONE`, default `runs/clone`), never in the caller's
# repository, so its `.git/config` is not touched. `REPO_TASKS_SOURCE` names the
# repository to clone from; it defaults to the repository holding this script.
#
# Environment:
#   REPO_TASKS_SOURCE   repository to clone (default: the repository of this file)
#   REPO_TASKS_CLONE    the separate clone (default: <runs>/clone)
#   REPO_TASKS_RUNS     output root (default: benchmarks/repo-tasks/runs)
#   REPO_TASKS_TIMEOUT  wall-time cap in seconds (default: 2700)
#   REPO_TASKS_RADIAN_BIN  radian binary (default: radian)
#   REPO_TASKS_PI_BIN      pi binary (default: pi)
#   REPO_TASKS_TASKS_DIR / REPO_TASKS_ARMS_DIR  another task/arm set (tests)
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
LIB=$HERE/lib.py

die() { echo "run.sh: $*" >&2; exit 2; }

[ $# -eq 3 ] || die "usage: run.sh <arm> <task> <k>"
arm=$1
task=$2
k=$3

SOURCE=${REPO_TASKS_SOURCE:-$(git -C "$HERE" rev-parse --show-toplevel 2>/dev/null)}
[ -n "$SOURCE" ] && [ -d "$SOURCE" ] || die "no source repository; set REPO_TASKS_SOURCE"
case $SOURCE in /*) ;; *) SOURCE="$PWD/$SOURCE" ;; esac
RUNS=${REPO_TASKS_RUNS:-$HERE/runs}
CLONE=${REPO_TASKS_CLONE:-$RUNS/clone}
# A worktree path is resolved by `git -C <clone>`; keep both roots absolute.
case $RUNS in /*) ;; *) RUNS="$PWD/$RUNS" ;; esac
case $CLONE in /*) ;; *) CLONE="$PWD/$CLONE" ;; esac
TIMEOUT=${REPO_TASKS_TIMEOUT:-2700}
RADIAN_BIN=${REPO_TASKS_RADIAN_BIN:-radian}
PI_BIN=${REPO_TASKS_PI_BIN:-pi}

# Validate names before creating anything: an unknown name lists the valid ones.
agent=$(python3 "$LIB" arm "$arm" agent) || exit 2
provider=$(python3 "$LIB" arm "$arm" provider) || exit 2
model=$(python3 "$LIB" arm "$arm" model) || exit 2
extra=$(python3 "$LIB" arm "$arm" extra) || exit 2
task_id=$(python3 "$LIB" task "$task" id) || exit 2
base=$(python3 "$LIB" task "$task" base) || exit 2

OUT=$RUNS/$arm/$task_id/$k
if [ -e "$OUT/run.json" ]; then die "run exists: $OUT (remove it first)"; fi
TREE=$RUNS/trees/$arm-$task_id-$k
if [ -e "$TREE" ]; then die "worktree exists: $TREE (remove it first)"; fi
mkdir -p "$OUT/session" "$RUNS/trees"

# A separate clone. The caller's repository is read by `clone`/`fetch` only; a
# worktree or a branch is never made in it.
if [ ! -d "$CLONE/.git" ]; then
  mkdir -p "$(dirname "$CLONE")"
  git clone --quiet --shared --no-checkout "$SOURCE" "$CLONE" \
    || die "cannot clone $SOURCE to $CLONE"
fi
if ! git -C "$CLONE" cat-file -e "$base^{commit}" 2>/dev/null; then
  git -C "$CLONE" fetch --quiet --no-tags "$SOURCE" \
    '+refs/heads/*:refs/remotes/eval/*' || die "cannot fetch from $SOURCE"
fi
git -C "$CLONE" cat-file -e "$base^{commit}" 2>/dev/null \
  || die "base $base is not in $CLONE (clone an older or deeper repository)"
git -C "$CLONE" worktree add --quiet --detach "$TREE" "$base" \
  || die "cannot make a worktree at $TREE"

python3 "$LIB" brief "$task" > "$OUT/brief.md" || exit 2

start=$(python3 -c 'import time; print(time.time())')

# The wall-time cap. GNU coreutils installs `timeout`; on macOS Homebrew it is
# `gtimeout`. Without either, run without a cap and say so.
TIMEOUT_BIN=""
if command -v gtimeout >/dev/null 2>&1; then
  TIMEOUT_BIN=gtimeout
elif command -v timeout >/dev/null 2>&1; then
  TIMEOUT_BIN=timeout
else
  echo "run.sh: no gtimeout/timeout; running without a wall-time cap" >&2
fi
run_capped() {
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" -k 30 "$TIMEOUT" "$@"
  else
    "$@"
  fi
}

if [ "$agent" = "radian" ]; then
  # shellcheck disable=SC2086
  ( cd "$TREE" && run_capped "$RADIAN_BIN" exec --output jsonl \
      --session-dir "$OUT/session" --provider "$provider" --model "$model" \
      $extra -p - < "$OUT/brief.md" ) \
      > "$OUT/radian.stdout.jsonl" 2> "$OUT/radian.stderr.log"
  rc=$?
elif [ "$agent" = "pi" ]; then
  # shellcheck disable=SC2086
  ( cd "$TREE" && run_capped "$PI_BIN" --model "$provider/$model" \
      $extra -p "$(cat "$OUT/brief.md")" < /dev/null ) \
      > "$OUT/pi.stdout.log" 2> "$OUT/pi.stderr.log"
  rc=$?
else
  die "arm $arm names an agent this runner does not know: $agent"
fi
end=$(python3 -c 'import time; print(time.time())')
wall=$(python3 -c "print(round($end-$start, 3))")
timed_out=0
case $rc in 124|137) timed_out=1 ;; esac

# The agent's committed and uncommitted change, one diff. `add -A` makes an
# untracked file part of it (it is the run tree's index, not the clone's).
git -C "$TREE" add -A >/dev/null 2>&1
git -C "$TREE" diff --no-ext-diff --no-textconv "$base" > "$OUT/change.diff"
git -C "$TREE" status --short > "$OUT/status-after.txt"
git -C "$TREE" log --format='%h %s' "$base"..HEAD > "$OUT/commits.txt" 2>/dev/null
journal=$(find "$OUT/session" -name '*.jsonl' -type f 2>/dev/null | head -1)

python3 - "$OUT/run.json" "$arm" "$task_id" "$k" "$base" "$agent" \
  "$provider" "$model" "$extra" "$rc" "$timed_out" "$wall" "$journal" <<'EOF'
import json, sys
(path, arm, task, k, base, agent, provider, model, extra, rc, timed_out,
 wall, journal) = sys.argv[1:]
status = "timeout" if timed_out == "1" else ("ok" if rc == "0" else "failed")
json.dump({
    "arm": arm, "task": task, "k": int(k), "base": base,
    "agent": agent, "provider": provider, "model": model,
    "extra": extra.split(), "exit_code": int(rc),
    "timed_out": timed_out == "1", "status": status,
    "wall_seconds": float(wall), "journal": journal or None,
}, open(path, "w"), indent=1)
print(json.dumps({"arm": arm, "task": task, "k": k, "status": status,
                  "exit_code": int(rc), "wall_seconds": float(wall)}))
EOF
