# `themes/` — test fixtures

## `kanagawa-wave.json`

Copied verbatim from the owner's repository
[hisea/pi-deck](https://github.com/hisea/pi-deck) (`themes/kanagawa-wave.json`, MIT license,
read 2026-10-13). Its palette comes from
[kanagawa.nvim](https://github.com/rebelot/kanagawa.nvim) (MIT).

The file is a [pi](https://github.com/earendil-works/pi) theme: Radian reads pi's theme JSON
as-is (ADR 0029 decision 1). Do not reformat or "improve" it — the point of the fixture is
that a real user theme, hex vars and all, round-trips through Radian's loader. The tests
that consume it live in `crates/radian-tui/src/theme/file.rs` (per-role RGB spot checks after
variable resolution) and `crates/radian-tui/tests/pty.rs` (the screen colors on a real pty).
