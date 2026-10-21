# Contributing

This project is built with Rust. The minimum Rust version is 1.99.1.
A debug build is `cargo build --workspace --locked`.
Read `AGENTS.md` before changing code.

Before a change is done, the checks in the Verify section of `AGENTS.md`
must all pass.

Read the ADRs a change touches before changing code. A change that
reverses a decision changes the ADR first, in its own docs-only commit.

Planned work lives in `docs/backlog/`, one file per item. A pull request
says which ADRs it touches.

## Releases

1. Tag the release: `git tag v<version> && git push origin v<version>`.
2. Wait for the `release` workflow: it builds the two Linux musl targets
   and creates the tag's release as a draft.
3. On a Mac, dry-run first, then publish:
   `sh scripts/release-macos.sh <version>`, then
   `sh scripts/release-macos.sh <version> --publish`.
   The script signs (Developer ID, via `RADIAN_SIGN_IDENTITY`), notarizes
   (notarytool profile in `RADIAN_NOTARY_PROFILE`), packages both macOS
   targets, fills `packaging/homebrew/radian.rb.in`, uploads the macOS
   files, publishes the draft, re-checks every asset over an anonymous
   download, and pushes the formula to `hisea/homebrew-tap`. Store the
   notary profile once with `xcrun notarytool store-credentials RADIAN
   --apple-id <addr> --team-id <id>` (without `--password`: notarytool then
   asks for it, keeping it out of shell history).
