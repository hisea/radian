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

## The 16 bundled community themes

Copied verbatim from the [`@firstpick/pi-themes-bundle`](https://www.npmjs.com/package/@firstpick/pi-themes-bundle)
npm package, version **0.1.7** (MIT; the license text is `LICENSE.pi-themes-bundle` in this
directory, read 2026-10-14). They are pi themes, and Radian reads pi's theme JSON as-is
(ADR 0029 decision 1): the fixture test `the_community_theme_fixtures_all_load_without_warnings`
loads all 17 files (these 16 plus `kanagawa-wave.json`) and requires **zero warnings** from
any of them and a resolved value for each of pi's three text levels. Several of them set
`text` to `""` (the terminal's default color), which is legal.

Do not edit these files to silence a warning: a warning means either Radian's role mapping is
wrong or the theme itself is.
