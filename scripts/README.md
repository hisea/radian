# scripts

- `worktree.sh --from SRC <path> [<branch-or-base>]`: `git worktree add` plus a
  warm `target/` cloned from still tree `SRC` (`$RADIAN_WARM_TARGET_FROM` when
  `--from` is absent; APFS clone on macOS, reflink copy on Linux). Refuses when
  `SRC` is building unless `--force`. `worktree.sh --drop-target <worktree>`
  removes only that worktree's `target/`.
- `verify.sh [--log-dir <dir>]`: the full Verify of `AGENTS.md`: the two binary
  builds, the Verify block in order, then the provider contract suites. Runs every
  step even after a failure; one `rc=<n> <seconds>s <command>` line per step.
  `AGENTS.md` and its `run_step` lines are kept in sync by a docs drift test.
- `arc-gate.sh quick|full`: the arc gate. `quick` is fmt, clippy, the two builds,
  and nextest for the crates changed since `origin/main` plus `radian-tui`; `full`
  is `verify.sh`, here or on `$ARC_GATE_HOST` inside `$ARC_GATE_TREE`. Both are
  documented in `docs/user/arc-gate.md`, including the absolute-path trust rule.
- `temp-check.sh`: the temp file system check, sourced by both scripts (it is not
  a step). It reads the file system that holds `${TMPDIR:-/tmp}` with `df` and
  fails, before any build, when fewer than 50,000 inodes or fewer than 2 GiB are
  free: one line on stderr names the path, the free and total inodes, the free
  space and the two limits. Nothing is deleted. A file system that reports no
  usable inode count (none, `-`, or a number too large for the shell) skips the
  inode check and keeps the space check; a negative free space fails it. The two
  limits are `RADIAN_VERIFY_MIN_FREE_INODES` and `RADIAN_VERIFY_MIN_FREE_GIB`;
  their type, default, allowed values and when they take effect are in
  [`docs/user/arc-gate.md`](../docs/user/arc-gate.md#a-gate-for-this-repository).

## Release

Pushing a `v*` tag is the only manual step: `.github/workflows/release.yml`
checks the tag, builds the Linux musl tarballs, signs/notarizes the macOS
tarballs via `release-macos.sh --package-only`, publishes the release with
the version's notes file, and pushes `Formula/radian.rb` to the Homebrew tap
(normal releases only; a `-rc.1` style tag is published as a pre-release and
skips the tap).

Once per setup (these live in the GitHub `release` environment, limited to
`v*` tags so only release jobs see them):

- `MACOS_CERT_P12_BASE64` / `MACOS_CERT_PASSWORD`: export the Developer ID
  Application certificate from Keychain Access as `.p12`, base64 it into the
  first secret, put its export password in the second.
- `APPLE_API_KEY_P8_BASE64` / `APPLE_API_KEY_ID` / `APPLE_API_ISSUER_ID`:
  create an App Store Connect API key with the Developer role, base64 the
  downloaded `.p8` into the first secret, put the key id and issuer id in the
  other two.
- `HOMEBREW_TAP_TOKEN`: a fine-grained token limited to contents
  read/write on the Homebrew tap repository.

Each release: write `packaging/release-notes/<version>.md` (`v0.1.0` takes
`0.1.0.md`), bump `crates/radian-tui`'s version when the tag base differs
from it, then tag and push the tag. The local fallback is
`sh scripts/release-macos.sh <version>` (dry run) and, with the Linux
artifacts from the CI run downloaded into `dist/`,
`sh scripts/release-macos.sh <version> --publish`.
