#!/bin/sh
# macOS half of the radian release: build, sign, notarize, package and the
# Homebrew tap. CI (.github/workflows/release.yml) builds the two Linux musl
# targets and drafts the GitHub release; this script does everything Apple.
#
# Maintainer steps for a release:
#   1. Tag: git tag v<version> && git push origin v<version>
#   2. Wait for CI: the release.yml workflow builds the musl tars and creates
#      the release for the tag as a DRAFT.
#   3. On this Mac: sh scripts/release-macos.sh <version>      # dry run first
#                    sh scripts/release-macos.sh <version> --publish
#
# Environment:
#   RADIAN_SIGN_IDENTITY   codesigning identity. Default: the first
#                          "Developer ID Application" identity that
#                          `security find-identity -v -p codesigning` lists.
#                          Refuses when there is none. TEST ONLY: "-" selects
#                          an ad-hoc signature (refused with --publish).
#   RADIAN_NOTARY_PROFILE  notarytool keychain profile. Store it once with:
#                          xcrun notarytool store-credentials RADIAN \
#                            --apple-id <addr> --team-id <id> --password <app-pw>
#                          then export RADIAN_NOTARY_PROFILE=RADIAN.
#                          Unneeded with --skip-notarize (test only).
#
# Modes: the default is a dry run (everything except the outward steps in
# step 7). --publish uploads the macOS files, publishes the draft, checks
# every asset anonymously, and pushes Formula/radian.rb to hisea/homebrew-tap.
# --target <triple> builds one macOS target (default: both; test convenience).
# --skip-notarize and --no-release-check are TEST ONLY and are refused with
# --publish. A bare Mach-O cannot be stapled (stapler needs a container), so
# there is no staple step: Gatekeeper verifies the notarization ticket online.
#
# Usage: sh scripts/release-macos.sh <version> [--publish] [--target <triple>]
#        [--skip-notarize] [--no-release-check]
set -eu

VERSION="${1:-}"
shift || true
PUBLISH=0
TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
SKIP_NOTARIZE=0
NO_RELEASE_CHECK=0
while [ $# -gt 0 ]; do
    case "$1" in
        --publish) PUBLISH=1 ;;
        --target) TARGETS="$2"; shift ;;
        --skip-notarize) SKIP_NOTARIZE=1 ;;
        --no-release-check) NO_RELEASE_CHECK=1 ;;
        *) echo "release-macos.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

case "$VERSION" in
    ''|*[!0-9.]*)
        echo "release-macos.sh: version must be like 0.1.0, got '$VERSION'." >&2
        exit 2 ;;
esac
TAG="v$VERSION"
REPO="hisea/radian"
DIST="dist"

if [ "$PUBLISH" -eq 1 ]; then
    [ "$SKIP_NOTARIZE" -eq 0 ] || {
        echo "release-macos.sh: --publish refuses --skip-notarize." >&2; exit 1; }
    [ "$NO_RELEASE_CHECK" -eq 0 ] || {
        echo "release-macos.sh: --publish refuses --no-release-check." >&2; exit 1; }
fi

# 1. Gates: clean tree, on the tag, draft release exists.
if [ -n "$(git status --porcelain)" ]; then
    echo "release-macos.sh: refusing to release from a dirty tree." >&2
    git status --porcelain >&2
    exit 1
fi
if [ "$NO_RELEASE_CHECK" -eq 0 ]; then
    ON_TAG="$(git describe --exact-match --match 'v*' HEAD 2>/dev/null || true)"
    [ "$ON_TAG" = "$TAG" ] || {
        echo "release-macos.sh: HEAD is not $TAG (got '${ON_TAG:-nothing}')." >&2
        exit 1; }
    IS_DRAFT="$(gh release view "$TAG" --json isDraft -q .isDraft 2>/dev/null || true)"
    [ "$IS_DRAFT" = "true" ] || {
        echo "release-macos.sh: no draft release for $TAG yet." >&2; exit 1; }
fi

# 3. Signing identity.
IDENTITY="${RADIAN_SIGN_IDENTITY:-}"
if [ -z "$IDENTITY" ]; then
    IDENTITY="$(security find-identity -v -p codesigning \
        | sed -n 's/^ *[0-9]*) [0-9A-F]* "\(Developer ID Application: .*\)"$/\1/p' \
        | head -n 1)"
fi
[ -n "$IDENTITY" ] || {
    echo "release-macos.sh: no Developer ID Application identity; set RADIAN_SIGN_IDENTITY." >&2
    exit 1; }
ADHOC=0
if [ "$IDENTITY" = "-" ]; then
    ADHOC=1
    [ "$PUBLISH" -eq 0 ] || {
        echo "release-macos.sh: --publish refuses the ad-hoc signature." >&2
        exit 1; }
fi
if [ "$PUBLISH" -eq 1 ] && [ -z "${RADIAN_NOTARY_PROFILE:-}" ]; then
    echo "release-macos.sh: RADIAN_NOTARY_PROFILE is not set." >&2
    exit 1
fi

# 2. Build.
for target in $TARGETS; do
    rustup target list --installed | grep -qx "$target" \
        || rustup target add "$target"
    cargo build --locked --release -p radian-tui --bin radian --bin rad \
        --target "$target"
done

NOTARIZED_LIST=""
for target in $TARGETS; do
    RELEASE_BIN="target/$target/release"
    test -x "$RELEASE_BIN/radian" || {
        echo "release-macos.sh: missing $RELEASE_BIN/radian." >&2; exit 1; }
    test -x "$RELEASE_BIN/rad" || {
        echo "release-macos.sh: missing $RELEASE_BIN/rad." >&2; exit 1; }

    # 3. Sign.
    if [ "$ADHOC" -eq 1 ]; then
        codesign --force --sign - "$RELEASE_BIN/radian" "$RELEASE_BIN/rad"
    else
        codesign --force --options runtime --timestamp \
            --sign "$IDENTITY" "$RELEASE_BIN/radian" "$RELEASE_BIN/rad"
    fi
    codesign --verify --strict "$RELEASE_BIN/radian" "$RELEASE_BIN/rad"

    # 4. Notarize.
    if [ "$SKIP_NOTARIZE" -eq 0 ]; then
        [ -n "${RADIAN_NOTARY_PROFILE:-}" ] || {
            echo "release-macos.sh: RADIAN_NOTARY_PROFILE is not set." >&2
            exit 1; }
        for bin in radian rad; do
            zipfile="$DIST/notarize-$target-$bin.zip"
            mkdir -p "$DIST"
            rm -f "$zipfile"
            ditto -c -k --keepParent "$RELEASE_BIN/$bin" "$zipfile"
            echo "release-macos.sh: notarizing $target/$bin ..."
            SUBMIT_OUT="$(xcrun notarytool submit "$zipfile" \
                --keychain-profile "$RADIAN_NOTARY_PROFILE" --wait 2>&1)" || {
                echo "$SUBMIT_OUT" >&2
                echo "release-macos.sh: notarytool submit failed for $target/$bin." >&2
                exit 1; }
            echo "$SUBMIT_OUT" | grep -q "status: Accepted" || {
                echo "$SUBMIT_OUT" >&2
                echo "release-macos.sh: notarization not Accepted for $target/$bin." >&2
                exit 1; }
            rm -f "$zipfile"
        done
        NOTARIZED_LIST="$NOTARIZED_LIST $target"
    fi

    # 5. Package.
    stage="$DIST/stage-$target"
    tarball="$DIST/radian-$VERSION-$target.tar.gz"
    rm -rf "$stage"
    mkdir -p "$stage" "$DIST"
    cp "$RELEASE_BIN/radian" "$RELEASE_BIN/rad" "$stage/"
    tar -czf "$tarball" -C "$stage" radian rad
    rm -rf "$stage"
    (cd "$DIST" && shasum -a 256 "$(basename "$tarball")" > "$(basename "$tarball").sha256")
    (cd "$DIST" && shasum -a 256 -c "$(basename "$tarball").sha256")

    # 5b. Smoke-test the package from a temp dir.
    smoke="$(mktemp -d)"
    tar -xzf "$tarball" -C "$smoke"
    [ "$("$smoke/radian" --version)" = "radian $VERSION" ] || {
        echo "release-macos.sh: $target: --version did not print 'radian $VERSION'." >&2
        exit 1; }
    codesign --verify --strict "$smoke/radian" "$smoke/rad"
    if [ "$ADHOC" -eq 0 ]; then
        codesign -dv "$smoke/radian" 2>&1 | grep -q "Authority=Developer ID Application" || {
            echo "release-macos.sh: $target: no Developer ID authority." >&2; exit 1; }
        codesign -dv "$smoke/radian" 2>&1 | grep -q "runtime" || {
            echo "release-macos.sh: $target: no hardened runtime flag." >&2; exit 1; }
    else
        codesign -dv "$smoke/radian" 2>&1 | grep -q "Signature=adhoc" || {
            echo "release-macos.sh: $target: no ad-hoc signature." >&2; exit 1; }
    fi
    arch="$(lipo -archs "$smoke/radian")"
    case "$target" in
        aarch64-apple-darwin) [ "$arch" = "arm64" ] ;;
        x86_64-apple-darwin) [ "$arch" = "x86_64" ] ;;
    esac || { echo "release-macos.sh: $target: arch is '$arch'." >&2; exit 1; }
    rm -rf "$smoke"
    echo "release-macos.sh: $target ok: $tarball"
done

if [ "$PUBLISH" -eq 1 ]; then
    for target in $TARGETS; do
        case " $NOTARIZED_LIST " in
            *" $target "*) ;;
            *) echo "release-macos.sh: refusing to publish $target: not notarized." >&2
                exit 1 ;;
        esac
    done
fi

# 6. Formula from the template. macOS hashes come from the local packages;
# Linux (musl) hashes come from the draft release's .sha256 files.
linux_sha() {
    # linux_sha <target>: hash of the draft asset, or zeros in test mode.
    if [ "$NO_RELEASE_CHECK" -eq 1 ]; then
        printf '0%.0s' $(seq 64)
        return
    fi
    tmp="$(mktemp -d)"
    gh release download "$TAG" --pattern "radian-$VERSION-$1.tar.gz.sha256" \
        --dir "$tmp" --clobber
    hash="$(cut -d' ' -f1 "$tmp/radian-$VERSION-$1.tar.gz.sha256")"
    rm -rf "$tmp"
    case "$hash" in
        ????????????????????????????????????????????????????????????????) ;;
        *) echo "release-macos.sh: bad Linux sha for $1." >&2; exit 1 ;;
    esac
    printf '%s' "$hash"
}
mac_sha() {
    # mac_sha <target>: echoes the hash. Refuses when the file does not
    # match its .sha256.
    (cd "$DIST" && shasum -a 256 -c "radian-$VERSION-$1.tar.gz.sha256" > /dev/null) \
        || { echo "release-macos.sh: sha mismatch for $1." >&2; exit 1; }
    cut -d' ' -f1 "$DIST/radian-$VERSION-$1.tar.gz.sha256"
}
formula_url() {
    printf 'https://github.com/%s/releases/download/%s/radian-%s-%s.tar.gz' \
        "$REPO" "$TAG" "$VERSION" "$1"
}
FORMULA="$DIST/Formula/radian.rb"
mkdir -p "$DIST/Formula"
sed -e "s/@VERSION@/$VERSION/g" \
    -e "s|@URL_MAC_ARM@|$(formula_url aarch64-apple-darwin)|g" \
    -e "s|@URL_MAC_INTEL@|$(formula_url x86_64-apple-darwin)|g" \
    -e "s|@URL_LINUX_ARM@|$(formula_url aarch64-unknown-linux-musl)|g" \
    -e "s|@URL_LINUX_INTEL@|$(formula_url x86_64-unknown-linux-musl)|g" \
    packaging/homebrew/radian.rb.in > "$FORMULA"
# Fill the macOS hashes (zeros for targets not built in this run: test only,
# and --publish always builds every target through the loop above).
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if [ -f "$DIST/radian-$VERSION-$target.tar.gz" ]; then
        hash="$(mac_sha "$target")"
    else
        hash="$(printf '0%.0s' $(seq 64))"
    fi
    case "$target" in
        aarch64-apple-darwin) key=SHA_MAC_ARM ;;
        *) key=SHA_MAC_INTEL ;;
    esac
    perl -pi -e "s/\@$key\@/$hash/" "$FORMULA"
done
perl -pi -e "s/\@SHA_LINUX_ARM\@/$(linux_sha aarch64-unknown-linux-musl)/" "$FORMULA"
perl -pi -e "s/\@SHA_LINUX_INTEL\@/$(linux_sha x86_64-unknown-linux-musl)/" "$FORMULA"
grep -q '@[A-Z_]*@' "$FORMULA" && {
    echo "release-macos.sh: unfilled placeholder in $FORMULA." >&2; exit 1; }
for target in aarch64-apple-darwin x86_64-apple-darwin \
    aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    grep -q "releases/download/$TAG/radian-$VERSION-$target.tar.gz" "$FORMULA" || {
        echo "release-macos.sh: formula URL for $target does not point at $TAG." >&2
        exit 1; }
done
echo "release-macos.sh: formula: $FORMULA"

# 7. Outward steps: --publish only.
if [ "$PUBLISH" -eq 1 ]; then
    gh release upload "$TAG" \
        "$DIST"/radian-"$VERSION"-aarch64-apple-darwin.tar.gz \
        "$DIST"/radian-"$VERSION"-aarch64-apple-darwin.tar.gz.sha256 \
        "$DIST"/radian-"$VERSION"-x86_64-apple-darwin.tar.gz \
        "$DIST"/radian-"$VERSION"-x86_64-apple-darwin.tar.gz.sha256 \
        --clobber
    gh release edit "$TAG" --draft=false
    checkdir="$(mktemp -d)"
    gh release download "$TAG" --pattern "radian-$VERSION-*.sha256" \
        --dir "$checkdir" --clobber
    for target in aarch64-apple-darwin x86_64-apple-darwin \
        aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        asset="radian-$VERSION-$target.tar.gz"
        curl --netrc-file /dev/null -fsSL -o "$checkdir/$asset" \
            "https://github.com/$REPO/radian/releases/download/$TAG/$asset"
        (cd "$checkdir" && shasum -a 256 -c "$asset.sha256") || {
            echo "release-macos.sh: anonymous download of $asset mismatches." >&2
            exit 1; }
    done
    rm -rf "$checkdir"
    tapdir="$(mktemp -d)"
    git clone "https://github.com/hisea/homebrew-tap.git" "$tapdir"
    cp "$FORMULA" "$tapdir/Formula/radian.rb"
    [ "$(cd "$tapdir" && git status --porcelain)" = " M Formula/radian.rb" ] || {
        echo "release-macos.sh: tap tree touched more than Formula/radian.rb." >&2
        (cd "$tapdir" && git status --porcelain) >&2
        exit 1; }
    (cd "$tapdir" && git add Formula/radian.rb \
        && git commit -m "radian $VERSION" && git push origin HEAD)
    rm -rf "$tapdir"
fi

echo "release-macos.sh: done ($([ "$PUBLISH" -eq 1 ] && echo published || echo dry run))."
