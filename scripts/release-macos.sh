#!/bin/sh
# macOS half of the radian release, plus the shared package/formula steps that
# GitHub Actions reuses. Pushing a v* tag is the only manual step:
# .github/workflows/release.yml checks the tag, builds the Linux musl
# tarballs, runs this script's --package-only mode for the signed and
# notarized macOS tarballs, publishes the release with the version's notes
# file, and pushes Formula/radian.rb to the Homebrew tap (normal releases
# only). The notes-file bug (a draft stamped with placeholder text and never
# replaced) is gone: every release is created with --notes-file.
#
# Maintainer steps for a release:
#   1. Write packaging/release-notes/<version>.md (no leading "v").
#   2. Tag: git tag v<version> && git push origin v<version>
#   3. Wait for the release.yml workflow on that tag.
#
# Maintainer fallback on this Mac (when CI cannot sign): build and check
# everything without outward steps, then publish by hand:
#   sh scripts/release-macos.sh <version>                # dry run first
#   sh scripts/release-macos.sh <version> --publish
# For --publish, download the two Linux musl tarballs plus their .sha256
# files from the CI run into dist/ first (gh run download), so the formula
# uses the real Linux hashes.
#
# Environment:
#   RADIAN_SIGN_IDENTITY   codesigning identity. Default: the first
#                          "Developer ID Application" identity that
#                          `security find-identity -v -p codesigning` lists.
#                          Refuses when there is none. TEST ONLY: "-" selects
#                          an ad-hoc signature (refused with --publish).
#                          CI sets this to the identity imported into the
#                          temporary keychain the workflow creates and deletes.
#   RADIAN_NOTARY_PROFILE  notarytool keychain profile. Store it once with:
#                          xcrun notarytool store-credentials RADIAN \
#                            --apple-id <addr> --team-id <id>
#                          (without --password: notarytool then asks for it,
#                          keeping it out of shell history).
#                          then export RADIAN_NOTARY_PROFILE=RADIAN.
#                          Unneeded with --skip-notarize (test only).
#   RADIAN_NOTARY_API_KEY_FILE
#                          App Store Connect API key .p8 file (CI): with
#                          RADIAN_NOTARY_KEY_ID and RADIAN_NOTARY_ISSUER_ID
#                          this selects API-key notarization instead of the
#                          keychain profile. The workflow writes this file to
#                          the runner's temp dir and removes it when done.
#   RADIAN_NOTARY_KEY_ID / RADIAN_NOTARY_ISSUER_ID
#                          the key id and issuer for RADIAN_NOTARY_API_KEY_FILE.
#
# Modes: the default is a dry run (everything except the outward steps in
# step 7). --publish uploads the macOS files (creating the release from the
# notes file when it does not exist yet), publishes it, checks every asset
# anonymously, and pushes Formula/radian.rb to hisea/homebrew-tap; a version
# with a pre-release suffix (e.g. 0.1.0-rc.1) is published as a pre-release
# and skips the tap push. --package-only is the CI entry point: it stops
# after packaging (step 5) and skips the tag/draft gates, because no release
# exists yet when CI packages. --render-formula only renders
# dist/Formula/radian.rb from the .sha256 files already in dist/ (all four
# targets must be present and verified): the tap job uses it after
# downloading every job's artifacts. --target <triple> builds one macOS
# target (default: both; test convenience). --skip-notarize and
# --no-release-check are TEST ONLY and are refused with --publish. A bare
# Mach-O cannot be stapled (stapler needs a container), so there is no staple
# step: Gatekeeper verifies the notarization ticket online.
#
# Usage: sh scripts/release-macos.sh <version>
#        [--publish | --package-only | --render-formula]
#        [--target <triple>] [--skip-notarize] [--no-release-check]
set -eu

VERSION="${1:-}"
shift || true
PUBLISH=0
PACKAGE_ONLY=0
RENDER_FORMULA=0
TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
SKIP_NOTARIZE=0
NO_RELEASE_CHECK=0
while [ $# -gt 0 ]; do
    case "$1" in
        --publish) PUBLISH=1 ;;
        --package-only) PACKAGE_ONLY=1 ;;
        --render-formula) RENDER_FORMULA=1 ;;
        --target) TARGETS="$2"; shift ;;
        --skip-notarize) SKIP_NOTARIZE=1 ;;
        --no-release-check) NO_RELEASE_CHECK=1 ;;
        *) echo "release-macos.sh: unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

[ $((PUBLISH + PACKAGE_ONLY + RENDER_FORMULA)) -le 1 ] || {
    echo "release-macos.sh: --publish, --package-only and --render-formula exclude each other." >&2
    exit 2; }

case "$VERSION" in
    *-*) BASE="${VERSION%%-*}"; SUFFIX="${VERSION#*-}" ;;
    *) BASE="$VERSION"; SUFFIX="" ;;
esac
valid=1
case "$BASE" in *.*) ;; *) valid=0 ;; esac
case "$BASE" in ''|*[!0-9.]*|*..*|.*|*.) valid=0 ;; esac
if [ -n "$SUFFIX" ]; then
    case "$SUFFIX" in ''|*[!0-9A-Za-z.-]*) valid=0 ;; esac
fi
[ "$valid" -eq 1 ] || {
    echo "release-macos.sh: version must be like 0.1.0 or 0.1.0-rc.1, got '$VERSION'." >&2
    exit 2; }
if [ -n "$SUFFIX" ]; then PRERELEASE=1; else PRERELEASE=0; fi
TAG="v$VERSION"
REPO="hisea/radian"
DIST="dist"
NOTES="packaging/release-notes/$VERSION.md"
[ -s "$NOTES" ] || {
    echo "release-macos.sh: missing notes file $NOTES." >&2; exit 1; }
echo "release-macos.sh: notes: $NOTES"
CRATE_VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' crates/radian-tui/Cargo.toml | head -n 1)"
[ -n "$CRATE_VERSION" ] || {
    echo "release-macos.sh: could not read the version of crates/radian-tui." >&2
    exit 1; }

if [ "$PUBLISH" -eq 1 ]; then
    [ "$SKIP_NOTARIZE" -eq 0 ] || {
        echo "release-macos.sh: --publish refuses --skip-notarize." >&2; exit 1; }
    [ "$NO_RELEASE_CHECK" -eq 0 ] || {
        echo "release-macos.sh: --publish refuses --no-release-check." >&2; exit 1; }
fi

STRICT=0
if [ "$PUBLISH" -eq 1 ] || [ "$RENDER_FORMULA" -eq 1 ]; then STRICT=1; fi

zeros64() {
    # zeros64: 64 zeros without seq (not present on every Mac).
    awk 'BEGIN { for (i = 0; i < 64; i++) printf "0" }'
}

mac_sha() {
    # mac_sha <target>: echoes the hash. Refuses when the file does not
    # match its .sha256.
    (cd "$DIST" && shasum -a 256 -c "radian-$VERSION-$1.tar.gz.sha256" > /dev/null) \
        || { echo "release-macos.sh: sha mismatch for $1." >&2; exit 1; }
    cut -d' ' -f1 "$DIST/radian-$VERSION-$1.tar.gz.sha256"
}

linux_sha() {
    # linux_sha <target>: echoes the hash. Prefers a local .sha256 (CI
    # artifacts downloaded into dist/); otherwise downloads it from the
    # release, or uses zeros in non-strict test mode.
    if [ -f "$DIST/radian-$VERSION-$1.tar.gz.sha256" ]; then
        (cd "$DIST" && shasum -a 256 -c "radian-$VERSION-$1.tar.gz.sha256" > /dev/null) || {
            echo "release-macos.sh: sha mismatch for $1." >&2; exit 1; }
        cut -d' ' -f1 "$DIST/radian-$VERSION-$1.tar.gz.sha256"
        return 0
    fi
    if [ "$STRICT" -eq 0 ]; then
        zeros64
        return 0
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

hash_for() {
    # hash_for <target>: echoes the formula hash. macOS hashes come from the
    # local packages (zeros for targets not built in this run: non-strict
    # test mode only); Linux hashes come from linux_sha.
    case "$1" in
        *-apple-darwin)
            if [ -f "$DIST/radian-$VERSION-$1.tar.gz" ]; then
                mac_sha "$1"; return 0
            fi
            [ "$STRICT" -eq 1 ] && {
                echo "release-macos.sh: missing local package for $1." >&2
                return 1; }
            zeros64; return 0 ;;
        *) linux_sha "$1" ;;
    esac
}

formula_url() {
    printf 'https://github.com/%s/releases/download/%s/radian-%s-%s.tar.gz' \
        "$REPO" "$TAG" "$VERSION" "$1"
}

render_formula() {
    # render_formula: fill packaging/homebrew/radian.rb.in into
    # dist/Formula/radian.rb. Refuses on an unfilled placeholder or a URL
    # that does not point at this tag.
    FORMULA="$DIST/Formula/radian.rb"
    mkdir -p "$DIST/Formula"
    sed -e "s/@VERSION@/$VERSION/g" \
        -e "s|@URL_MAC_ARM@|$(formula_url aarch64-apple-darwin)|g" \
        -e "s|@URL_MAC_INTEL@|$(formula_url x86_64-apple-darwin)|g" \
        -e "s|@URL_LINUX_ARM@|$(formula_url aarch64-unknown-linux-musl)|g" \
        -e "s|@URL_LINUX_INTEL@|$(formula_url x86_64-unknown-linux-musl)|g" \
        packaging/homebrew/radian.rb.in > "$FORMULA"
    for target in aarch64-apple-darwin x86_64-apple-darwin; do
        case "$target" in
            aarch64-apple-darwin) key=SHA_MAC_ARM ;;
            *) key=SHA_MAC_INTEL ;;
        esac
        hash="$(hash_for "$target")" || return 1
        perl -pi -e "s/\@$key\@/$hash/" "$FORMULA"
    done
    perl -pi -e "s/\@SHA_LINUX_ARM\@/$(hash_for aarch64-unknown-linux-musl)/" "$FORMULA" || return 1
    perl -pi -e "s/\@SHA_LINUX_INTEL\@/$(hash_for x86_64-unknown-linux-musl)/" "$FORMULA" || return 1
    grep -q '@[A-Z_]*@' "$FORMULA" && {
        echo "release-macos.sh: unfilled placeholder in $FORMULA." >&2; return 1; }
    for target in aarch64-apple-darwin x86_64-apple-darwin \
        aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        grep -q "releases/download/$TAG/radian-$VERSION-$target.tar.gz" "$FORMULA" || {
            echo "release-macos.sh: formula URL for $target does not point at $TAG." >&2
            return 1; }
    done
    echo "release-macos.sh: formula: $FORMULA"
}

notarize_bin() {
    # notarize_bin <zipfile>: submit one zip and require "status: Accepted".
    # API-key credentials (CI) win when RADIAN_NOTARY_API_KEY_FILE is set;
    # otherwise the local keychain profile is used.
    if [ -n "${RADIAN_NOTARY_API_KEY_FILE:-}" ]; then
        [ -n "${RADIAN_NOTARY_KEY_ID:-}" ] || {
            echo "release-macos.sh: RADIAN_NOTARY_KEY_ID is not set." >&2; exit 1; }
        [ -n "${RADIAN_NOTARY_ISSUER_ID:-}" ] || {
            echo "release-macos.sh: RADIAN_NOTARY_ISSUER_ID is not set." >&2; exit 1; }
        [ -f "$RADIAN_NOTARY_API_KEY_FILE" ] || {
            echo "release-macos.sh: notary key file is missing." >&2; exit 1; }
        SUBMIT_OUT="$(xcrun notarytool submit "$1" \
            --key "$RADIAN_NOTARY_API_KEY_FILE" \
            --key-id "$RADIAN_NOTARY_KEY_ID" \
            --issuer "$RADIAN_NOTARY_ISSUER_ID" --wait 2>&1)" || {
            echo "$SUBMIT_OUT" >&2
            echo "release-macos.sh: notarytool submit failed for $1." >&2
            exit 1; }
    else
        [ -n "${RADIAN_NOTARY_PROFILE:-}" ] || {
            echo "release-macos.sh: RADIAN_NOTARY_PROFILE is not set." >&2; exit 1; }
        SUBMIT_OUT="$(xcrun notarytool submit "$1" \
            --keychain-profile "$RADIAN_NOTARY_PROFILE" --wait 2>&1)" || {
            echo "$SUBMIT_OUT" >&2
            echo "release-macos.sh: notarytool submit failed for $1." >&2
            exit 1; }
    fi
    echo "$SUBMIT_OUT" | grep -q "status: Accepted" || {
        echo "$SUBMIT_OUT" >&2
        echo "release-macos.sh: notarization not Accepted for $1." >&2
        exit 1; }
}

if [ "$RENDER_FORMULA" -eq 1 ]; then
    render_formula || exit 1
    echo "release-macos.sh: done (render-formula)."
    exit 0
fi

# 1. Gates: clean tree always; the tag and draft gates only when this run
# publishes or dry-runs (CI --package-only has no release yet).
if [ -n "$(git status --porcelain)" ]; then
    echo "release-macos.sh: refusing to release from a dirty tree." >&2
    git status --porcelain >&2
    exit 1
fi
if [ "$NO_RELEASE_CHECK" -eq 0 ] && [ "$PACKAGE_ONLY" -eq 0 ]; then
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
if [ "$PUBLISH" -eq 1 ] && [ -z "${RADIAN_NOTARY_PROFILE:-}" ] \
    && [ -z "${RADIAN_NOTARY_API_KEY_FILE:-}" ]; then
    echo "release-macos.sh: RADIAN_NOTARY_PROFILE or RADIAN_NOTARY_API_KEY_FILE is not set." >&2
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
        for bin in radian rad; do
            zipfile="$DIST/notarize-$target-$bin.zip"
            mkdir -p "$DIST"
            rm -f "$zipfile"
            ditto -c -k --keepParent "$RELEASE_BIN/$bin" "$zipfile"
            echo "release-macos.sh: notarizing $target/$bin ..."
            notarize_bin "$zipfile"
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
    [ "$("$smoke/radian" --version)" = "radian $CRATE_VERSION" ] || {
        echo "release-macos.sh: $target: --version did not print 'radian $CRATE_VERSION'." >&2
        exit 1; }
    codesign --verify --strict "$smoke/radian" "$smoke/rad"
    if [ "$ADHOC" -eq 0 ]; then
        codesign -dvv "$smoke/radian" 2>&1 | grep -q "Authority=Developer ID Application" || {
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

if [ "$PACKAGE_ONLY" -eq 1 ]; then
    echo "release-macos.sh: done (package-only)."
    exit 0
fi

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
# Linux hashes prefer local .sha256 files (CI artifacts in dist/) and fall
# back to the release download (strict) or zeros (non-strict test mode).
render_formula || exit 1

# 7. Outward steps: --publish only.
if [ "$PUBLISH" -eq 1 ]; then
    FILES=""
    for target in $TARGETS; do
        FILES="$FILES $DIST/radian-$VERSION-$target.tar.gz $DIST/radian-$VERSION-$target.tar.gz.sha256"
    done
    if gh release view "$TAG" >/dev/null 2>&1; then
        # shellcheck disable=SC2086: FILES is a file list on purpose.
        gh release upload "$TAG" $FILES --clobber
    else
        if [ "$PRERELEASE" -eq 1 ]; then
            # shellcheck disable=SC2086: FILES is a file list on purpose.
            gh release create "$TAG" --title "$TAG" --notes-file "$NOTES" --prerelease $FILES
        else
            # shellcheck disable=SC2086: FILES is a file list on purpose.
            gh release create "$TAG" --title "$TAG" --notes-file "$NOTES" $FILES
        fi
    fi
    gh release edit "$TAG" --draft=false
    checkdir="$(mktemp -d)"
    gh release download "$TAG" --pattern "radian-$VERSION-*.sha256" \
        --dir "$checkdir" --clobber
    for target in aarch64-apple-darwin x86_64-apple-darwin \
        aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        asset="radian-$VERSION-$target.tar.gz"
        curl --netrc-file /dev/null -fsSL -o "$checkdir/$asset" \
            "https://github.com/$REPO/releases/download/$TAG/$asset"
        (cd "$checkdir" && shasum -a 256 -c "$asset.sha256") || {
            echo "release-macos.sh: anonymous download of $asset mismatches." >&2
            exit 1; }
    done
    rm -rf "$checkdir"
    if [ "$PRERELEASE" -eq 1 ]; then
        echo "release-macos.sh: pre-release: skipping the tap push."
    else
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
fi

echo "release-macos.sh: done ($([ "$PUBLISH" -eq 1 ] && echo published || echo dry run))."
