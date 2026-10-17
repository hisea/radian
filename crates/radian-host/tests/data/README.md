# Vendored upstream model list for the structural test

`opencode-go-models.json` is a **trimmed** copy of the upstream provider's `models.json`.
Kept fields: `id`, `name`, `api`, `baseUrl` and (when present) `thinkingLevelMap` with all
seven levels, `null` meaning unsupported. Everything else upstream carries (cost, input
modalities, context window, …) is dropped.

- Source: <https://github.com/monotykamary/pi-opencode-go-provider>, file `models.json`
- Commit: `cdd8a70` (v1.1.24, 2026-10-14)
- License: **MIT** (the repository's `LICENSE`; redistribution and modification are allowed,
  so the file can live here). The file is data, not code; the upstream `LICENSE` notice is the
  attribution.

`crates/radian-host/tests/opencode_go_models.rs` reads this file and asserts the built-in
list in `crates/radian-host/src/provider.rs` matches it: the **names** and the
`thinkingLevelMap`s of every kept model.

Since ADR 0026 the list is **resynced to models.dev** (the 2026-10-17 snapshot), not
to upstream: models.dev decides each model's wire protocol (`provider.npm`) and the ids present,
while upstream still supplies the names and the thinking maps that models.dev does not carry.
The test encodes the two deliberate differences — the ids models.dev dropped
(`deepseek-v4-flash`, `grok-4.6`) and the five it moved to Responses (`@ai-sdk/openai`) — in
`DROPPED_PER_MODELS_DEV` and `RESPONSES_PER_MODELS_DEV`, so a future drift is still caught.

## Updating

1. `git clone --depth 1 https://github.com/monotykamary/pi-opencode-go-provider /tmp/upstream`
   (shallow; never read the user's `~/.pi/` checkout).
2. Record the new commit (`git -C /tmp/upstream rev-parse --short HEAD`). Replace the commit
   above and in `crates/radian-host/src/provider.rs`'s `OPENCODE_GO_MODELS` doc.
3. Regenerate this file from the new `models.json`: keep `id`, `name`, `api`, `baseUrl` and
   `thinkingLevelMap` (all seven levels), in upstream order.
4. Resync the ids and protocols against models.dev
   (`crates/radian-host/scripts/update-catalog-snapshot.py` regenerates the bundled snapshot from
   the same source): regenerate the model list in `provider.rs`, and update
   `DROPPED_PER_MODELS_DEV` / `RESPONSES_PER_MODELS_DEV` in the test.
5. Run `cargo test -p radian-host --test opencode_go_models`.
