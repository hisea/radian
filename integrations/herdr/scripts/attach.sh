#!/bin/sh
# ADR 0034: the "Radian: attach" pane.
#
# A herdr plugin pane runs a fixed argv, so the session id arrives in the environment:
#
#   herdr plugin pane open --plugin radian --entrypoint attach --env RADIAN_ATTACH_SESSION=<id>
#
# Without it the pane says so instead of attaching to a session named "".
set -eu

if [ -z "${RADIAN_ATTACH_SESSION:-}" ]; then
    echo "radian attach: set RADIAN_ATTACH_SESSION to the session id, e.g."
    echo "  herdr plugin pane open --plugin radian --entrypoint attach --env RADIAN_ATTACH_SESSION=<id>"
    exit 1
fi

exec radian attach "$RADIAN_ATTACH_SESSION" --follow
