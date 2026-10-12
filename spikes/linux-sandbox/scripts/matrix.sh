#!/usr/bin/env bash
# Runs the 10 standard escape/use cases through one launcher.
#
#   WRAPPER=<path> SANDBOX=on|off [EXPECT_GIT_RO=on|off] matrix.sh <label>
#
# Prints one `CASE|<id>|<name>|expected=<fail|success>|actual=<fail|success>|<OK|MISMATCH>|detail`
# line per case, then a `SUMMARY|<label>|ok=<n>|mismatch=<n>` line. Exits non-zero on mismatch.
set -uo pipefail
[ "$(uname -s)" = Linux ] || { echo "REFUSING: not Linux"; exit 1; }
LABEL="${1:?usage: matrix.sh <label>}"
HERE="$(cd "$(dirname "$0")" && pwd)"
WRAPPER_ABS="$(cd "$(dirname "$WRAPPER")" && pwd)/$(basename "$WRAPPER")"
WRAPPER="${WRAPPER:?set WRAPPER}"
SANDBOX="${SANDBOX:-on}"
ROOT="${ROOT:-/tmp/radian-linux-sandbox}"
WS="$ROOT/ws"
OUTSIDE="$ROOT/outside"
EXPECT_GIT_RO="${EXPECT_GIT_RO:-on}"
PORT=47311
export WS ROOT OUTSIDE SANDBOX
NET="${NET:-off}"
export NET
export RO_EXTRA="${RO_EXTRA:-$OUTSIDE:$WS/wt/.git}"

# With the sandbox off, every escape case is expected to SUCCEED: that is the control that
# proves a sandboxed failure is a denial and not a missing path or a broken command.
if [ "$SANDBOX" = off ]; then ESCAPE=fail_expected_to_succeed; else ESCAPE=fail; fi
ok=0
mismatch=0
run() { "$WRAPPER" "$@"; }

record() { # record <id> <name> <expected> <actual> <detail>
  local id="$1" name="$2" expected="$3" actual="$4" detail="$5"
  if [ "$expected" = "$actual" ]; then
    printf 'CASE|%s|%s|expected=%s|actual=%s|OK|%s\n' "$id" "$name" "$expected" "$actual" "$detail"
    ok=$((ok + 1))
  else
    printf 'CASE|%s|%s|expected=%s|actual=%s|MISMATCH|%s\n' "$id" "$name" "$expected" "$actual" "$detail"
    mismatch=$((mismatch + 1))
  fi
}

# try <expected> <id> <name> -- <cmd...> ; runs and records fail/success
try() {
  local expected="$1" id="$2" name="$3"; shift 3
  [ "$1" = "--" ] && shift
  local detail
  if detail="$(run "$@" 2>&1)"; then
    record "$id" "$name" "$expected" success "${detail:0:160}"
  else
    record "$id" "$name" "$expected" fail "${detail:0:160}"
  fi
}

# --- 1..4, 7: write escapes -------------------------------------------------
try "${ESCAPE/fail_expected_to_succeed/success}" 1 abs-write-outside -- /bin/sh -c "echo x > $OUTSIDE/abs.txt"
try "${ESCAPE/fail_expected_to_succeed/success}" 2 dotdot-write-outside -- /bin/sh -c "cd $WS && echo x > ../outside/dotdot.txt"
try "${ESCAPE/fail_expected_to_succeed/success}" 3 symlink-file-outside -- /bin/sh -c "cd $WS && echo x > leak.txt"
try "${ESCAPE/fail_expected_to_succeed/success}" 4 symlink-parent-outside -- /bin/sh -c "cd $WS && echo x > link_dir/new.txt"
try "${ESCAPE/fail_expected_to_succeed/success}" 7 nested-sh-outside -- /bin/sh -c "cd $WS && sh -c 'sh -c \"echo x > $OUTSIDE/nested.txt\"'"
try success 9 workspace-write-read -- /bin/sh -c "cd $WS && echo hi > in.txt && cat in.txt"

# --- 5: network -------------------------------------------------------------
# The listener runs on the host (outside the sandbox). A blocking FIFO read is the gate:
# the listener writes `ready` only after bind() succeeds.
READY_FIFO="$ROOT/listener.ready.fifo"
rm -f "$READY_FIFO"
mkfifo "$READY_FIFO"
python3 "$HERE/listener.py" "$PORT" >"$READY_FIFO" 2>/dev/null &
LISTENER_PID=$!
read -r _ready <"$READY_FIFO"
rm -f "$READY_FIFO"
try "${ESCAPE/fail_expected_to_succeed/success}" 5 connect-local-listener -- python3 -c "
import socket
s = socket.create_connection(('127.0.0.1', $PORT), timeout=3)
print(s.recv(4))
"
kill "$LISTENER_PID" 2>/dev/null
wait "$LISTENER_PID" 2>/dev/null

# --- 6: environment ---------------------------------------------------------
# The mechanism is the spawner (bwrap --clearenv + explicit --setenv), not the kernel;
# Landlock cannot do this at all. The check is on the output, not the exit status.
export FIXTURE_SECRET="leaked-secret-value"
export FIXTURE_INHERITED="passed-through"
export PASS_ENV=FIXTURE_INHERITED
if [ "$SANDBOX" = on ] && [ "${EXPECT_ENV_CLEARED:-on}" = on ]; then
  expected_secret="<unset>"
else
  expected_secret="leaked-secret-value"
fi
actual_secret="$(run /bin/sh -c 'echo ${FIXTURE_SECRET:-<unset>}' 2>&1)"
record 6a env-secret-hidden "$expected_secret" "$actual_secret" "$actual_secret"
actual_inherited="$(run /bin/sh -c 'echo ${FIXTURE_INHERITED:-<unset>}' 2>&1)"
record 6b env-whitelist-visible "passed-through" "$actual_inherited" "$actual_inherited"

# --- 8: MCP stdio -----------------------------------------------------------
# The handshake must work inside the sandbox, and the MCP child must be unable to write
# outside. `mcp_check.py` reports what the server did.
expected_outside=success
[ "$SANDBOX" = on ] && expected_outside=fail
# The MCP *client* runs unsandboxed; only the server it launches is sandboxed.
if out="$(python3 "$HERE/mcp_check.py" "$WRAPPER_ABS" "$HERE/../src/mcp_echo.py" 2>&1)"; then
  record 8a mcp-handshake success success "$out"
  if [[ "$out" == *"outside_write_succeeded=False"* ]]; then
    record 8b mcp-outside-write "$expected_outside" fail "$out"
  else
    record 8b mcp-outside-write "$expected_outside" success "$out"
  fi
else
  record 8a mcp-handshake success fail "$out"
  record 8b mcp-outside-write "$expected_outside" fail "handshake failed"
fi

# --- 10: shared .git read-only ---------------------------------------------
# Read-only git first: under a mechanism that cannot protect `.git` (Landlock) the write
# cases below corrupt `.git/config`, which would otherwise make these fail for the wrong
# reason.
git_expected=fail
[ "$EXPECT_GIT_RO" = off ] && git_expected=success
[ "$SANDBOX" = off ] && git_expected=success
try success 10i git-status-read -- git -C "$WS" --no-optional-locks status --porcelain
try success 10j git-log-read -- git -C "$WS" --no-optional-locks log --oneline
try "$git_expected" 10e git-add-index-lock -- git -C "$WS" add tracked.txt
try "$git_expected" 10d git-hookspath -- git -C "$WS" config core.hooksPath /tmp
try "$git_expected" 10a git-hooks-write -- /bin/sh -c "cd $WS && mkdir -p .git/hooks && echo x > .git/hooks/evil"
try "$git_expected" 10b git-config-write -- /bin/sh -c "cd $WS && echo x >> .git/config"
try "$git_expected" 10c git-info-write -- /bin/sh -c "cd $WS && mkdir -p .git/info && echo x > .git/info/evil"
try "$git_expected" 10f worktree-pointer -- /bin/sh -c "echo x > $WS/wt/.git"
try "$git_expected" 10g worktree-gitdir -- /bin/sh -c "echo x > $WS/.git/worktrees/wt/gitdir"
try "$git_expected" 10h worktree-symlink -- /bin/sh -c "echo x > $WS/wt/gitlink/config"

printf 'SUMMARY|%s|ok=%s|mismatch=%s\n' "$LABEL" "$ok" "$mismatch"
[ "$mismatch" -eq 0 ]
