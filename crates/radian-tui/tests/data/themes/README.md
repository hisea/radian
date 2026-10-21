# `themes/` — test fixtures

Every `*.json` here is a real **pi** theme, kept verbatim; every `*.toml` beside it is the
**golden** Radian theme produced by `radian theme import <name>.json` (ADR 0029
decision 3). The importer test
(`crates/radian-tui/src/theme/importer.rs::every_fixture_converts_to_its_committed_golden`)
re-converts each JSON and requires the result to equal its golden byte for byte: a change to
the mapping table changes a golden, so the mutation is caught. The JSON files are never edited
to silence a warning or to match a golden — a mismatch means either the mapping is wrong or the
theme is.

## `kanagawa-wave.json`

Copied verbatim from the repository
[hisea/pi-deck](https://github.com/hisea/pi-deck) (`themes/kanagawa-wave.json`, MIT license,
read 2026-10-13). Its palette comes from
[kanagawa.nvim](https://github.com/rebelot/kanagawa.nvim) (MIT).

Its `.toml` golden is loaded by the round-trip test
(`file.rs::an_imported_fixture_round_trips_to_the_json_loader_role_by_role`): importing the
JSON and loading the TOML gives the same resolved theme, role by role, as the old JSON loader.

## The 16 bundled community themes

Copied verbatim from the [`@firstpick/pi-themes-bundle`](https://www.npmjs.com/package/@firstpick/pi-themes-bundle)
npm package, version **0.1.7** (MIT; the license text is `LICENSE.pi-themes-bundle` in this
directory, read 2026-10-14). The fixture test
`the_community_theme_fixtures_all_load_without_warnings` parses all 17 JSON files (these 16
plus `kanagawa-wave.json`), requires **zero warnings** from the JSON loader and a resolved
value for each of pi's three text levels. Several of them set `text` to `""` (the terminal's
default color), which is legal.

## The `.toml` goldens

`git log` has the commit that added them; regenerate them only by re-running the importer
(the acceptance is that they are the converter's output, not hand-written). A `.toml` beside a
`.json` is also what `tui.theme = "<name>"` loads: the `.toml` wins and the `.json` is the
one-version transition (ADR 0029).
