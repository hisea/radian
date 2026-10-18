# progress — patched multi-edit + retrievable foreground shell output

Worktree `<home>/Code/radian/.radian-worktrees/multi-edit-output`, branch
`pi/multi-edit-output`. Two commits: `5eaafae` (slice 1), `2d5a356` (slice 2),
`baa47d5` (slice 2 follow-up: keep `job_output`'s read-failure code). Nothing
pushed or merged.

## Slice 1 — atomic multi-edit `patch` — DONE (`5eaafae`)

- `patch` accepts `{path, edits: [{old, new, replace_all?}]}` next to the legacy
  `{path, old, new, replace_all?}`. Every edit is located against the original bytes before any
  write; overlapping/identical ranges are rejected naming both 1-based edit numbers; adjacent
  ranges are allowed; the combined result is built in memory, checked against `max_write_bytes`,
  then written once through the same fd (`O_NOFOLLOW` walk, CRLF preserved). Legacy result shape
  `{path, replaced, matched, hunks}` and text are unchanged; the new form adds `edits`, per-edit
  `results`, and flattened `hunks`.
- Mutations in `crates/radian-coding/MUTATIONS.md`: ME-1 (write the first edit before validating
  the rest) caught by `a_missing_second_edit_leaves_the_first_unapplied`; ME-2 (skip the overlap
  check) caught by `overlapping_ranges_are_refused` and `identical_ranges_are_refused`.

## Slice 2 — foreground shell output retrieval — DONE (`2d5a356`, `baa47d5`)

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
  0700 directory, outside the workspace, on the sandbox deny-read list through the sessions dir
  ADR 0020 decision 3) and are created by the shared `output::open_output_file`
  (`O_CREAT|O_EXCL|O_NOFOLLOW`, 0600). Bounds: `max_foreground_retained_bytes` per item (8 MiB),
  `max_foreground_session_bytes` (32 MiB) and `max_foreground_items` (32) per session; keeping
  past the session bounds evicts the oldest item, whose handle then reads as unknown. When the
  per-item cap cuts a file the shell result says how many bytes are retained and how many are
  **not recoverable**.
- **Reuse vs separation from job output.** The line readers, `FileMode` handling and the hardened
  file creator are shared in `src/output.rs`; the foreground registry is a separate object
  (`src/foreground.rs`) with its own handle space and no process/lifecycle, so a completed
  foreground command is never a job, `job_list` never shows it, and no job-notification path is
  touched.
- **ADR 0037/0039 semantics.** Same private-directory and `O_CREAT|O_EXCL|O_NOFOLLOW` standard as
  job output (ADR 0039 decision 1). No path is ever put in the model-facing text, structured
  result, logs or errors. A non-truncated command's file is removed and gets no handle.
  `shutdown_outputs` drops the registry and removes the files on the normal close path; the panic
  path does no file I/O (state drops with the session). No ADR was changed: this implements
  0039's existing intent for the foreground case.

### Mutations (in `crates/radian-coding/MUTATIONS.md`)

- SF-1 resolve a handle as a path → caught by `a_path_like_handle_cannot_open_a_file` and
  `a_handle_from_another_session_is_unknown` (plus two more).
- SF-2 ignore the per-item cap → caught by `the_retained_file_is_capped_and_says_so`.
- SF-3 never remove an untruncated command's file → caught by
  `a_non_truncated_command_leaves_no_handle_or_file`.
- The shared creator is pinned by the `output.rs` unit test and `job_output_security`.

## Verification (committed state, `baa47d5`)

- `cargo fmt --all -- --check` → 0
- `cargo clippy --workspace --all-targets -- -D warnings` → 0
- `cargo test --workspace --locked --no-fail-fast` → 0
- `cargo test -p agent-core --locked --features journal-selfcheck --no-fail-fast` → 0
- `cargo clippy -p agent-core --all-targets --features journal-selfcheck -- -D warnings` → 0
- `bash docs/schema/check-fixtures.sh` → 0
- `bash docs/schema/check-config-fixtures.sh` → 0
- `cargo +1.88 check --workspace --all-targets` → 0 (MSRV 1.88)
- Provider contract suites not run: no provider adapter changed.

## Uncertainties

- On resume, a handle from the previous process reads as `unknown_output` (the registry is
  per-process). This is deliberate and explicit, not silent.
- `shell_output` has no permission rule of its own; like the `job_*` tools it falls through to
  the profile's default `ask`. Reading session-private output grants nothing in the workspace.
- The retained file is removed by `shutdown_outputs` on the normal close path; on a panic it is
  left in the session-private directory (the registry state still drops). It is never a
  model-readable path.
