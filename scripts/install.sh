#!/bin/sh
# Install radian from a local release tarball or a URL.
# Never needs sudo; never deletes anything outside the install directory.
#
# Usage: sh scripts/install.sh [--prefix DIR] <tarball-path-or-URL>
#
# Installs `radian` and `rad` into DIR (default ~/.local/bin), checks the
# tarball against a SHA256SUMS next to it when present, prints a PATH hint
# when needed, and points at the next step (`radian login`).
set -eu

PREFIX="$HOME/.local/bin"
ARG=""

usage() {
    echo "usage: install.sh [--prefix DIR] <tarball-path-or-URL>" >&2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || { usage; exit 2; }
            PREFIX="$2"
            shift 2
            ;;
        --prefix=*)
            PREFIX="${1#--prefix=}"
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
            echo "install.sh: unknown option $1" >&2
            usage
            exit 2
            ;;
        *)
            if [ -n "${ARG}" ]; then
                echo "install.sh: only one tarball argument" >&2
                usage
                exit 2
            fi
            ARG="$1"
            shift
            ;;
    esac
done

if [ -z "${ARG}" ]; then
    usage
    exit 2
fi

TMPDIR="$(mktemp -d)"
trap 'rm -rf "${TMPDIR}"' EXIT INT TERM

TARBALL=""
SUMFILE=""

case "${ARG}" in
    http://*|https://*)
        echo "install.sh: downloading ${ARG}"
        curl -fsSL -o "${TMPDIR}/bundle.tar.gz" "${ARG}"
        TARBALL="${TMPDIR}/bundle.tar.gz"
        # A SHA256SUMS next to the URL verifies the download when present.
        if curl -fsSL -o "${TMPDIR}/SHA256SUMS" "${ARG%/*}/SHA256SUMS" 2>/dev/null; then
            SUMFILE="${TMPDIR}/SHA256SUMS"
        fi
        ;;
    *)
        if [ ! -f "${ARG}" ]; then
            echo "install.sh: no such file: ${ARG}" >&2
            exit 1
        fi
        case "${ARG}" in
            /*) TARBALL="${ARG}" ;;
            *) TARBALL="$PWD/${ARG}" ;;
        esac
        CANDIDATE="$(dirname "${TARBALL}")/SHA256SUMS"
        if [ -f "${CANDIDATE}" ]; then
            SUMFILE="${CANDIDATE}"
        fi
        ;;
esac

if [ -n "${SUMFILE}" ]; then
    BASENAME="$(basename "${TARBALL}")"
    # For a download the tarball is bundle.tar.gz, so match any single entry.
    if [ "${BASENAME}" = "bundle.tar.gz" ]; then
        ENTRIES="$(grep -c ' .*\.tar\.gz$' "${SUMFILE}" || true)"
        if [ "${ENTRIES}" = "1" ]; then
            EXPECTED="$(awk '{print $1}' "${SUMFILE}")"
            ACTUAL="$(shasum -a 256 "${TARBALL}" | awk '{print $1}')"
            if [ "${EXPECTED}" != "${ACTUAL}" ]; then
                echo "install.sh: SHA256 mismatch for ${ARG}" >&2
                echo "  expected: ${EXPECTED}" >&2
                echo "  actual:   ${ACTUAL}" >&2
                exit 1
            fi
            echo "install.sh: SHA256 verified."
        else
            echo "install.sh: SHA256SUMS has ${ENTRIES} entries; skipping check." >&2
        fi
    else
        LINE="$(grep -F "  ${BASENAME}" "${SUMFILE}" || true)"
        if [ -n "${LINE}" ]; then
            EXPECTED="$(printf '%s\n' "${LINE}" | awk '{print $1}')"
            ACTUAL="$(shasum -a 256 "${TARBALL}" | awk '{print $1}')"
            if [ "${EXPECTED}" != "${ACTUAL}" ]; then
                echo "install.sh: SHA256 mismatch for ${BASENAME}" >&2
                echo "  expected: ${EXPECTED}" >&2
                echo "  actual:   ${ACTUAL}" >&2
                exit 1
            fi
            echo "install.sh: SHA256 verified."
        else
            echo "install.sh: no entry for ${BASENAME} in ${SUMFILE}; skipping check." >&2
        fi
    fi
else
    echo "install.sh: no SHA256SUMS next to the tarball; skipping check." >&2
fi

tar -xzf "${TARBALL}" -C "${TMPDIR}"
for bin in radian rad; do
    if [ ! -f "${TMPDIR}/${bin}" ]; then
        echo "install.sh: tarball has no ${bin} binary" >&2
        exit 1
    fi
done

mkdir -p "${PREFIX}"
cp "${TMPDIR}/radian" "${TMPDIR}/rad" "${PREFIX}/"
chmod +x "${PREFIX}/radian" "${PREFIX}/rad"
# Best effort: a downloaded bundle may carry the macOS quarantine flag.
if command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "${PREFIX}/radian" 2>/dev/null || true
    xattr -d com.apple.quarantine "${PREFIX}/rad" 2>/dev/null || true
fi

echo "install.sh: installed radian and rad into ${PREFIX}"
case ":$PATH:" in
    *":${PREFIX}:"*) ;;
    *)
        echo "install.sh: ${PREFIX} is not on your PATH; add this and restart your shell:"
        echo "  export PATH=\"${PREFIX}:\$PATH\""
        ;;
esac
"${PREFIX}/radian" --version
echo "next step: ${PREFIX}/radian login   (see README.md in the bundle)"
