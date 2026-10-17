#!/bin/sh
# ADR 0034: the "Start radian here" action.
#
# herdr runs a plugin command with the caller's context in the environment (HERDR_BIN_PATH,
# HERDR_PANE_ID), so this starts radian in the pane the action was invoked from.
set -eu

exec "$HERDR_BIN_PATH" pane run "$HERDR_PANE_ID" radian
