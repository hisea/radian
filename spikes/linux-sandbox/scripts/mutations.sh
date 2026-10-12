#!/usr/bin/env bash
# Mutation checks: break one sandbox mechanism and show the matching case flips from the
# sandboxed result to the control result. Each mutation runs the full matrix and prints the
# affected CASE lines; a mutation that changes nothing would leave them at the sandboxed
# values and is called out.
set -uo pipefail
[ "$(uname -s)" = Linux ] || exit 1
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="${ROOT:-/tmp/radian-linux-sandbox}"
cd "$ROOT"
mkdir -p data

mutation() { # mutation <id> <desc> <case-pattern> <env assignments...> -- <wrapper> <label>
  local id="$1" desc="$2" pattern="$3"; shift 3
  local envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  local wrapper="$1" label="$2"
  "$HERE/setup.sh" >/dev/null
  env "${envs[@]}" WRAPPER="$wrapper" SANDBOX=on EXPECT_GIT_RO="${EXPECT_GIT_RO:-on}" \
    "$HERE/matrix.sh" "$label" >"data/mutation-$id.txt" 2>&1
  echo "MUTATION|$id|$desc"
  grep -E "$pattern" "data/mutation-$id.txt" || echo "  (no matching case)"
}

mutation M1 "bwrap: drop the .git read-only bind" "10[ab]|10[efg]|10h" GIT_RO=off -- "$HERE/sbwrap.sh" m1
mutation M2 "bwrap: keep the network namespace shared" "^CASE\|5\|" NET=on -- "$HERE/sbwrap.sh" m2
mutation M3 "bwrap: do not clear the environment" "6a" CLEARENV=off -- "$HERE/sbwrap.sh" m3
# Removing BOTH ways the escape target is made visible: the point is that a sandboxed
# failure caused by "No such file or directory" proves nothing.
mutation M4 "bwrap: do not make the escape target visible at all" "^CASE\|1\|" BIND_ROOT=off "RO_EXTRA=$ROOT/ws/wt/.git" -- "$HERE/sbwrap.sh" m4
mutation M5 "landlock: drop the workspace writable rule" "^CASE\|9\|" ALLOW_WS=off -- "$HERE/llwrap.sh" m5
mutation M6 "landlock: drop the network denial" "^CASE\|5\|" DENY_NET=off -- "$HERE/llwrap.sh" m6
