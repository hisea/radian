# progress — patched multi-edit + retrievable foreground shell output

Worktree `<home>/Code/radian/.radian-worktrees/multi-edit-output`, branch
`pi/multi-edit-output`.

## Slice 1 — atomic multi-edit `patch` — DONE (commit `5eaafae`)

- `patch` accepts `{path, edits: [{old, new, replace_all?}]}` next to the legacy
  `{path, old, new, replace_all?}`. Every edit is located against the original bytes before any
  write; overlapping/identical ranges are rejected naming both edit numbers; adjacent ranges are
  allowed; the combined result is built in memory, checked against `max_write_bytes`, then
  written once through the same fd (`O_NOFOLLOW` walk, CRLF preserved). Legacy result shape
  `{path, replaced, matched, hunks}` and text are unchanged; the new form adds `edits`, per-edit
  `results`, and flattened `hunks`.
- Tests: `cargo test -p radian-coding --test patch --locked --no-fail-fast` → 23 passed.
- Mutations ME-1 (write the first edit before validating the rest) and ME-2 (skip the overlap
  check) recorded in `crates/radian-coding/MUTATIONS.md`.

## Slice 2 — foreground shell output retrieval — DONE (this commit)

### Design note (written before coding the tool)

- **Handle format / lifetime.** `out-<16-hex nonce>-<n>`. The nonce is a random `u64` from
  `std::collections::hash_map::RandomState` (no new dependency), fresh per `ForegroundOutputs`;
  `n` is a per-registry counter. The registry is created per `CodingSession`, so two sessions in
  one process never share a handle even when they share the file directory. Lifetime is the
  session process: a resumed session starts a fresh registry, so an old handle reads as
  `unknown_output` (never a path). A handle is opaque — it carries no path bytes.
- **Tool schema / modes.** New built-in `shell_output { handle, mode?, tail_lines?, from_line?,
  max_lines?, pattern? }`. `mode` names the read (`tail` / `range` / `pattern`); without it, give
  at most one of the reads. The file arguments are the **same** `output::FileArgs` / `FileMode` /
  line readers `job_output` uses, so the two tools cannot drift. `shell_output` requires a read;
  `mode: new` is not offered and is refused.
- **Storage / retention bounds.** Files live under `<jobs_dir>/foreground/` (the session's own
  0700 directory, outside the workspace) and are created by the shared `output::open_output_file`
  (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600). Bounds: `max_foreground_retained_bytes` per item (default
  8 MiB), `max_foreground_session_bytes` (32 MiB) and `max_foreground_items` (32) per session;
  keeping past the session bounds evicts the oldest item, whose handle then reads as unknown.
  When the per-item cap cuts a file the shell result says exactly how many bytes are retained
  and how many are **not recoverable**.
- **Reuse vs separation from job output.** The line readers, the `FileMode` argument handling
  and the hardened file creator are shared in `src/output.rs`; the foreground registry is a
  separate object (`src/foreground.rs`) with its own handle space and no process/lifecycle, so a
  completed foreground command is never a job, `job_list` never shows it, and no job-notification
  path is touched.
- **ADR 0037/0039 semantics.** Same private-directory and `O_CREAT|O_EXCL|O_NOFOLLOW` standard
  as job output (ADR 0039 decision 1); the directory is the session's, never a workspace path. No
  path is ever put in the model-facing text, structured result, logs or errors — the model only
  ever presents the opaque handle. A non-truncated command's file is removed and gets no handle,
  so there is no unnecessary artifact. `shutdown_outputs` drops the registry and removes the
  files on the normal close path; the panic path does no file I/O (state drops with the session).
- **No ADR change needed:** this implements the existing intent (bounded, session-private output
  ADR 0039 already establishes for job output) for the foreground case; it does not weaken 0037
  or 0039.

### Implementation

- `src/output.rs` (new): shared `open_output_file`, `open_output_read`, `ensure_private_dir`,
  `count_lines`, `read_range`, `read_pattern`, `read_file`, `FileArgs`, `FileMode`, renderers.
  `src/jobs.rs` now delegates its full-file reads to it (no behaviour change).
- `src/foreground.rs` (new): the registry, the retention sink, the `shell_output` tool.
- `src/shell.rs`: `StreamSink` tees each stream to the retention until promotion; `report` adds
  the handle and the cut note; a non-truncated run deletes the file; an adopted run discards it.
- `src/provider.rs`: per-session `ForegroundOutputs` over `jobs.dir()/foreground/`; the spec and
  dispatch; `shutdown_outputs`. `Limits` gains the three retention fields.
- `radian-host`: `builtin_tools` lists `shell_output`; `close` calls `shutdown_outputs`.

### Tests

`cargo test -p radian-coding --test shell_output --locked --no-fail-fast` → 9 passed; existing
`--test shell`, `--test jobs`, `--test job_output_file`, `--test job_output_security` stay green.
Mutations SF-1 (resolve a handle as a path) and SF-2 (ignore the per-item cap) are recorded in
`crates/radian-coding/MUTATIONS.md`, each caught by a named test.
