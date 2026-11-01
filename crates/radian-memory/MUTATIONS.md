# `radian-memory` mutation record (ADR 0021)

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

Base: `75e31a5` · one-shot worktree `/tmp/memstore-mut` (created from HEAD, deleted after) ·
each mutation run with `cargo test -p radian-memory --no-fail-fast`, then `git checkout -- .`;
`git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | `create` overwrites an existing Active key instead of a `KeyConflict` | `store::the_same_key_cannot_be_created_twice_in_a_scope`, `store::an_agent_create_on_a_user_key_is_a_key_conflict`, `multiprocess::two_processes_creating_the_same_key_yield_one_conflict` | ✅ failed |
| M2 | an agent may revise/archive/forget a user record (`guard_source` removed) | `store::an_agent_cannot_revise_archive_or_forget_a_user_record` | ✅ failed |
| M3 | `MemoryForgotten` no longer removes the record on replay | `forget::forget_removes_the_record_from_every_view`, `forget::a_forgotten_record_stays_gone_after_reopening`, `forget::purge_removes_the_forgotten_content_bytes`, `forget::purge_is_atomic_at_every_step`, `forget::a_purged_log_replays_the_live_records` | ✅ failed |
| M4 | `purge` writes the log in place (no temp + atomic rename) | `forget::purge_is_atomic_at_every_step` | ✅ failed |
| M5 | `confirm` rewrites `source` to `User` | `store::confirm_keeps_the_source_and_records_who_and_when` | ✅ failed |
| M6 | `revise`/`archive`/`restore`/`confirm` ignore `expected_revision` | `store::a_stale_revision_does_not_overwrite`, `multiprocess::two_processes_revising_the_same_revision_yield_one_conflict` | ✅ failed |
| M7 | `purge` skips the directory fsync | **nothing** — see below | ❌ not caught |

6/7 caught. M1 and M6 are caught at both the unit and the two-process level; M3/M4 show the
Forget/Purge atomicity tests do real work (M4's first form renamed the file onto itself and
deadlocked on the writer lock — a hang is a failure too, but the record uses the cleaner
write-in-place form so the catch is an assertion).

## M7 (no directory fsync) is not observable in the test environment

Skipping the directory fsync after the rename leaves every test green. The rename is already
visible in the page cache, and only a power loss or kernel crash distinguishes "renamed" from
"renamed and durable". No test can simulate that. It is an **equivalent mutation under the
test environment**, not a test hole: the fsync stays because ADR 0021 D3 requires the rewrite
to be durable (crash at any step ⇒ old log or new log, never a partial file), and it is the
same class as `radian-session`'s documented `sync_all` mutation.

## Round 2: the review's two correctness defects and the decisions

Base: `0b8578d` · one-shot worktree `/tmp/memstore2-mut` (deleted after) · each mutation run with
`cargo test -p radian-memory --no-fail-fast`, then `git checkout -- .`; `git status --short`
empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M8 | `open_locked` stops revalidating `(dev, ino)` | `purge_race::a_writer_that_opened_the_old_inode_retries_after_a_replace` | ✅ failed |
| M9 | `split` drops the target lock before the write | `split::a_move_holds_the_target_lock_between_the_check_and_the_write` | ✅ failed |
| M10 | a half-finished move is not recognized on re-run | `split::a_half_finished_move_is_completed_on_a_rerun` | ✅ failed |
| M11 | an unknown `kind` makes the scope fail to open | `crash::an_unknown_kind_is_forward_compatible` | ✅ failed |
| M12 | an import `key` change is silently applied | `markdown::an_import_that_changes_key_or_kind_is_rejected_not_silently_applied` | ✅ failed |

5/5 caught. M8's test is the deterministic form of the reviewer's repro (the replace happens in
a thread-local open hook, between `open()` and the lock); M9 uses the move hook to check the
target log is locked at the check-to-write point. M7 from round 1 (the directory fsync) stays
as the documented equivalent mutation.

## TK0268: the file store and the log migration

Base: this branch's code commit · each mutation run with
`cargo nextest run -p radian-memory --locked --profile sandbox --test <binary> -E 'test(<name>)'`,
then restored from `.radian/files.rs.bak`; `git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0268-M1 | `touch_last_used` rewrites `modified` too | `files::touch_last_used_changes_only_the_last_used_line` | ✅ failed |
| TK0268-M2 | the migration keeps Archived records | `migrate::two_active_one_archived_and_one_forgotten_become_two_files_and_an_index` | ✅ failed |
| TK0268-M3 | the `type` closed-set check is removed from the reader | `files::a_type_outside_the_closed_set_is_unreadable_and_reports_its_line` | ✅ failed |

3/3 caught.

## Re-review: quoted YAML, a fixed source, the directory race, the docs drift test, description bytes

Base: this branch's rework · each mutation run with
`cargo nextest run -p radian-memory --locked --profile sandbox --test <binary> -E 'test(<name>)'`,
then restored from `.radian/files.rs.bak` and `.radian/fs.rs.bak`; `git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0268-M4 | `write` takes `source` from the new record on replacement | `files::a_write_keeps_the_source_of_an_existing_record` | ✅ failed |
| TK0268-M5 | `parse_scalar` treats a quoted scalar verbatim | `files::a_quoted_yaml_scalar_reads_as_its_value` | ✅ failed |
| TK0268-M6 | the migration trims the first content line | `migrate::a_description_is_the_first_content_line_cut_at_120_characters` | ✅ failed |
| TK0268-M7 | `create_or_accept` returns the race's `AlreadyExists` error | `fs::tests::a_directory_a_racing_caller_created_is_not_an_error` | ✅ failed |
| TK0268-M8 | `RECORD_TYPES` gains a value the docs do not list | `files::the_documented_closed_sets_are_the_constants` | ✅ failed |

5/5 caught.

## TK0274: the file store's rename rewrites the name line (2026-10-31)

Branch `work/TK0274-memory-commands`. The mutation was applied in place and the named test run
with `cargo nextest run -p radian-memory --locked --profile sandbox --test files -E
'test(<name>)'`, then restored from `.radian/mut-backup/files.rs`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0274-M1 | `replace_name` returns the text unchanged, so the new file keeps the old `name:` | ✅ 1 failed (rc=100) | `rename_rewrites_only_the_name_line_and_keeps_the_rest` (reading the new stem fails with "name ... does not match the file stem") |
