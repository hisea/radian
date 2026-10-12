#!/usr/bin/env bash
# Landlock launcher: WS=<workspace> SANDBOX=on|off NET=off|on llwrap.sh <cmd> [args...]
set -euo pipefail
WS="${WS:?set WS}"
ROOT="${ROOT:-$(dirname "$WS")}"
SANDBOX="${SANDBOX:-on}"
NET="${NET:-off}"
DENY_NET="${DENY_NET:-on}"     # mutation knob
ALLOW_WS="${ALLOW_WS:-on}"     # mutation knob: off => no writable workspace rule
HERE="$(cd "$(dirname "$0")" && pwd)"
if [ "$SANDBOX" = off ]; then exec "$@"; fi
# Landlock confines reads too. git treats a *denied* ~/.config/git/config as fatal (not as
# missing), so HOME must point at an allowed, empty directory. Spawner-side mitigation,
# exactly like clearing the environment.
export HOME="$ROOT/empty-home"
# /dev/null must be writable: git opens it read-write (same finding as the macOS spike).
args=(--ro /usr --ro /bin --ro /lib --ro /etc --ro /proc --ro /dev --ro "$ROOT" --rw /dev/null)
[ "$ALLOW_WS" = on ] && args+=(--rw "$WS")
[ -e /lib64 ] && args+=(--ro /lib64)
[ "$NET" = off ] && [ "$DENY_NET" = on ] && args+=(--deny-net)
exec "$HERE/../bin/llsandbox" "${args[@]}" -- "$@"
