# progress — patched multi-edit + retrievable foreground shell output

Worktree `<home>/Code/radian/.radian-worktrees/multi-edit-output`, branch
`pi/multi-edit-output`.

## Slice 1 — atomic multi-edit `patch` — DONE (this commit)

- `patch` accepts `{path, edits: [{old, new, replace_all?}]}` next to the legacy
  `{path, old, new, replace_all?}`. Every edit is located against the original bytes before any
  write; overlapping/identical ranges are rejected naming both edit numbers; the combined result
  is built in memory, checked against `max_write_bytes`, then written once through the same fd
  (`O_NOFOLLOW` walk, CRLF preserved). Legacy result shape `{path, replaced, matched, hunks}` and
  text are unchanged; the new form adds `edits`, per-edit `results`, and flattened `hunks`.
- Tests: `cargo test -p radian-coding --test patch --locked --no-fail-fast` → 23 passed.
- Mutations ME-1 (write first edit before validating the rest) and ME-2 (skip overlap check)
  recorded in `crates/radian-coding/MUTATIONS.md`.
- Verified: `cargo fmt --all -- --check`, `cargo clippy -p radian-coding --all-targets -- -D
  warnings`, `cargo test -p radian-coding --locked --no-fail-fast` (exit 0).

## Slice 2 — foreground shell output retrieval — NEXT

Design note to be written here before coding (handle format/lifetime, tool schema and modes,
storage/retention bounds, reuse vs separation from job output, ADR 0037/0039 security).
