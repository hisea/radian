# `agent-cli` sessions-on-disk mutation record (Phase 2 / B)

Base: `3451dc1` · one-shot worktree `/tmp/cli-mut` (deleted after) · each mutation run with
`cargo test -q -p agent-cli --test cli --no-fail-fast`; `git checkout --` after each;
`git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | `--resume` opens the session with an empty history (`Vec::new()` instead of the recovered one) | `resume_carries_the_previous_history` | ✅ failed |
| M2 | terminal input is enqueued with `source = System` instead of `User` | `a_new_session_writes_a_valid_jsonl` (asserts the durable `InboxEnqueued.source`) | ✅ failed |
| M3 | reasoning is printed to stdout with the answer (no `[reasoning]` split) | `reasoning_is_printed_apart_from_the_answer` | ✅ failed |
| M4 | the writer ignores `try_lock_exclusive`, so a second process opens the same session | `a_second_process_cannot_open_the_same_session` | ✅ failed |
| M5 | the `SessionCreated` snapshot carries the `--header` values | `the_session_file_contains_no_key_or_header_values` | ✅ failed |
| M6 | `--list` ignores the workspace filter (`list(None)`) | `list_shows_only_the_current_workspace` | ✅ failed |
| M7 | recovery does not run the core's `repair`, so an unpaired tool call is not classified | `a_killed_session_resumes_without_replaying_the_tool` | ✅ failed |
| M8 | recovery ignores the transcript when deciding admission (reverting the fix) | `a_killed_session_resumes_without_replaying_the_tool` (the input is replayed → an extra request → the mock runs out of replies) | ✅ failed |

8/8 caught, none equivalent.

## Bugs this slice found (kept as regression tests)

- **Ids collided across processes.** The inbox's numeric id counter restarted at 1 in a new
  process, so a resumed session saw the previous process's `inbox-1` and the new one's
  `inbox-1` as the same input. Fixed with a per-process id prefix; pinned by
  `radian-runtime::inbox::tests::two_inboxes_never_generate_the_same_id`.
- **`InputAdmitted` was written more than once per input.** `admit_seen` never recorded what
  it had written, so every later step in the turn wrote it again. Pinned by
  `radian-runtime::driver::each_input_is_admitted_once`.
- **A crash between "committed at the start of the step" and "admission recorded" replayed
  the input.** Recovery now treats the transcript (via `meta.id`) as authoritative and writes
  the missing record; pinned by
  `radian-runtime::recovery::an_input_in_the_transcript_without_an_admission_record_is_not_replayed`.
