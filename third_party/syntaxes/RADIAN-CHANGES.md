# RADIAN-CHANGES for `third_party/syntaxes`

Radian vendors three Sublime syntax files **unchanged** (ADR 0028 R2, owner
2026-10-14) so the code-block highlighter can recognize TypeScript, TSX and TOML,
which `syntect`'s packed default set does not ship. Every file is byte-identical
to the copy in `bat`'s tree; verified by sha256 at vendoring time. Nothing in this
directory is modified by Radian.

## Source

- Repository: <https://github.com/sharkdp/bat>
- Pinned commit: `d9559c69f541873f8c47ac8ad81d985f4441edf6`
- Vendored on: 2026-10-14

## Copied (verbatim)

| File here | Upstream path | License |
|---|---|---|
| `TypeScript.sublime-syntax` | `assets/syntaxes/02_Extra/TypeScript.sublime-syntax` | `TypeScript.LICENSE` |
| `TypsecriptReact.sublime-syntax` | `assets/syntaxes/02_Extra/TypsecriptReact.sublime-syntax` (bat's spelling) | `TypeScript.LICENSE` |
| `TOML.sublime-syntax` | `assets/syntaxes/02_Extra/TOML/TOML.sublime-syntax` | `TOML.LICENSE` |
| `TypeScript.LICENSE` | `assets/syntaxes/02_Extra/TypeScript/LICENSE` | — |
| `TOML.LICENSE` | `assets/syntaxes/02_Extra/TOML/LICENSE` | — |
| `bat.LICENSE-MIT` | `LICENSE-MIT` | bat's own MIT |
| `bat.LICENSE-APACHE` | `LICENSE-APACHE` | bat's own Apache-2.0 |

## Licenses (verified at vendoring time)

- **TOML** (`jasonwilliams/sublime_toml_highlighting`, bat's submodule
  `assets/syntaxes/02_Extra/TOML` at `f5a57e8bff694a4e6c52a491dae579aabc7427cf`):
  **MIT**, Copyright (c) 2021 Jason Williams. `TOML.LICENSE` is that text
  verbatim.
- **TypeScript / TypeScriptReact** (`Microsoft/TypeScript-Sublime-Plugin`, bat's
  submodule `assets/syntaxes/02_Extra/TypeScript` at
  `ba45efd058df5111837e30fb9598cfc8cbd51095`): **Apache-2.0**.
  `TypeScript.LICENSE` is that `LICENSE` verbatim. bat's own
  `assets/acknowledgements.bin` lists `syntaxes/02_Extra/TypeScript/LICENSE`
  (Apache-2.0) as the attribution for the `TypeScript/` directory.
  **The two `.sublime-syntax` files carry no license header themselves**, and the
  upstream Microsoft plugin ships `.tmLanguage` (plist), not these YAML files. The
  YAML form is bat's (the format conversion), so these two files are **dual-
  attributed**: bat's own `LICENSE-MIT` / `LICENSE-APACHE` (MIT OR Apache-2.0, at
  the user's option) for bat's conversion, **and** the upstream Microsoft
  `TypeScript-Sublime-Plugin` Apache-2.0 for the grammar they are converted from
  (`TypeScript.LICENSE`). Both licenses are shipped here. This is the one item in
  this directory whose per-file license is attributed from the directory's
  upstream rather than read from the file itself; the owner accepted that
  attribution on 2026-10-14.

## Not copied

- The `.tmLanguage`/`.tmTheme`/sublime-settings files and the rest of the two
  submodules — only the three syntaxes above are needed.

## Updating

Re-copy the three files from a newer bat checkout, re-run
`crates/radian-tui/src/highlight.rs`'s tests, and replace the pinned commit here.
