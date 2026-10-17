#!/bin/sh
# ADR 0034: the optional `worktree.created` hook, **off by default**.
#
# It does nothing unless this plugin's own config file turns it on:
#
#   dir=$(herdr plugin config-dir radian); mkdir -p "$dir"; echo 'start_worktree = true' > "$dir/radian.toml"
#
# With it on, every worktree created from this session gets radian started in its pane.
#
# Only `$HERDR_BIN_PATH` and `radian` are executed; the config and the event JSON are read with
# shell parameter expansion (decision 4).
set -eu

config="${HERDR_PLUGIN_CONFIG_DIR:-}/radian.toml"
[ -f "$config" ] || exit 0
enabled=0
while IFS= read -r line || [ -n "$line" ]; do
    case $line in
        *start_worktree*=*true*) enabled=1 ;;
    esac
done <"$config"
[ "$enabled" = 1 ] || exit 0

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

# The event's own workspace: `worktree.created` fires with the created workspace in its payload.
workspace=$(json_string "${HERDR_PLUGIN_EVENT_JSON:-}" workspace_id)
[ -n "$workspace" ] || workspace=$(json_string "${HERDR_PLUGIN_EVENT_JSON:-}" workspace)
[ -n "$workspace" ] || exit 0

pane=$("$HERDR_BIN_PATH" pane list --workspace "$workspace")
pane=$(json_string "$pane" pane_id)
[ -n "$pane" ] || exit 0

exec "$HERDR_BIN_PATH" pane run "$pane" radian
