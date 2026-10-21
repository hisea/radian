#!/usr/bin/env bash
# radian rendering prototype: prints the proposed transcript look with ANSI truecolor.
# Usage: bash render-prototype.sh [--today|--new] [--width 80] [--marks on|off] [--bold plain|warm]
#        [--code text|box] [--header plain|bold] [--rail on|off] [--numbers on|off] [--tint on|off]
# Run it in the terminal you use for radian (truecolor). See `--help` for each switch.
exec python3 -I "$(dirname "$0")/render-prototype.py" "$@"
