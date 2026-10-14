# Vendored upstream model list for the structural test

`opencode-go-models.json` is a **trimmed** copy of the upstream provider's `models.json`.
Kept fields: `id`, `name`, `api`, `baseUrl` and (when present) `thinkingLevelMap` with all
seven levels, `null` meaning unsupported. Everything else upstream carries (cost, input
modalities, context window, …) is dropped, because the built-in list does not use it.

- Source: <https://github.com/monotykamary/pi-opencode-go-provider>, file `models.json`
- Commit: `cdd8a70` (v1.1.24, 2026-10-14)
- License: **MIT** (the repository's `LICENSE`; redistribution and modification are allowed,
  so the file can live here). The file is data, not code; the upstream `LICENSE` notice is the
  attribution.

`crates/radian-host/tests/opencode_go_models.rs` reads this file and asserts the built-in
list in `crates/radian-host/src/provider.rs` matches it id for id, field for field.

## Updating

1. `git clone --depth 1 https://github.com/monotykamary/pi-opencode-go-provider /tmp/upstream`
   (shallow; never read the user's `~/.pi/` checkout).
2. Record the new commit (`git -C /tmp/upstream rev-parse --short HEAD`). Replace the commit
   above and in `crates/radian-host/src/provider.rs`'s `OPENCODE_GO_MODELS` doc, and in ADR
   0026 D11's two revision lines.
3. Regenerate this file from the new `models.json`: keep `id`, `name`, `api`, `baseUrl` and
   `thinkingLevelMap` (all seven levels), in upstream order.
4. Regenerate the `thinkingLevelMap` consts and the model list in `provider.rs` from the same
   data (do **not** invent a map for a model upstream does not map). A model upstream removed
   is kept in the list until the owner decides; the test only checks that every upstream model
   is present and correct.
5. Run `cargo test -p radian-host --test opencode_go_models`.
