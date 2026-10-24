#!/usr/bin/env bash
# The arc gate for this repository.
#
# Usage:
#   scripts/arc-gate.sh quick
#   scripts/arc-gate.sh full
#
# quick: cargo fmt, workspace clippy, the two binaries, then nextest for every
# crate under crates/ that the checkout changed since its merge base with
# origin/main, plus radian-tui (its integration tests exercise the rest). It
# stops at the first failing step.
#
# full: scripts/verify.sh here, or on ARC_GATE_HOST when that is set.
#
# Trust: arc runs a gate's `commands` in a checkout of the task's own commit, so
# a script read from that checkout would let a task change its own gate. The arc
# configuration must therefore call this script by the *main checkout's absolute
# path* (/abs/path/to/repo/scripts/arc-gate.sh), never a relative `scripts/...`
# path. The same rule is in docs/user/arc-gate.md. full runs the verify.sh next to
# this script, so the command list comes from the trusted checkout too.
#
# CARGO_TARGET_DIR is the caller's business: this script never sets it.
set -uo pipefail

PROG="arc-gate.sh"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
. "$SCRIPT_DIR/temp-check.sh"

usage() {
    cat <<EOF
usage: $PROG quick
       $PROG full

quick  cargo fmt, workspace clippy, the radian and mcp-fixture builds, then
       nextest for every crate this checkout changed since its merge base with
       origin/main, plus radian-tui. Stops at the first failure.

full   scripts/verify.sh (the full Verify of AGENTS.md). With ARC_GATE_HOST
       set, runs it on that ssh host instead:
         ARC_GATE_HOST  the ssh host, unset by default (run here)
         ARC_GATE_TREE  the gate checkout on that host, a path relative to the
                        remote home; required when ARC_GATE_HOST is set
       The host receives HEAD as a git bundle, takes a blocking lock on
       ~/.arc-gate.lock, checks HEAD out detached inside ARC_GATE_TREE, and
       runs that checkout's scripts/verify.sh.
EOF
}

# The repository root of the current checkout (not of this script: full runs
# against the checkout the gate is verifying, which arc may have made elsewhere).
enter_root() {
    if ! ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"; then
        echo "$PROG: not inside a git working tree" >&2
        exit 1
    fi
    cd "$ROOT" || exit 1
}

# Print one crate name per line: the crates under crates/ that the working tree
# or HEAD changed since the merge base with origin/main.
changed_crates() {
    if ! BASE="$(git merge-base HEAD refs/remotes/origin/main 2>/dev/null)"; then
        echo "$PROG: no merge base with origin/main; fetch it first" >&2
        return 1
    fi
    git diff --name-only "$BASE" -- crates \
        | sed -n 's|^crates/\([^/][^/]*\)/.*$|\1|p' \
        | sort -u
}

# Run one quick step and stop the gate at its exit code when it fails.
quick_step() {
    printf '+ %s\n' "$*"
    "$@"
    RC=$?
    if [ "$RC" -ne 0 ]; then
        echo "$PROG: step failed (rc=$RC): $*" >&2
        exit "$RC"
    fi
}

# Print the name of every failing test in a verify log directory.
report_failures() {
    for LOG in "$1"/*.log; do
        [ -e "$LOG" ] || continue
        grep -E '^[[:space:]]*FAIL[[:space:]]+\[' "$LOG" \
            | sed -E 's/^[[:space:]]*FAIL[[:space:]]+\[[^]]*\][[:space:]]*//'
        grep -E '^test .* \.\.\. FAILED$' "$LOG" \
            | sed -E 's/^test (.*) \.\.\. FAILED$/\1/'
    done | while IFS= read -r NAME; do
        printf '%s: failing test: %s\n' "$PROG" "$NAME"
    done
}

quick() {
    enter_root
    # Before any step: a temp file system out of inodes or space fails a
    # thousand unrelated tests in milliseconds, so fail once, here.
    radian_check_temp_fs "$PROG" || exit 1
    CRATES="$(changed_crates)" || exit 1
    case " $(printf '%s' "$CRATES" | tr '\n' ' ') " in
        *" radian-tui "*) ;;
        *) CRATES="$(printf '%s\n%s' "$CRATES" 'radian-tui')" ;;
    esac
    ARGS=()
    for CRATE in $CRATES; do
        ARGS+=(-p "$CRATE")
    done
    printf '%s: nextest crates: %s\n' "$PROG" "$(printf '%s' "$CRATES" | tr '\n' ' ')"

    quick_step cargo fmt --all -- --check
    quick_step cargo clippy --workspace --all-targets -- -D warnings
    quick_step cargo build -p radian-tui --bin radian --locked
    quick_step cargo build -p agent-extensions --bin mcp-fixture --locked
    quick_step cargo nextest run --locked --no-fail-fast "${ARGS[@]}"
}

# The remote half of `full`: run from the ssh host with the script on stdin.
# $1 the checkout (relative to the remote home), $2 the sha, $3 the bundle path
# (relative to the remote home, empty when HEAD is already in origin/main).
remote_script() {
    cat <<'REMOTE'
set -uo pipefail
TREE="$1"
SHA="$2"
NAME="$3"

# One gate at a time, and wait for it instead of failing.
if command -v flock >/dev/null 2>&1; then
    exec 9>"$HOME/.arc-gate.lock"
    flock 9
else
    while ! mkdir "$HOME/.arc-gate.lock.d" 2>/dev/null; do sleep 2; done
    trap 'rmdir "$HOME/.arc-gate.lock.d" 2>/dev/null' EXIT
fi

BUNDLE=""
if [ -n "$NAME" ]; then
    BUNDLE="$HOME/$NAME"
fi

cd "$TREE" || { echo "arc-gate: cannot enter $TREE" >&2; exit 1; }
if [ -n "$BUNDLE" ]; then
    git fetch --quiet "$BUNDLE" '+HEAD:refs/arc-gate/candidate' \
        || { echo "arc-gate: cannot fetch the bundle" >&2; exit 1; }
    rm -f "$BUNDLE"
fi
if ! git cat-file -e "$SHA^{commit}" 2>/dev/null; then
    git fetch --quiet origin || { echo "arc-gate: cannot fetch origin" >&2; exit 1; }
fi
git checkout --force --detach "$SHA" \
    || { echo "arc-gate: cannot check out $SHA" >&2; exit 1; }
git clean -fdq || true

LOG_DIR="$TREE/.radian/verify-full"
rm -rf "$LOG_DIR"
bash "$TREE/scripts/verify.sh" --log-dir "$LOG_DIR"
RC=$?

for LOG in "$LOG_DIR"/*.log; do
    [ -e "$LOG" ] || continue
    grep -E '^[[:space:]]*FAIL[[:space:]]+\[' "$LOG" \
        | sed -E 's/^[[:space:]]*FAIL[[:space:]]+\[[^]]*\][[:space:]]*//'
    grep -E '^test .* \.\.\. FAILED$' "$LOG" \
        | sed -E 's/^test (.*) \.\.\. FAILED$/\1/'
done | while IFS= read -r TEST; do
    printf 'arc-gate: failing test: %s\n' "$TEST"
done

exit "$RC"
REMOTE
}

# Quote one word for the remote shell of `ssh host "bash -s -- ..."`.
quote() {
    printf '%q' "$1"
}

full() {
    enter_root
    radian_check_temp_fs "$PROG" || exit 1

    if [ -z "${ARC_GATE_HOST:-}" ]; then
        # The trusted copy: the verify.sh next to this script, run on the
        # checkout the gate is verifying.
        LOG_DIR="$ROOT/.radian/verify-full"
        rm -rf "$LOG_DIR"
        "$SCRIPT_DIR/verify.sh" --log-dir "$LOG_DIR"
        RC=$?
        report_failures "$LOG_DIR"
        exit "$RC"
    fi

    HOST="$ARC_GATE_HOST"
    TREE="${ARC_GATE_TREE:-}"
    if [ -z "$TREE" ]; then
        echo "$PROG: ARC_GATE_HOST is set; ARC_GATE_TREE (the gate checkout on that host, relative to its home) is required" >&2
        exit 2
    fi

    if ! SHA="$(git rev-parse HEAD 2>/dev/null)"; then
        echo "$PROG: cannot read HEAD" >&2
        exit 1
    fi

    # HEAD --not origin/main: no bundle when HEAD is already in origin/main.
    BUNDLE=""
    SCRATCH=""
    if ! git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then
        echo "$PROG: no origin/main to compare with" >&2
        exit 1
    fi
    git merge-base --is-ancestor HEAD refs/remotes/origin/main
    case $? in
        0) BUNDLE="" ;;
        1)
            SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/arc-gate.XXXXXX")" || exit 1
            BUNDLE="$SCRATCH/head.bundle"
            if ! git bundle create "$BUNDLE" HEAD --not refs/remotes/origin/main; then
                echo "$PROG: cannot create the git bundle" >&2
                exit 1
            fi
            ;;
        *)
            echo "$PROG: cannot compare HEAD with origin/main" >&2
            exit 1
            ;;
    esac
    trap '[ -n "$SCRATCH" ] && rm -rf "$SCRATCH"' EXIT

    REMOTE_BUNDLE=""
    if [ -n "$BUNDLE" ]; then
        REMOTE_BUNDLE=".arc-gate/bundle.$$"
        # shellcheck disable=SC2029 # $REMOTE_BUNDLE is built here, on purpose
        if ! ssh "$HOST" "mkdir -p .arc-gate && cat > $REMOTE_BUNDLE" <"$BUNDLE"; then
            echo "$PROG: cannot send the bundle to $HOST" >&2
            exit 1
        fi
    fi

    REMOTE="$(mktemp "${TMPDIR:-/tmp}/arc-gate-remote.XXXXXX")" || exit 1
    trap '[ -n "$SCRATCH" ] && rm -rf "$SCRATCH"; rm -f "$REMOTE"' EXIT
    remote_script >"$REMOTE"

    # shellcheck disable=SC2029 # the arguments are quoted here, on purpose
    ssh "$HOST" "bash -s -- $(quote "$TREE") $(quote "$SHA") $(quote "$REMOTE_BUNDLE")" <"$REMOTE"
    exit $?
}

case "${1:-}" in
    quick)
        [ $# -eq 1 ] || { usage >&2; exit 2; }
        quick
        ;;
    full)
        [ $# -eq 1 ] || { usage >&2; exit 2; }
        full
        ;;
    -h|--help)
        usage
        exit 0
        ;;
    "")
        usage >&2
        exit 2
        ;;
    *)
        echo "$PROG: unknown argument: $1" >&2
        usage >&2
        exit 2
        ;;
esac
