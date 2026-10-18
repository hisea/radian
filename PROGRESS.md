# progress — patched multi-edit + retrievable foreground shell output

Worktree `<home>/Code/radian/.radian-worktrees/multi-edit-output`, branch
`pi/multi-edit-output`. Commits: `5eaafae` (slice 1), `2d5a356` (slice 2),
`baa47d5` (slice 2 follow-up: keep `job_output`'s read-failure code),
`2a289b4` (review fix: private root, retention cleanup). Nothing pushed or merged.

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

### Design note

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
- **Storage / retention bounds.** The retained files live **directly in the caller-supplied,
  session-owned jobs directory** (0700, outside the workspace, on the sandbox deny-read list
  through the sessions dir, ADR 0020 decision 3) as `out-*.out`, beside the registry's
  `job-*.out`. There is no `foreground` subdirectory. Bounds: `max_foreground_retained_bytes`
  per item (8 MiB), `max_foreground_session_bytes` (32 MiB) and `max_foreground_items` (32) per
  session; keeping past the session bounds evicts the oldest item, whose handle then reads as
  unknown. When the per-item cap cuts a file the shell result says how many bytes are retained
  and how many are **not recoverable**.
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

## Review fixes (this commit)

The reviewer found three blocking defects; all three are fixed here.

1. **No-host sessions retained into the workspace.** `CodingTools::open_session`'s fallback
   (`Jobs::new(<ws>/.radian/jobs)`) made `ForegroundOutputs` write `out-*.out` under the
   workspace, where the model's `read` could reach it. Fixed by design: **foreground retention is
   enabled only when the provider has a caller-supplied private directory** (`with_jobs`). A
   `with_sandbox`-only provider has no `shell_output`, no handle, and writes nothing; the
   ordinary truncation notice still says bytes were discarded. The host path is unchanged
   (`build_coding_tools` always supplies the session jobs dir), so the TUI/host always has the
   tool. Exact embedder behavior: *retention (and therefore `shell_output` and
   `output_handle`) exists iff the embedder called `CodingTools::with_jobs`; without it the tool
   is not advertised and no file is created.*
2. **Retention leaked on an error path.** `shell::run` created the file before `execute(...)?`,
   so a spawn/build error left it behind. Fixed with a `Drop` guard on `RetentionInner`: if
   `ForegroundOutputs::finish` never committed the item, the last clone removes the file.
   Synchronous only; on a panic unwind it leaves the file (no I/O on the panic path) and the
   registry state still drops.
3. **Privacy of the root.** The private root is now held as an `O_DIRECTORY|O_NOFOLLOW` fd
   (`output::PrivateDir`); every file is created/read with `openat` (`O_CREAT|O_EXCL|O_NOFOLLOW`,
   0600), and a symlink at the root is refused. This closes the `ensure_private_dir`-follows-a-
   symlinked-`foreground` gap the reviewer noted. The root is created one level (its parent is
   the caller's to create), so a workspace fallback cannot reappear.

### Mutations (in `crates/radian-coding/MUTATIONS.md`)

- `2d5a356` slice: SF-1 resolve a handle as a path; SF-2 ignore the per-item cap; SF-3 never
  remove an untruncated file.
- review fix: RF-1 retain in the workspace fallback again → `the_no_host_provider_retains_nothing_in_the_workspace`;
  RF-2 drop the cleanup → `a_spawn_failure_leaves_no_retained_file`; RF-3 follow a symlinked root →
  `a_planted_root_symlink_cannot_redirect_output`. RF-2's first run survived (the test's jobs
  parent did not exist, so no file was created); the test now creates the session tree first and
  the mutation fails on the leftover file.

## Remaining scope / ADR question (not done here)

The owner's original request spoke of a **common retention/truncation/read model and explicit
retention/total bounds for foreground *and* background output**. This branch implements the
**foreground** half as a first slice. Background `job_output` files remain **full, unbounded
history** in the session jobs directory, exactly as ADR 0037 chose; only their *readers* and
creator are shared. Bounding background output would change ADR 0037 and needs an decision,
so it is **not** done here and this must not be reported as the whole requirement complete.
Foreground retention is intentionally the first slice.

## Verification (committed state)

- `cargo fmt --all -- --check` → 0
- `cargo clippy --workspace --all-targets -- -D warnings` → 0
- `cargo test --workspace --locked --no-fail-fast` → 0
- `cargo test -p agent-core --locked --features journal-selfcheck --no-fail-fast` → 0
- `cargo clippy -p agent-core --all-targets --features journal-selfcheck -- -D warnings` → 0
- `bash docs/schema/check-fixtures.sh` → 0
- `bash docs/schema/check-config-fixtures.sh` → 0
- `cargo +1.88 check --workspace --all-targets` → 0 (MSRV 1.88)
- `cargo test -p radian-coding --test shell_output` run five times → 0 each
- Provider contract suites not run: no provider adapter changed.

## Uncertainties

- On resume, a handle from the previous process reads as `unknown_output` (the registry is
  per-process). This is deliberate and explicit, not silent.
- `shell_output` has no permission rule of its own; like the `job_*` tools it falls through to
  the profile's default `ask`. Reading session-private output grants nothing in the workspace.
- The retained file is removed by `shutdown_outputs` on the normal close path; on a panic it is
  left in the session-private directory (the registry state still drops). It is never a
  model-readable path.
