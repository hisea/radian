#!/bin/bash
# One review of a finished run: review.sh <arm> <task> <k> [reviewer-arm]
#
# Builds arc's first-review prompt from `crates/radian-arc` (see prompt.py),
# appends this run's commits and `change.diff`, and runs the reviewer named by
# the reviewer arm file (default `arms/reviewer.md`) read-only in the run tree.
# The reviewer's command and model come from that file, never from a key in
# the repository.
#
# The result is `review.json`: whether a report parsed, its blocking items and
# `review_ok` (parsed and no blocking item). Exit 0 passes, 1 blocks or is
# unusable, 2 is a usage or setup error.
#
# Environment:
#   REPO_TASKS_RUNS       output root (default: benchmarks/repo-tasks/runs)
#   REPO_TASKS_TIMEOUT    wall-time cap for the review in seconds (default: 1800)
#   REPO_TASKS_RADIAN_BIN / REPO_TASKS_PI_BIN  agent binaries
#   REPO_TASKS_REVIEW_RS  review.rs to read the prompt from (tests)
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
LIB=$HERE/lib.py
die() { echo "review.sh: $*" >&2; exit 2; }

[ $# -eq 3 ] || [ $# -eq 4 ] || die "usage: review.sh <arm> <task> <k> [reviewer-arm]"
arm=$1
task=$2
k=$3
reviewer=${4:-reviewer}

RUNS=${REPO_TASKS_RUNS:-$HERE/runs}
case $RUNS in /*) ;; *) RUNS="$PWD/$RUNS" ;; esac
TIMEOUT=${REPO_TASKS_TIMEOUT:-1800}
RADIAN_BIN=${REPO_TASKS_RADIAN_BIN:-radian}
PI_BIN=${REPO_TASKS_PI_BIN:-pi}

r_agent=$(python3 "$LIB" arm "$reviewer" agent) || exit 2
r_provider=$(python3 "$LIB" arm "$reviewer" provider) || exit 2
r_model=$(python3 "$LIB" arm "$reviewer" model) || exit 2
r_extra=$(python3 "$LIB" arm "$reviewer" extra) || exit 2
task_id=$(python3 "$LIB" task "$task" id) || exit 2
base=$(python3 "$LIB" task "$task" base) || exit 2

OUT=$RUNS/$arm/$task_id/$k
TREE=$RUNS/trees/$arm-$task_id-$k
[ -f "$OUT/run.json" ] || die "no run for $arm $task_id $k (run run.sh first)"
[ -f "$OUT/change.diff" ] || die "no change.diff for $arm $task_id $k"
[ -d "$TREE" ] || die "no worktree for $arm $task_id $k"

python3 "$HERE/prompt.py" "$task" "$TREE" "$base" "$OUT/change.diff" \
  > "$OUT/review-prompt.md" || exit 2

TIMEOUT_BIN=""
if command -v gtimeout >/dev/null 2>&1; then TIMEOUT_BIN=gtimeout
elif command -v timeout >/dev/null 2>&1; then TIMEOUT_BIN=timeout
fi
run_capped() {
  if [ -n "$TIMEOUT_BIN" ]; then "$TIMEOUT_BIN" -k 30 "$TIMEOUT" "$@"; else "$@"; fi
}

mkdir -p "$OUT/review-session"
if [ "$r_agent" = "radian" ]; then
  # shellcheck disable=SC2086
  ( cd "$TREE" && run_capped "$RADIAN_BIN" exec --output jsonl \
      --session-dir "$OUT/review-session" --provider "$r_provider" \
      --model "$r_model" $r_extra -p - < "$OUT/review-prompt.md" ) \
      > "$OUT/review.stdout.log" 2> "$OUT/review.stderr.log"
  rc=$?
elif [ "$r_agent" = "pi" ]; then
  # shellcheck disable=SC2086
  ( cd "$TREE" && run_capped "$PI_BIN" --model "$r_provider/$r_model" \
      $r_extra -p "$(cat "$OUT/review-prompt.md")" < /dev/null ) \
      > "$OUT/review.stdout.log" 2> "$OUT/review.stderr.log"
  rc=$?
else
  die "reviewer arm $reviewer names an agent this script does not know: $r_agent"
fi

timed_out=0
case $rc in 124|137) timed_out=1 ;; esac
python3 - "$OUT/review.json" "$reviewer" "$r_agent" "$r_provider" "$r_model" \
  "$rc" "$timed_out" "$OUT/review.stdout.log" <<'EOF'
import json, re, sys
path, reviewer, agent, provider, model, rc, timed_out, log = sys.argv[1:]
text = open(log, encoding="utf-8", errors="replace").read()
# The last closed fenced `json` block, as arc's `parse_report` reads it.
reports = [
    (info.strip(), body)
    for info, body in re.findall(r"```([^\n`]*)\n(.*?)```", text, re.S)
]
verdict = None
for info, body in reversed(reports):
    if info.lower() == "json":
        try:
            candidate = json.loads(body)
        except ValueError:
            continue
        if isinstance(candidate, dict) and "blocking" in candidate:
            verdict = candidate
        break
levels = [
    item.get("level") if isinstance(item, dict) else None
    for item in (verdict or {}).get("blocking", [])
]
blocking = [
    item for item in (verdict or {}).get("blocking", [])
    if not isinstance(item, dict) or item.get("level", "major") in ("critical", "major")
]
result = {
    "reviewer": reviewer, "agent": agent, "provider": provider, "model": model,
    "exit_code": int(rc), "timed_out": timed_out == "1",
    "parsed": verdict is not None, "blocking": len(blocking),
    "all_blocking": len((verdict or {}).get("blocking", [])),
    "levels": levels, "verdict": verdict,
}
result["review_ok"] = verdict is not None and not blocking
json.dump(result, open(path, "w"), indent=1)
print(json.dumps({key: result[key] for key in
                  ("parsed", "blocking", "levels", "review_ok", "timed_out")}))
sys.exit(0 if result["review_ok"] else 1)
EOF
