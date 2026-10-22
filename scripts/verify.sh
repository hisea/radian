#!/usr/bin/env bash
# Run the full Verify of AGENTS.md in the current checkout.
#
# Order: the two binary builds, then every command of the AGENTS.md "Verify"
# block, then the three provider contract suites. Every step runs even after a
# failure; the exit code is 0 only when every step passed.
#
# Each step is one `run_step` line, in the order it runs. A docs drift test
# (crates/radian-tui/tests/docs_sync.rs) reads those lines and the Verify block
# of AGENTS.md and fails when they differ, so a command added to one needs the
# same change in the other.
#
# Usage: scripts/verify.sh [--log-dir <dir>]
set -uo pipefail

PROG="verify.sh"
LOG_DIR=".radian/verify"

usage() {
    cat <<EOF
usage: $PROG [--log-dir <dir>]

Runs the full Verify of AGENTS.md in the current checkout:
  cargo build -p radian-tui --bin radian --locked
  cargo build -p agent-extensions --bin mcp-fixture --locked
  every command of the AGENTS.md "Verify" block, in order
  the three provider contract suites

Every step runs even after a failure. Each step's output is kept in
<dir>/<n>-<command>.log (default: .radian/verify/). The exit code is 0 only
when every step passed.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --log-dir)
            [ $# -ge 2 ] || { usage >&2; exit 2; }
            LOG_DIR="$2"
            shift 2
            ;;
        --log-dir=*)
            LOG_DIR="${1#--log-dir=}"
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "$PROG: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    cd "$ROOT" || exit 1
fi
mkdir -p "$LOG_DIR" || exit 1

STEP=0
FAILED=0

# run_step ARG...: run one step with its output in a log, print its rc and
# elapsed seconds, and count a failure without stopping.
run_step() {
    STEP=$((STEP + 1))
    SLUG="$(printf '%s' "$*" | tr -cs 'A-Za-z0-9._-' '-' | sed 's/^-*//; s/-*$//' | cut -c1-60)"
    LOG="$(printf '%s/%02d-%s.log' "$LOG_DIR" "$STEP" "$SLUG")"
    printf '%s\n' "$*" >"$LOG"
    START="$(date +%s)"
    "$@" >>"$LOG" 2>&1
    RC=$?
    ELAPSED=$(( $(date +%s) - START ))
    printf 'rc=%s %ss %s\n' "$RC" "$ELAPSED" "$*"
    if [ "$RC" -ne 0 ]; then
        FAILED=$((FAILED + 1))
    fi
}

# The two binaries the integration tests run.
run_step cargo build -p radian-tui --bin radian --locked
run_step cargo build -p agent-extensions --bin mcp-fixture --locked

# The AGENTS.md "Verify" block, in its order.
run_step cargo fmt --all -- --check
run_step cargo clippy --workspace --all-targets -- -D warnings
run_step cargo nextest run --workspace --locked --no-fail-fast
run_step cargo nextest run -p radian-core --locked --features journal-selfcheck --no-fail-fast
run_step cargo clippy -p radian-core --all-targets --features journal-selfcheck -- -D warnings
run_step bash docs/schema/check-fixtures.sh
run_step bash docs/schema/check-config-fixtures.sh

# The provider contract suites.
run_step cargo test -p agent-provider-openai-compatible --features testing --test contract
run_step cargo test -p agent-provider-anthropic --features testing --test contract
run_step cargo test -p agent-provider-openai-responses --features testing --test contract

printf 'verify: %s step(s), %s failed; logs in %s\n' "$STEP" "$FAILED" "$LOG_DIR"
[ "$FAILED" -eq 0 ]
