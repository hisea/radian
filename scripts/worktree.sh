#!/usr/bin/env bash
# Make a git worktree with a warm Cargo target/ cloned from a still tree.
#
# Usage:
#   scripts/worktree.sh --from SRC <path> [<branch-or-base> | -b <branch> ...]
#   scripts/worktree.sh --drop-target <worktree-path>
#
# SRC is a warm checkout (or $RADIAN_WARM_TARGET_FROM); --from wins. There is
# no built-in default: a wrong cache is worse than no cache.
#
# The first form runs `git worktree add` with the arguments it is given, then
# clones SRC/target/ into the new worktree: `cp -Rc` (APFS clone) on macOS,
# `cp -a --reflink=auto` (copy where the filesystem has no reflink) on Linux.
# It refuses when the source tree is building (its Cargo build lock is held)
# unless --force is passed. The second form deletes only <worktree>/target/.
set -euo pipefail

PROG="worktree.sh"

usage() {
    echo "usage: $PROG --from SRC <path> [<branch-or-base> | -b <branch> ...]" >&2
    echo "       $PROG --drop-target <worktree-path>" >&2
    echo "       SRC may come from \$RADIAN_WARM_TARGET_FROM instead of --from." >&2
}

FROM="${RADIAN_WARM_TARGET_FROM:-}"
FORCE=0
MODE="add"

while [ $# -gt 0 ]; do
    case "$1" in
        --from)
            [ $# -ge 2 ] || { usage; exit 2; }
            FROM="$2"
            shift 2
            ;;
        --from=*)
            FROM="${1#--from=}"
            shift
            ;;
        --drop-target)
            MODE="drop"
            shift
            break
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "$PROG: unknown option $1" >&2
            usage
            exit 2
            ;;
        *)
            break
            ;;
    esac
done

# Elapsed milliseconds between two `now_ms` readings.
now_ms() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import time; print(int(time.time() * 1000))'
    else
        echo $((SECONDS * 1000))
    fi
}

# lock_held PATH: 0 when another process holds an flock on PATH (a Cargo
# build in that target dir), 1 when the lock is free or the file is absent
# (no build created it), 2 when there is no probe available at all.
lock_held() {
    [ -e "$1" ] || return 1
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$1" <<'PYEOF'
import fcntl, sys
try:
    fh = open(sys.argv[1], "a+b")
except OSError:
    sys.exit(2)
try:
    fcntl.flock(fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError:
    sys.exit(0)
sys.exit(1)
PYEOF
    elif command -v perl >/dev/null 2>&1; then
        perl -MFcntl=:flock -e '
            open(my $fh, "+<", $ARGV[0]) or exit(2);
            flock($fh, LOCK_EX | LOCK_NB) or exit(0);
            exit(1);' "$1"
    else
        return 2
    fi
}

# Refuse when the source tree is mid-build: cloning target/ under a running
# rustc risks a half-written artifact set in the new worktree.
assert_still() {
    src="$1"
    busy=0
    for lock in "$src/target/.cargo-lock" \
                "$src/target/debug/.cargo-lock" \
                "$src/target/release/.cargo-lock"; do
        rc=0
        lock_held "$lock" || rc=$?
        case $rc in
            0)
                echo "$PROG:   locked: $lock" >&2
                busy=1
                ;;
            2)
                echo "$PROG: no python3 or perl to probe $lock; refusing." >&2
                echo "$PROG: pass --force if the source tree is really still." >&2
                exit 1
                ;;
        esac
    done
    if [ "$busy" -eq 1 ]; then
        echo "$PROG: refusing: a build holds the lock under $src (use --force to override)." >&2
        exit 1
    fi
}

if [ "$MODE" = "drop" ]; then
    [ $# -eq 1 ] || { usage; exit 2; }
    WT="$1"
    case "$WT" in
        ""|"/"|"."|"./") echo "$PROG: refusing to drop the target of '$WT'." >&2; exit 1 ;;
    esac
    # Only a git worktree root has a .git pointer; anything else is a typo.
    if [ ! -e "$WT/.git" ]; then
        echo "$PROG: refusing: $WT has no .git, not a worktree root." >&2
        exit 1
    fi
    TARGET="$WT/target"
    if [ -L "$TARGET" ]; then
        echo "$PROG: refusing: $TARGET is a symlink; remove it by hand." >&2
        exit 1
    fi
    if [ ! -d "$TARGET" ]; then
        echo "$PROG: nothing to drop: $TARGET is not a directory." >&2
        exit 0
    fi
    rm -rf "$TARGET"
    echo "$PROG: dropped $TARGET (the worktree itself is untouched)."
    exit 0
fi

# Add mode: remaining arguments go to `git worktree add` untouched.
[ $# -ge 1 ] || { usage; exit 2; }
if [ -z "$FROM" ]; then
    echo "$PROG: no warm source: pass --from SRC or set RADIAN_WARM_TARGET_FROM." >&2
    exit 2
fi
if [ ! -d "$FROM/target" ]; then
    echo "$PROG: no warm source: $FROM/target is not a directory." >&2
    exit 1
fi
if [ "$FORCE" -eq 0 ]; then
    assert_still "$FROM"
fi

SCRATCH="$(mktemp -d)"
trap 'rm -rf "${SCRATCH}"' EXIT INT TERM
git worktree list --porcelain | grep '^worktree ' | sort > "$SCRATCH/before"
git worktree add "$@"
git worktree list --porcelain | grep '^worktree ' | sort > "$SCRATCH/after"
NEW="$(comm -13 "$SCRATCH/before" "$SCRATCH/after" | sed 's/^worktree //')"
if [ -z "$NEW" ] || [ "$(printf '%s\n' "$NEW" | wc -l)" -ne 1 ]; then
    echo "$PROG: cannot tell which worktree was added; clone target/ by hand." >&2
    exit 1
fi
if [ -e "$NEW/target" ]; then
    echo "$PROG: $NEW/target already exists; drop it first:" >&2
    echo "$PROG:   scripts/worktree.sh --drop-target $NEW" >&2
    exit 1
fi

case "$(uname -s)" in
    Darwin) CLONE="cp -Rc" ;;
    Linux)  CLONE="cp -a --reflink=auto" ;;
    *) echo "$PROG: unknown platform $(uname -s); trying cp -a --reflink=auto." >&2
       CLONE="cp -a --reflink=auto" ;;
esac

START="$(now_ms)"
# shellcheck disable=SC2086
$CLONE "$FROM/target" "$NEW/target"
END="$(now_ms)"
ELAPSED_MS=$((END - START))
echo "$PROG: worktree at $NEW (git worktree add $*)."
echo "$PROG: cloned $FROM/target -> $NEW/target ($CLONE) in ${ELAPSED_MS} ms."
