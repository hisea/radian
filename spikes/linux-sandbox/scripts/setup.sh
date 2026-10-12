#!/usr/bin/env bash
# Creates a fresh spike root. Only writes under ROOT (default /tmp/radian-linux-sandbox).
set -euo pipefail
[ "$(uname -s)" = Linux ] || { echo "REFUSING: this spike only runs on Linux"; exit 1; }
ROOT="${ROOT:-/tmp/radian-linux-sandbox}"
WS="$ROOT/ws"
OUTSIDE="$ROOT/outside"
rm -rf "$WS" "$OUTSIDE" "$ROOT/listener.ready"
mkdir -p "$WS" "$OUTSIDE" "$ROOT/empty-home"
echo "SENTINEL-CONTENT" > "$OUTSIDE/sentinel.txt"
git -C "$WS" init -q
git -C "$WS" config user.email spike@example.com
git -C "$WS" config user.name Spike
echo tracked > "$WS/tracked.txt"
git -C "$WS" add tracked.txt
git -C "$WS" commit -q -m init
# A linked worktree inside the workspace: its `.git` is a pointer file into ws/.git.
if ! git -C "$WS" worktree add -q "$WS/wt" -b spike-wt 2>/dev/null; then
  git -C "$WS" worktree add -q "$WS/wt"
fi
[ -f "$WS/wt/.git" ] || { echo "setup failed: no worktree pointer at $WS/wt/.git"; exit 1; }
# Escape fixtures: a file link and a directory link, both pointing outside.
ln -sfn "$OUTSIDE/sentinel.txt" "$WS/leak.txt"
ln -sfn "$OUTSIDE" "$WS/link_dir"
# A link from inside the worktree to the shared .git.
ln -sfn "$WS/.git" "$WS/wt/gitlink"
echo "setup ok: WS=$WS OUTSIDE=$OUTSIDE"
