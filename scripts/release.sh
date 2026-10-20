#!/bin/sh
# Build the shareable radian release bundle for the host this runs on.
# Local artifacts only: nothing is uploaded, published, tagged or pushed.
#
# Usage: sh scripts/release.sh
#
# Output (gitignored): dist/radian-<version>-<target>.tar.gz containing the
# `radian` and `rad` binaries (two separate binary targets sharing one entry
# point, not an argv[0] alias, so both are built and shipped), the user-facing
# quickstart as README.md, LICENSE-MIT, LICENSE-APACHE; plus dist/SHA256SUMS.
#
# Version is crates/radian-tui's version plus the short commit id
# (<version>+<sha>). Refuses to build from a dirty tree.
set -eu

cd "$(dirname "$0")/.."

if [ -n "$(git status --porcelain)" ]; then
    echo "release.sh: refusing to build from a dirty tree; commit first." >&2
    git status --porcelain >&2
    exit 1
fi

VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/radian-tui/Cargo.toml | head -n 1)"
SHA="$(git rev-parse --short HEAD)"
FULL_VERSION="${VERSION}+${SHA}"
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
DIST="dist"
STAGE="${DIST}/stage-${TARGET}"
TARBALL="${DIST}/radian-${FULL_VERSION}-${TARGET}.tar.gz"

if [ -z "${VERSION}" ] || [ -z "${SHA}" ] || [ -z "${TARGET}" ]; then
    echo "release.sh: could not determine version/sha/target." >&2
    exit 1
fi

echo "release.sh: radian ${FULL_VERSION} for ${TARGET}"
START="$(date +%s)"

export CARGO_TARGET_DIR="$PWD/target-release"
# Research-measured size wins (40.1 -> 24.6 MiB); environment only, Cargo.toml untouched.
export CARGO_PROFILE_RELEASE_LTO=fat
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
export CARGO_PROFILE_RELEASE_STRIP=symbols

cargo build --locked --release -p radian-tui --bin radian --bin rad

for bin in radian rad; do
    if [ ! -x "${CARGO_TARGET_DIR}/release/${bin}" ]; then
        echo "release.sh: missing ${CARGO_TARGET_DIR}/release/${bin}" >&2
        exit 1
    fi
done

# macOS: ad-hoc sign so the bundle runs on arm64; verify right away.
case "${TARGET}" in
    *-apple-darwin)
        codesign --force --sign - "${CARGO_TARGET_DIR}/release/radian" \
            "${CARGO_TARGET_DIR}/release/rad"
        codesign -v "${CARGO_TARGET_DIR}/release/radian" \
            "${CARGO_TARGET_DIR}/release/rad"
        ;;
esac

rm -rf "${STAGE}"
mkdir -p "${STAGE}" "${DIST}"
cp "${CARGO_TARGET_DIR}/release/radian" "${CARGO_TARGET_DIR}/release/rad" "${STAGE}/"
cp packaging/README.md "${STAGE}/README.md"
cp LICENSE-MIT LICENSE-APACHE "${STAGE}/"

tar -czf "${TARBALL}" -C "${STAGE}" radian rad README.md LICENSE-MIT LICENSE-APACHE
rm -rf "${STAGE}"

# SHA256SUMS covers every tarball in dist/.
(cd "${DIST}" && shasum -a 256 radian-*.tar.gz > SHA256SUMS)

END="$(date +%s)"
echo "release.sh: ${TARBALL}"
ls -l "${TARBALL}"
du -h "${CARGO_TARGET_DIR}/release/radian" "${CARGO_TARGET_DIR}/release/rad"
echo "release.sh: build+package took $((END - START))s"
echo "release.sh: file of radian:"
file "${CARGO_TARGET_DIR}/release/radian"
