#!/usr/bin/env bash
# The temp file system check shared by scripts/verify.sh and scripts/arc-gate.sh.
#
# Sourced, not run. The caller calls `radian_check_temp_fs <program-name>`
# before its first step. The check reads the file system that holds
# ${TMPDIR:-/tmp} with `df`: the free inodes and the free space. It fails with
# one line on stderr and return code 1 when fewer than
# RADIAN_VERIFY_MIN_FREE_INODES inodes (default 50000) or fewer than
# RADIAN_VERIFY_MIN_FREE_GIB GiB (default 2) are free; 0 turns that check off.
# Both limits are whole numbers of at most 18 digits, so no comparison below
# can overflow. Nothing is deleted. A file system that reports no usable inode
# count (some macOS volumes report none, `-`, or a number too large for `[`)
# skips the inode check and keeps the space check; a negative free space (GNU
# df on ext4 below the reserved-block count) fails the space check instead of
# being read as unreadable.
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
# GNU df prints a negative Available below the reserved-block count; that is a
# real value, so keep the minus sign.
radian_temp_space_kib() {
    LC_ALL=C df -P -k "$1" 2>/dev/null | awk 'NR == 2 { print $4; exit }'
}

# True when $1 is a whole number that `[` and `$(( ))` can compare: digits
# only, at most 18 of them once leading zeros are gone. A file system that
# reports `-` or a number this large (APFS `df -i` reports ifree as
# 9223372036854775807, which awk prints as 9223372036854775808) has no usable
# inode count.
radian_temp_is_count() {
    case "$1" in
        ''|*[!0-9]*) return 1 ;;
    esac
    local digits="$1"
    while [ "${digits#0}" != "$digits" ]; do digits="${digits#0}"; done
    [ "${#digits}" -le 18 ]
}

# Print $3 as the value of a limit, or fail with the reason. $1 is the
# program name, $2 the variable name. Leading zeros are stripped (a leading
# zero must not read as octal) and at most 18 digits remain, so the value fits
# below 2^63 - 1 and no later comparison can overflow.
radian_temp_limit() {
    local prog="$1" name="$2" value="$3"
    case "$value" in
        ''|*[!0-9]*)
            printf '%s: %s must be a whole number (got %s)\n' "$prog" "$name" "$value" >&2
            return 1
            ;;
    esac
    local digits="$value"
    while [ "${digits#0}" != "$digits" ]; do digits="${digits#0}"; done
    if [ "${#digits}" -gt 18 ]; then
        printf '%s: %s must be at most 999999999999999999 (got %s)\n' \
            "$prog" "$name" "$value" >&2
        return 1
    fi
    printf '%s\n' "${digits:-0}"
}

radian_check_temp_fs() {
    local prog="${1:-radian}"
    local path="${TMPDIR:-/tmp}"
    local inode_limit gib_limit

    inode_limit="$(radian_temp_limit "$prog" RADIAN_VERIFY_MIN_FREE_INODES \
        "${RADIAN_VERIFY_MIN_FREE_INODES:-50000}")" || return 1
    gib_limit="$(radian_temp_limit "$prog" RADIAN_VERIFY_MIN_FREE_GIB \
        "${RADIAN_VERIFY_MIN_FREE_GIB:-2}")" || return 1

    local inode_total="" inode_free=""
    if [ "$inode_limit" -ne 0 ]; then
        local pair
        pair="$(radian_temp_inodes "$path")"
        if [ -n "$pair" ]; then
            set -- $pair
            inode_total="$1"
            inode_free="$2"
            if ! radian_temp_is_count "$inode_total" \
                || ! radian_temp_is_count "$inode_free"; then
                # df answered, but with no usable count: `-`, or a number `[`
                # cannot compare. Some macOS volumes report that; skip
                # quietly, as the manual promises.
                inode_total=""
                inode_free=""
            fi
        else
            printf '%s: cannot read the free inodes of %s (df failed); the inode check is skipped\n' \
                "$prog" "$path" >&2
        fi
    fi

    local space_kib=""
    if [ "$gib_limit" -ne 0 ]; then
        local space_value sign=""
        space_value="$(radian_temp_space_kib "$path")"
        case "$space_value" in
            -*) sign="-"; space_value="${space_value#-}" ;;
        esac
        if radian_temp_is_count "$space_value"; then
            space_kib="$sign$space_value"
        else
            printf '%s: cannot read the free space of %s (df failed); the space check is skipped\n' \
                "$prog" "$path" >&2
            space_kib=""
        fi
    fi

    local failed=0
    if [ "$inode_limit" -ne 0 ] && [ -n "$inode_total" ] && [ "$inode_total" -gt 0 ] \
        && [ "$inode_free" -lt "$inode_limit" ]; then
        failed=1
    fi
    # Compare in MiB by division instead of building gib_limit * 1024 * 1024:
    # for whole numbers `space_kib / 1048576 < gib_limit` is exactly
    # `space_kib < gib_limit * 1048576`, and it cannot overflow.
    if [ "$gib_limit" -ne 0 ] && [ -n "$space_kib" ] \
        && [ $((space_kib / 1048576)) -lt "$gib_limit" ]; then
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
