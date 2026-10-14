# RADIAN-CHANGES for `third_party/codex-mermaid`

Radian vendors the upstream crate unchanged (ADR 0028 R8): every file except
`Cargo.toml` is byte-identical to upstream, verified by sha256 at vendoring time.
All Radian-specific adaptation lives in `Cargo.toml` and this file.

## Source

- Repository: <https://github.com/openai/codex>
- Pinned commit: `2f05c0a71f59fb2f05800846351f3e9926c3e52b`
- Upstream directory: `codex-rs/mermaid`
- Vendored on: 2026-10-13

## Copied

- `README.md`, `examples/render.rs`
- `src/draw.rs`, `src/families_tests.rs`, `src/lib.rs`, `src/output.rs`,
  `src/parse.rs`, `src/relations.rs`, `src/sequence.rs`, `src/state.rs`,
  `src/syntax.rs`, `src/tests.rs`
- `src/snapshots/` — all 7 `.snap` files
- `LICENSE` — Radian's `LICENSE-APACHE` (the canonical Apache-2.0 text). Upstream
  ships the same license text with different indentation; the canonical text is
  kept here.
- `NOTICE` — upstream's root `NOTICE` verbatim.

## Not copied

- `BUILD.bazel` — Bazel build definition; Radian builds with Cargo only.

## Changes (Cargo.toml only)

1. Header comment: provenance (repository, upstream path, pinned commit) and the
   note that workspace inheritance was resolved.
2. `[package]`: `version.workspace`, `edition.workspace`, `license.workspace`
   resolved to `version = "0.0.0"`, `edition = "2024"`,
   `license = "Apache-2.0"`, plus `rust-version = "1.88"` (upstream declares no
   `rust-version`; its toolchain file pins 1.95.0, Radian's MSRV is 1.88).
3. `[lints] workspace = true` resolved to the concrete table from codex's
   `[workspace.lints]` at the pinned commit (`[lints.rust]` empty, the clippy
   deny list verbatim), with one Radian-specific change:
   - `unwrap_used = "allow"` — all 53 clippy hits are `.unwrap()` in the two
     upstream test files (`src/tests.rs`, `src/families_tests.rs`); the library
     code has none. Denying it would force edits to upstream test sources.
4. `unicode-width = { workspace = true }` → `unicode-width = "0.2"` (codex pins
   `"0.2"`; Radian's lock already resolves 0.2.2).
5. `insta` / `pretty_assertions` dev-dependencies resolved to concrete
   requirements matching codex's pins (`"1.46.3"`, `"1.4.1"`). Cargo's resolver
   selected insta 1.49.0; it compiles and passes on Rust 1.88 (verified with
   `cargo +1.88 test -p codex-mermaid`).

## Ratatui attribution

Upstream's root `NOTICE` states that the codex *project* includes code derived
from Ratatui. Every vendored source file was searched for `ratatui`,
`copyright`, and attribution marks: no matches. The vendored crate contains no
Ratatui-derived code, so the Ratatui line is not carried into this directory's
`NOTICE` entry in the repository root `NOTICE`. Re-check this at every upgrade
and add the attribution if a future upstream version introduces such code.

## Upgrading

1. Choose the new upstream commit. Update the pinned sha in this file and in the
   `Cargo.toml` header comment.
2. Re-download each file over the vendored copy from
   `https://raw.githubusercontent.com/openai/codex/<sha>/codex-rs/mermaid/<path>`
   (directory listings via `https://api.github.com/repos/openai/codex/contents/<dir>?ref=<sha>`).
   Do not clone the repository.
3. `diff -r` the fresh download against this directory. Every difference must be
   explained by the change list above (i.e. only `Cargo.toml`, `LICENSE`,
   `NOTICE`, and this file); anything else is an accidental local edit.
4. Re-verify: `cargo +1.88 check -p codex-mermaid --all-targets`,
   `cargo clippy -p codex-mermaid --all-targets -- -D warnings`,
   `cargo test -p codex-mermaid` (snapshot files must be taken from upstream
   unchanged), plus the workspace Verify suite from the root `AGENTS.md`.
5. Re-run the Ratatui check above and update the root `NOTICE` entry's commit
   pin.
