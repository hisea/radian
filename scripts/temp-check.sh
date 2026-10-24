#!/usr/bin/env bash
# The temp file system check shared by scripts/verify.sh and scripts/arc-gate.sh.
#
# Sourced, not run. The caller calls `radian_check_temp_fs <program-name>`
# before its first step. The check reads the file system that holds
# ${TMPDIR:-/tmp} with `df`: the free inodes and the free space. It fails with
# one line on stderr and return code 1 when fewer than
# RADIAN_VERIFY_MIN_FREE_INODES inodes (default 50000) or fewer than
# RADIAN_VERIFY_MIN_FREE_GIB GiB (default 2) are free; 0 turns that check off.
# Nothing is deleted. A file system that reports no inode count (some macOS
# volumes) skips the inode check and keeps the space check.
#
# `df -P -k` is parsed in both its GNU and its macOS shape, and `df -P -k -i`
# likewise: the inode columns are found by their header names, so a missing
# total (`df -i` on macOS has only iused/ifree) is iused + ifree.

# Print "<total> <free>" inodes of the file system holding $1, or nothing when
# `df` fails.
radian_temp_inodes() {
    LC_ALL=C df -P -k -i "$1" 2>/dev/null | awk '
        NR == 1 {
            for (i = 1; i <= NF; i++) {
                if ($i == "ifree" || $i == "IFree") free = i
                else if ($i == "iused" || $i == "IUsed") used = i
                else if ($i == "Inodes") total = i
            }
            next
        }
        {
            if (!free) { print "0 0"; exit }
            if (total) { print $total, $free } else { print $(used) + $free, $free }
            exit
        }'
}

# Print the free KiB of the file system holding $1, or nothing when `df` fails.
radian_temp_space_kib() {
    LC_ALL=C df -P -k "$1" 2>/dev/null | awk 'NR == 2 { print $4; exit }'
}

radian_check_temp_fs() {
    local prog="${1:-radian}"
    local path="${TMPDIR:-/tmp}"
    local inode_limit="${RADIAN_VERIFY_MIN_FREE_INODES:-50000}"
    local gib_limit="${RADIAN_VERIFY_MIN_FREE_GIB:-2}"

    case "$inode_limit" in
        *[!0-9]*)
            printf '%s: RADIAN_VERIFY_MIN_FREE_INODES must be a whole number (got %s)\n' \
                "$prog" "$inode_limit" >&2
            return 1
            ;;
    esac
    case "$gib_limit" in
        *[!0-9]*)
            printf '%s: RADIAN_VERIFY_MIN_FREE_GIB must be a whole number (got %s)\n' \
                "$prog" "$gib_limit" >&2
            return 1
            ;;
    esac
    # 10#: a leading zero must not read as octal.
    inode_limit=$((10#$inode_limit))
    gib_limit=$((10#$gib_limit))

    local inode_total="" inode_free=""
    if [ "$inode_limit" -ne 0 ]; then
        local pair
        pair="$(radian_temp_inodes "$path")"
        if [ -n "$pair" ]; then
            set -- $pair
            inode_total="$1"
            inode_free="$2"
        fi
        case "$inode_total" in
            ''|*[!0-9]*)
                printf '%s: cannot read the free inodes of %s (df failed); the inode check is skipped\n' \
                    "$prog" "$path" >&2
                inode_total=""
                inode_free=""
                ;;
        esac
    fi

    local space_kib=""
    if [ "$gib_limit" -ne 0 ]; then
        space_kib="$(radian_temp_space_kib "$path")"
        case "$space_kib" in
            ''|*[!0-9]*)
                printf '%s: cannot read the free space of %s (df failed); the space check is skipped\n' \
                    "$prog" "$path" >&2
                space_kib=""
                ;;
        esac
    fi

    local failed=0
    if [ "$inode_limit" -ne 0 ] && [ -n "$inode_total" ] && [ "$inode_total" -gt 0 ] \
        && [ "$inode_free" -lt "$inode_limit" ]; then
        failed=1
    fi
    if [ "$gib_limit" -ne 0 ] && [ -n "$space_kib" ] \
        && [ "$space_kib" -lt $((gib_limit * 1024 * 1024)) ]; then
        failed=1
    fi
    [ "$failed" -ne 0 ] || return 0

    local line=""
    if [ "$inode_limit" -ne 0 ]; then
        if [ -n "$inode_total" ] && [ "$inode_total" -gt 0 ]; then
            line="has $inode_free of $inode_total inodes free (limit $inode_limit)"
        else
            line="has no inode count"
        fi
    fi
    if [ "$gib_limit" -ne 0 ] && [ -n "$space_kib" ]; then
        local space_gib=$((space_kib / 1024 / 1024))
        if [ -n "$line" ]; then
            line="$line and $space_gib GiB free (limit $gib_limit GiB)"
        else
            line="has $space_gib GiB free (limit $gib_limit GiB)"
        fi
    fi
    printf '%s: %s %s; remove leaked test directories first\n' "$prog" "$path" "$line" >&2
    return 1
}
