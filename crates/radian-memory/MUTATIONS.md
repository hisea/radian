# `radian-memory` mutation record (ADR 0021 M1)

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
