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
# `review_ok` (a completed, exit-0 session that parsed and had no blocking
# item). Exit 0 passes, 1 blocks or is unusable, 2 is a usage or setup error.
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

run_capped() {
  python3 "$HERE/timeout_run.py" --kill-after 30 "$TIMEOUT" -- "$@"
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
  "$rc" "$timed_out" "$OUT/review.stdout.log" "$OUT/review-session" "$HERE" <<'EOF'
import json, sys
path, reviewer, agent, provider, model, rc, timed_out, log, session, here = sys.argv[1:]
sys.path.insert(0, here)
import lib
log_text = open(log, encoding="utf-8", errors="replace").read()
# A radian reviewer writes JSONL to stdout, where the answer is only an escaped
# string; its final assistant message in the session journal is the report.
report_text = log_text
if agent == "radian":
    answer = lib.final_assistant_text(session)
    if answer is not None:
        report_text = answer
parsed = lib.parse_review_text(report_text)
# arc accepts a report only from a session that completed and exited 0. A
# radian session ends with a `Result` line; a pi review has none, so its exit
# code is the only signal.
completed = None
for line in reversed(log_text.splitlines()):
    try:
        value = json.loads(line)
    except ValueError:
        continue
    if isinstance(value, dict) and value.get("type") == "Result":
        completed = value.get("outcome") == "completed"
        break
result = {
    "reviewer": reviewer, "agent": agent, "provider": provider, "model": model,
    "exit_code": int(rc), "timed_out": timed_out == "1",
    "session_completed": completed,
    "parsed": parsed["parsed"], "blocking": parsed["blocking"],
    "levels": parsed["levels"], "verdict": parsed["verdict"],
}
result["review_ok"] = (
    not result["timed_out"]
    and result["exit_code"] == 0
    and completed is not False
    and parsed["parsed"]
    and parsed["blocking"] == 0
)
json.dump(result, open(path, "w"), indent=1)
print(json.dumps({key: result[key] for key in
                  ("parsed", "blocking", "levels", "review_ok", "timed_out",
                   "session_completed")}))
sys.exit(0 if result["review_ok"] else 1)
EOF
