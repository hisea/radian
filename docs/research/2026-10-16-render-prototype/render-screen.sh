#!/usr/bin/env bash
# radian full-screen rendering prototype: every component in one unified look.
# Usage: bash render-screen.sh [--width 110] [--panel on|off] [--menu on|off]
#        [--modal none|approval|ask] [--input thin|bar|lines] [--marks on|off] [--numbers on|off] [--today]
exec python3 -I "$(dirname "$0")/render-screen.py" "$@"
