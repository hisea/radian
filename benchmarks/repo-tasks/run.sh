#!/bin/bash
# One repository-task run: run.sh <arm> <task> <k>
#
# Makes a run tree that holds the task's base sha and nothing else, runs the
# agent there with the task's brief on stdin (radian) or on the pi command line,
# under a wall-time cap, and saves `change.diff`, the session journal,
# `run.json` and `leak.json`.
#
# A tree is not a clone and not a worktree: `git init`, a depth-1 fetch of the
# base sha from REPO_TASKS_SOURCE, a detached checkout (lib.py `make_tree`). It
# has no remote, no branch, no tag and no commit after its base, so an agent
# cannot read the task's merged fix from it. The source is only read; the fetch
# passes its upload-pack options on the command line, so the source's
# `.git/config` is never written. A run root inside any git checkout is refused:
# an agent that leaves its tree would find that checkout's history.
#
# Environment:
#   REPO_TASKS_SOURCE   repository the base is fetched from (default: the repository of this file)
#   REPO_TASKS_RUNS     output root (default: ${XDG_CACHE_HOME:-$HOME/.cache}/radian-repo-tasks/runs)
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

SOURCE=$(python3 "$LIB" source) || exit 2
[ -d "$SOURCE" ] || die "no source repository at $SOURCE; set REPO_TASKS_SOURCE"
RUNS=$(python3 "$LIB" runs-root) || exit 2
TIMEOUT=${REPO_TASKS_TIMEOUT:-2700}
RADIAN_BIN=${REPO_TASKS_RADIAN_BIN:-radian}
PI_BIN=${REPO_TASKS_PI_BIN:-pi}

# Before anything is created: a run root inside a git checkout is refused.
python3 "$LIB" check-runs-root "$RUNS" || exit 2

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
if [ -e "$TREE" ]; then die "run tree exists: $TREE (remove it first)"; fi
mkdir -p "$OUT/session" "$RUNS/trees"

# The tree's only commit is the base; make_tree refuses any other count, and
# the count goes into run.json as `tree_commits`.
tree_commits=$(python3 "$LIB" make-tree "$SOURCE" "$base" "$TREE") || exit 2
[ "$tree_commits" = "1" ] || die "the run tree $TREE reaches $tree_commits commits, not 1"

python3 "$LIB" brief "$task" > "$OUT/brief.md" || exit 2

start=$(python3 -c 'import time; print(time.time())')

# The wall-time cap. `timeout_run.py` is the cap on every machine: it needs no
# coreutils, so a run is never started without one.
run_capped() {
  python3 "$HERE/timeout_run.py" --kill-after 30 "$TIMEOUT" -- "$@"
}

if [ "$agent" = "radian" ]; then
  # shellcheck disable=SC2086
  # Without `--workspace` radian exec has no file, shell or job tools.
  ( cd "$TREE" && run_capped "$RADIAN_BIN" exec --output jsonl \
      --workspace "$TREE" \
      --session-dir "$OUT/session" --provider "$provider" --model "$model" \
      $extra -p - < "$OUT/brief.md" ) \
      > "$OUT/radian.stdout.jsonl" 2> "$OUT/radian.stderr.log"
  rc=$?
elif [ "$agent" = "pi" ]; then
  # shellcheck disable=SC2086
  # `--session-dir` keeps pi's session in the run, where leak.py reads it.
  ( cd "$TREE" && run_capped "$PI_BIN" --model "$provider/$model" \
      --session-dir "$OUT/session" $extra -p "$(cat "$OUT/brief.md")" < /dev/null ) \
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
# untracked file part of it (it is the run tree's own index).
git -C "$TREE" add -A >/dev/null 2>&1
git -C "$TREE" diff --no-ext-diff --no-textconv "$base" > "$OUT/change.diff"
git -C "$TREE" status --short > "$OUT/status-after.txt"
git -C "$TREE" log --format='%h %s' "$base"..HEAD > "$OUT/commits.txt" 2>/dev/null
journal=$(find "$OUT/session" -name '*.jsonl' -type f 2>/dev/null | head -1)

python3 - "$OUT/run.json" "$arm" "$task_id" "$k" "$base" "$agent" \
  "$provider" "$model" "$extra" "$rc" "$timed_out" "$wall" "$journal" "$OUT/session" \
  "$TREE" "$tree_commits" <<'EOF'
import json, sys
(path, arm, task, k, base, agent, provider, model, extra, rc, timed_out,
 wall, journal, session_dir, tree, tree_commits) = sys.argv[1:]
status = "timeout" if timed_out == "1" else ("ok" if rc == "0" else "failed")
json.dump({
    "arm": arm, "task": task, "k": int(k), "base": base,
    "agent": agent, "provider": provider, "model": model,
    "extra": extra.split(), "exit_code": int(rc),
    "timed_out": timed_out == "1", "status": status,
    "wall_seconds": float(wall), "journal": journal or None,
    "session_dir": session_dir, "tree": tree,
    "tree_commits": int(tree_commits),
}, open(path, "w"), indent=1)
print(json.dumps({"arm": arm, "task": task, "k": k, "status": status,
                  "exit_code": int(rc), "wall_seconds": float(wall)}))
EOF

# Whether the agent reached later commits; a failure here does not fail the run.
python3 "$HERE/leak.py" "$OUT" --source "$SOURCE" > /dev/null \
  || echo "run.sh: leak.py could not scan $OUT" >&2
