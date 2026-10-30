#!/bin/bash
# One arm end to end: eval.sh <arm> <k>...
#
# Runs every task in the fixed set k times, checks each run, optionally reviews
# it, and writes the report. This is the one command the README gives for a
# whole arm.
#
# Environment:
#   REPO_TASKS_REVIEW=1   also run the review for each run
#   REPO_TASKS_BASELINE   the arm the report's ratios divide by (default: none)
#   plus every REPO_TASKS_* variable run.sh/check.sh/review.sh read.
set -uo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
LIB=$HERE/lib.py
die() { echo "eval.sh: $*" >&2; exit 2; }

[ $# -ge 2 ] || die "usage: eval.sh <arm> <k>..."
arm=$1
shift
for k in "$@"; do
  case $k in ''|*[!0-9]*) die "k must be a positive integer: $k" ;; esac
done
python3 "$LIB" arm "$arm" agent >/dev/null || exit 2

fail=0
for task in $(python3 "$LIB" list-tasks); do
  for k in "$@"; do
    echo "== $arm $task k=$k"
    bash "$HERE/run.sh" "$arm" "$task" "$k" || { fail=1; continue; }
    bash "$HERE/check.sh" "$arm" "$task" "$k" || fail=1
    if [ "${REPO_TASKS_REVIEW:-0}" = "1" ]; then
      bash "$HERE/review.sh" "$arm" "$task" "$k" || fail=1
    fi
  done
done

if [ -n "${REPO_TASKS_BASELINE:-}" ]; then
  python3 "$HERE/report.py" --baseline "$REPO_TASKS_BASELINE" || fail=1
else
  python3 "$HERE/report.py" || fail=1
fi
exit $fail
