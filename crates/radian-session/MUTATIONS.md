# `radian-session` mutation record (Phase 1 / A)

Base: `f350a13` · one-shot worktree `/tmp/session-mut` (created from HEAD, deleted after) · method: apply a mutation, run the listed test binary, expect a non-zero exit; `git checkout --` after each. `git status --short` was empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | writer: `fetch_add(1)` → `load` in `TranscriptJournal::append` (every record reuses one `seq`) | `write::a_session_file_has_the_v1_envelope_and_sequential_seqs` (`seq == i`); a reader replay would also hit `ReadError::BadSeq` | ✅ failed |
| M2 | writer: `#[serde(skip)]` on the envelope's `critical` field | `write::a_session_file_has_the_v1_envelope_and_sequential_seqs` (`v["critical"].is_boolean()`) | ✅ failed |
| M3 | writer: remove `file.sync_all()` (no fsync) | **nothing** — see below | ❌ not caught |
| M4 | writer: `resume` no longer truncates the torn tail | `append::a_torn_tail_is_truncated_before_appending` (the fragment survives / later corruption) | ✅ failed |
| M5 | reader: unknown event accepted even when `critical: true` | `read_fixtures::an_unknown_critical_event_refuses_the_file_with_line_and_event` | ✅ failed |
| M6 | writer: `try_lock_exclusive` result ignored (single-writer lock disabled) | `append::a_second_writer_in_this_process_is_refused` and `a_second_writer_in_another_process_is_refused` | ✅ failed |
| M7 | reader: `UnknownCritical` message drops `line {line}` | `read_fixtures::an_unknown_critical_event_refuses_the_file_with_line_and_event` (`text.contains("line 2")`) | ✅ failed |
| M8 | writer: `resume` always restarts `next_seq` at 1 instead of continuing from disk | `e2e::repair_write_back_appends_normally_and_keeps_seq_continuous`, `write::resuming_continues_the_sequence_and_keeps_the_session_id`, `append::a_torn_tail_is_truncated_before_appending` | ✅ failed |

## M3 (no fsync) is not observable in this test environment

Removing `sync_all()` leaves every test green, because:

- The torn-tail and crash-injection tests exercise **parsing** of whatever bytes exist, not durability; a killed process still leaves its written bytes in the page cache, visible to the next open on the same machine.
- No test can simulate a power loss or a kernel crash, which is the only event that distinguishes fsync from a plain `write`.

So this is an **equivalent mutation under the test environment**, not a test hole: fsync is intentionally kept (ADR 0001 D3 makes each `append` await the durable write), and the mutation record states plainly that no test can prove its presence. The measured cost it buys is in the report (~4 ms/append on this machine).

## A real bug the mutation work surfaced (kept as a regression test)

`append::a_torn_tail_is_truncated_before_appending` first failed for a different reason: after `resume` truncated the torn tail with `set_len`, the file cursor still pointed past EOF, so the next write produced a sparse hole (NUL bytes) and the following read failed with a JSON corruption. The fix is to open the file with `O_APPEND` so writes always land at the current end. That test now pins the behavior.

## Notes

- M1 was run against `read_fixtures` as well and did not fail there: that suite reads the repository's own fixture files, which are not produced by this crate's writer. The writer-side suite is the one that owns writer invariants.
- The lock guarantee is verified in two ways: two independent `File` handles in one process (kernel `flock` still conflicts) and a spawned child process that must report `LOCKED`.