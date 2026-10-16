# RADIAN-CHANGES for `third_party/grok-mermaid`

Radian vendors the upstream Mermaid renderer with **two recorded, reversible edits** (ADR 0042
decision 2). Every other byte of `src/mermaid.rs` is identical to upstream at the pinned commit,
verified by sha256: `tests/pristine.rs` reverses the edits textually and asserts the hash below.

## Source

- Repository: <https://github.com/xai-org/grok-build>
- Pinned commit: `4827113` (2026-09-15, the commit named in ADR 0042)
- Upstream path: `crates/codegen/xai-grok-markdown/src/mermaid.rs`
- Vendored on: 2026-10-16
- `src/mermaid.rs` as fetched (before Radian's edits): 5699 lines,
  sha256 `e53c81fc02cddc78a3f7f3729237f83acd7c2678535a830210d2b1320f60529f`

## Copied

- `LICENSE` — upstream's `LICENSE` verbatim (Apache License 2.0, `Copyright 2023-2026 SpaceXAI`).
- `THIRD-PARTY-NOTICES` — upstream's `THIRD-PARTY-NOTICES` verbatim (762 KB). This is the
  monorepo's whole aggregated third-party list; it is carried unchanged because that is the file
  upstream ships, and Radian's vendoring rule is "verbatim unless a change is listed here". It is
  not required to build or use the crate.
- `src/mermaid.rs` — upstream's file, with the two edits below.

## Written by Radian (not upstream)

- `Cargo.toml` — provenance header above; `[package]` resolves the workspace inheritance to
  concrete values (`version = "0.0.0"`, `edition = "2024"`, `license = "Apache-2.0"`, plus
  `rust-version = "1.88"`; upstream declares no `rust-version` and builds with edition 2024).
  `[dependencies]` keeps only `unicode-width = "0.2"` (upstream's other dependency, `ratatui`, is
  removed by edit 1). `[lints.clippy] question_mark = "allow"` — see "Lints" below.
- `src/lib.rs` — Radian's entry point, [`grok_mermaid::render`]. This is a copy of upstream's own
  34-line dispatch (`src/mermaid.rs` lines 67-82) with the trailing `fallback` step replaced by a
  `Reason` (ADR 0042 decision 3). It is not a modification of `src/mermaid.rs`; it is a second
  caller of that file's (now `pub`) stages.
- `src/text.rs` — the text shim edit 1 compiles against.
- `tests/pristine.rs` — reverses the edits and checks the hash.

## Edits to `src/mermaid.rs` (in order)

### Edit 1 — the two `use` lines (one replacement of two lines)

Upstream:

```rust
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
```

Radan (ADR 0042 decision 2: the crate does not depend on ratatui, so both `radian-host` and
`radian-tui` can use it):

```rust
use crate::text::{Line, Modifier, Span, Style};
```

`src/text.rs` supplies those four names with only the surface the vendored file calls.

### Edit 2 — visibility (the `pub(crate)` → `pub` replacement)

`src/lib.rs` reuses the stages, so the items it names are made `pub` (or `pub(crate)` → `pub`).
Every change is a single `pub` keyword (or the `pub(crate)` → `pub` promotion); no signature, body
or ordering changed. The full list, by upstream line:

| Upstream line | Before | After |
|---|---|---|
| 14 | `pub(crate) struct MermaidStyles` | `pub struct MermaidStyles` |
| 23 | `pub(crate) struct MermaidArt` | `pub struct MermaidArt` |
| 52 | `enum Oversize` | `pub enum Oversize` |
| 58 | `pub(crate) fn render` | `pub fn render` |
| 139 | `struct Group` | `pub struct Group` |
| 145 | `struct Graph` | `pub struct Graph` |
| 149 | `groups: Vec<Group>,` | `pub groups: Vec<Group>,` |
| 189 | `fn parse_graph` | `pub fn parse_graph` |
| 766 | `fn parse_state` | `pub fn parse_state` |
| 974 | `struct ClassInfo` | `pub struct ClassInfo` |
| 980 | `fn parse_class` | `pub fn parse_class` |
| 1258 | `fn parse_er` | `pub fn parse_er` |
| 1409 | `fn render_class` | `pub fn render_class` |
| 1843 | `fn layout_flowchart` | `pub fn layout_flowchart` |
| 2065 | `fn render_grouped` | `pub fn render_grouped` |
| 3415 | `struct Sequence` | `pub struct Sequence` |
| 3440 | `fn parse_sequence` | `pub fn parse_sequence` |
| 3639 | `fn layout_sequence` | `pub fn layout_sequence` |
| 3925 | `fn fallback` | `pub fn fallback` |

`Graph`, `ClassInfo` and `Sequence` are `pub` because they appear in the `pub` stage signatures
(Rust's `private_interfaces` lint; with `-D warnings` in the workspace Verify it is an error).
`Groups`' `groups` field is `pub` because the dispatch reads `graph.groups.is_empty()`.
`fallback` is `pub` for completeness; `src/lib.rs` never calls it (ADR 0042 decision 3).

## Lints

`Cargo.toml` allows one clippy lint the upstream file trips under the workspace's pinned clippy:

- `question_mark` — `src/mermaid.rs:3561` has an `else if let … { value? } else { return None }`
  that newer clippy prefers as `let … ?`. Editing it would break the byte-identical rule, so it is
  allowed here instead.

## Upgrading

1. Choose the new upstream commit. Update the pin and the hash in this file and the header of
   `Cargo.toml`.
2. Re-download `src/mermaid.rs` over the vendored copy from
   `https://raw.githubusercontent.com/xai-org/grok-build/<commit>/crates/codegen/xai-grok-markdown/src/mermaid.rs`
   (via `gh api repos/xai-org/grok-build/contents/<path>?ref=<commit> --jq .content | base64 -d`).
3. Re-apply edit 1 and edit 2 (the table above), `diff` the result, and record the new hash.
4. Re-verify: `cargo test -p grok-mermaid` (all upstream tests), `cargo tree -p grok-mermaid -e
   normal` (only `unicode-width`), and the workspace Verify suite from the root `AGENTS.md`.
