#!/usr/bin/env bash
# Combination: bwrap outside (mount/net/env) and Landlock inside (read confinement).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
# Control mode must be genuinely unsandboxed.
if [ "${SANDBOX:-on}" = off ]; then
  exec env SANDBOX=off "$HERE/llwrap.sh" "$@"
fi
# bwrap clears the environment, so the inner wrapper needs WS/ROOT passed through.
exec env SANDBOX=on PASS_ENVS=WS:ROOT "$HERE/sbwrap.sh" "$HERE/llwrap.sh" "$@"
