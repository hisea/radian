#!/bin/sh
# ADR 0034: the "Start radian in a new worktree" action.
#
# `herdr worktree create` creates the worktree workspace and opens it; the command then starts
# radian in that workspace's pane. The workspace and its pane are read back from herdr (the create
# answer carries the workspace, `pane list` carries its pane) because a plugin pane runs a fixed
# argv and has no argument of its own.
#
# Only `$HERDR_BIN_PATH` and `radian` are executed; the JSON is read with shell parameter
# expansion (decision 4).
set -eu

# $1: the JSON text; $2: the field name. Prints the first string value, or nothing.
json_string() {
    text=$1
    key=$2
    prefix=${text%%\"$key\"*}
    [ "$prefix" != "$text" ] || return 0
    rest=${text#*\"$key\"}
    rest=${rest#*:}
    rest=${rest#*\"}
    printf '%s' "${rest%%\"*}"
}

answer=$("$HERDR_BIN_PATH" worktree create --focus)
workspace=$(json_string "$answer" workspace_id)
[ -n "$workspace" ] || workspace=$(json_string "$answer" workspace)
[ -n "$workspace" ] || exit 0

pane=$("$HERDR_BIN_PATH" pane list --workspace "$workspace")
pane=$(json_string "$pane" pane_id)
[ -n "$pane" ] || exit 0

exec "$HERDR_BIN_PATH" pane run "$pane" radian
