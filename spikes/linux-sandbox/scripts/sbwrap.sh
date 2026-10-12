#!/usr/bin/env bash
# bubblewrap launcher for the Linux sandbox spike.
#
#   WS=<workspace> SANDBOX=on|off NET=off|on CLEARENV=on|off PASS_ENV=<name> \
#   RO_EXTRA=<colon-separated paths> sbwrap.sh <cmd> [args...]
#
# SANDBOX=off is the control for every "escape must fail" case: the same command runs
# without bubblewrap, so a failure cannot be explained by a missing file.
set -euo pipefail
WS="${WS:?set WS to the workspace path}"
# ROOT is bound read-only so escape targets are *visible* (a denial, not a missing path).
ROOT="${ROOT:-$(dirname "$WS")}"
SANDBOX="${SANDBOX:-on}"
NET="${NET:-off}"
CLEARENV="${CLEARENV:-on}"
PASS_ENV="${PASS_ENV:-}"
PASS_ENVS="${PASS_ENVS:-}"   # colon-separated names passed into the sandbox (for nesting)
RO_EXTRA="${RO_EXTRA:-}"
GIT_RO="${GIT_RO:-on}"       # mutation knob: off => .git stays writable
BIND_ROOT="${BIND_ROOT:-on}" # mutation knob: off => escape targets are not visible

if [ "$SANDBOX" = off ]; then
  exec "$@"
fi

args=(--die-with-parent --new-session)
if [ "$NET" = off ]; then
  args+=(--unshare-all)
else
  args+=(--unshare-user --unshare-ipc --unshare-pid --unshare-uts --unshare-cgroup)
fi
# Read-only system tree; the workspace is the only writable place.
args+=(--ro-bind /usr /usr --ro-bind /bin /bin --ro-bind /lib /lib)
[ -e /lib64 ] && args+=(--ro-bind /lib64 /lib64)
args+=(--ro-bind /etc /etc)
args+=(--proc /proc --dev /dev --tmpfs /tmp --tmpfs /run)
# The spike root read-only (so `..`/absolute/symlink targets exist but are not writable),
# then the workspace writable on top of it, then read-only repository metadata.
[ "$BIND_ROOT" = on ] && args+=(--ro-bind "$ROOT" "$ROOT")
args+=(--bind "$WS" "$WS")
[ "$GIT_RO" = on ] && [ -e "$WS/.git" ] && args+=(--ro-bind "$WS/.git" "$WS/.git")
if [ -n "$RO_EXTRA" ]; then
  IFS=: read -ra extras <<<"$RO_EXTRA"
  for p in "${extras[@]}"; do
    [ -n "$p" ] && args+=(--ro-bind-try "$p" "$p")
  done
fi
args+=(--dir /sandbox-home --chdir "$WS")
if [ "$CLEARENV" = on ]; then
  args+=(--clearenv)
fi
args+=(--setenv PATH /usr/bin:/bin --setenv HOME /sandbox-home --setenv LANG C)
if [ -n "$PASS_ENV" ]; then
  args+=(--setenv "$PASS_ENV" "${!PASS_ENV}")
fi
if [ -n "$PASS_ENVS" ]; then
  IFS=: read -ra names <<<"$PASS_ENVS"
  for n in "${names[@]}"; do
    [ -n "$n" ] && args+=(--setenv "$n" "${!n}")
  done
fi
exec bwrap "${args[@]}" "$@"
