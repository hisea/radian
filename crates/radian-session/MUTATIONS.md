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
## Phase 7: corruption fuzz and crash injection

Base: `015ffbb` · one-shot worktree `/tmp/fuzz-mut` (created from HEAD, deleted after) · a
private `CARGO_TARGET_DIR` so no mutation artifact can leak into the main worktree's build ·
each mutation run with `cargo test -p radian-session --no-fail-fast`, then `git checkout --.`;
`git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | reader: a torn tail is treated as a committed event (`break` → `line.push(b'\n')`) | `read_fixtures::a_torn_tail_is_dropped_at_read_time`, `index::a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append`, `index::a_deleted_index_rebuilds_to_the_same_rows`, `findings::a_crash_mid_replacement_is_refused_as_an_incomplete_replacement` | ✅ failed |
| M2 | reader: a line that is not UTF-8 or not JSON is silently skipped (and `seq` advanced) | `read_fixtures::a_bad_json_line_names_its_line_number`, `fuzz::corruption_replay_is_total_and_never_writes_the_file` | ✅ failed |
| M3 | reader: an unknown `critical:true` event is skipped instead of refused | `read_fixtures::an_unknown_critical_event_refuses_the_file_with_line_and_event`, `fuzz::corruption_replay_is_total_and_never_writes_the_file` | ✅ failed |
| M4 | writer: `resume` does not truncate the torn tail before appending | `append::a_torn_tail_is_truncated_before_appending`, `crash_inject::every_partial_line_is_truncated_before_the_next_append`, `index::a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append`, `write::the_new_method_truncates_a_torn_tail_like_every_other_line` | ✅ failed |
| M5 | reader: the read truncates the torn tail in place (writes the file) | `fuzz::corruption_replay_is_total_and_never_writes_the_file` | ✅ failed |
| M6 | index: `message_index` is no longer recorded (index disagrees with the reader) | `fuzz::corruption_replay_is_total_and_never_writes_the_file`, `index::every_indexed_field_is_derivable_from_the_jsonl`, and three more index tests | ✅ failed |
| M7 | writer: `resume` reuses the last `seq` instead of continuing | `crash_inject::every_partial_line_is_truncated_before_the_next_append`, `append::a_torn_tail_is_truncated_before_appending`, `e2e::repair_write_back_appends_normally_and_keeps_seq_continuous`, and four more | ✅ failed |

7/7 caught, no misses. M1–M4 are the brief's required mutations; M6/M7 were added because the
fuzz's index-vs-reader comparison and the crash test's `next_seq` assertion are the parts of
this slice's own tests that do new work.

**Where the catch came from.** M1–M4 were caught by an *existing* test as well as the new
ones, which is the right shape: the new tests do not replace the pinned regressions, they widen
the input space (every line × every partial length; every mutation class × every seed). M5 is
caught only by the fuzz's byte-for-byte before/after assertion, and M6 only by the fuzz's
index comparison plus the index suite — no existing test would have noticed a reader that
"helpfully" rewrites the file.

**A note on M2's size.** The mutation is not a one-line change: skipping a line has to advance
`expect_seq` and `line_offset` too, or the next line fails `BadSeq` and the "silent" skip is
not silent at all. The faithful mutation is the one in the table; it is caught by the fuzz's
"an event that vanished must appear in `skipped`" rule.

## Phase 7 fixes: read/write consistency

Base: `1953b12` · one-shot worktree `/tmp/sessfix-mut` (created from HEAD, deleted after) · a
private `CARGO_TARGET_DIR` · each mutation run with `--no-fail-fast` against
`cargo test -p radian-session` and `cargo test -p agent-core --features journal-selfcheck`,
then `git checkout --.`; `git status --short` empty at the end.

| # | Mutation (each fix reverted) | Caught by | Result |
|---|---|---|---|
| M1 | reader: count every later `MessageAppended` into the replacement segment again | `replace::a_normal_append_after_a_complete_replacement_is_not_counted_as_part_of_it`, `replace::replacements_and_the_appends_between_them_fold_to_the_last_revision`, `fuzz::the_fuzz_base_is_a_valid_v1_session` | ✅ failed |
| M2 | reader: a short replacement at end of file is refused again | `findings::a_crash_mid_replacement_is_recovered_as_the_written_prefix`, `replace::resume_continues_after_a_short_replacement`, `fuzz::corruption_replay_is_total_and_never_writes_the_file` | ✅ failed |
| M3 | writer: `scan_lines` skips an empty line again | `findings::resume_refuses_an_interior_empty_line` | ✅ failed |
| M4 | reader: an orphan tool marker is accepted again | `findings::a_marker_whose_call_id_never_appeared_is_refused` | ✅ failed |
| M5 | reader: the torn-tail check is disabled (`if false && ...`) | `fuzz::corruption_replay_is_total_and_never_writes_the_file`, `read_fixtures::a_torn_tail_is_dropped_at_read_time`, `index::a_deleted_index_rebuilds_to_the_same_rows`, `index::a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append`, `findings::a_crash_mid_replacement_is_recovered_as_the_written_prefix` | ✅ failed |

5/5 caught. **`agent-core --features journal-selfcheck` failed on none of them**: Core's
`replay`/`repair` never consult `TranscriptReplaced.len` (they only check `index` contiguity
and reset at a marker), and the selfcheck slices from the last replacement and replays, so
finding 0 had no Core equivalent. The other three fixes are reader/writer-only, so Core is
unaffected by construction.

M5 is the mutation the brief asked for: with the tightened fuzz oracle (a torn tail must read
back, not error) the fuzz now catches a reader that stops dropping the torn tail. Before the
tightening it was only caught by the `read_fixtures`/`index` tests.

## Phase 7 fixes, round 2: a short replacement before a new marker

Base: `d5d936d` · one-shot worktree `/tmp/sessfix2-mut` (created from HEAD, deleted after) · a
private `CARGO_TARGET_DIR` · each mutation run with `--no-fail-fast` against
`cargo test -p radian-session` and `cargo test -p agent-core --features journal-selfcheck`,
then `git checkout --.`; `git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M6 | reader: a short segment before a new `TranscriptReplaced` is refused again | `findings::a_short_replacement_followed_by_another_replacement_reads_back`, `replace::resume_continues_after_a_short_replacement_is_replaced_again` | ✅ failed |
| M2 | reader: a short segment at end of file is refused again (recheck after the round-2 change) | `findings::a_crash_mid_replacement_is_recovered_as_the_written_prefix`, `replace::resume_continues_after_a_short_replacement`, `fuzz::corruption_replay_is_total_and_never_writes_the_file` | ✅ failed |

2/2 caught; `agent-core --features journal-selfcheck` failed on neither, as before. The M6
catch is exactly the two new tests, so the decision "accept a short segment before a new
marker" is pinned.

## Phase 7 privacy: owner-only modes and diagnostic redaction

Base: `90a91a6` · one-shot worktree `/tmp/privacy-mut` (created from HEAD, deleted after) · a
private `CARGO_TARGET_DIR` · each mutation run with `--no-fail-fast`, then `git checkout --.`;
`git status --short` empty at the end. The permission tests live in
`crates/radian-session/tests/privacy.rs` and `crates/agent-cli/tests/cli.rs`; the redaction
tests in `crates/agent-cli/tests/cli.rs`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | the `.jsonl` is created with the default mode (`.mode(0600)` removed) | `privacy::a_new_session_file_is_owner_only`, `cli::a_new_session_directory_and_file_are_owner_only` | ✅ failed |
| M2 | the session directory is created with the default mode (`DirBuilder::mode` removed) | `privacy::a_new_session_directory_is_owner_only`, `cli::a_new_session_directory_and_file_are_owner_only` | ✅ failed |
| M3 | `wide_permissions_warning` always returns `None` | `privacy::an_existing_wide_{session_directory,session_file,index}_is_reported_and_not_changed`, `cli::a_wide_session_directory_is_warned_about_and_not_changed` | ✅ failed |
| M4 | the index database is created with the default mode (`ensure_private_file` removed) | `privacy::the_index_database_and_its_wal_are_owner_only` | ✅ failed |
| M5 | `redact_base_url` returns the raw URL | `cli::a_base_url_with_credentials_is_redacted_in_errors` | ✅ failed |
| M6 | the `--search` snippet skips `redact` | `cli::search_redacts_the_key_from_a_snippet` | ✅ failed |

6/6 caught. M1–M3 are the mutations the brief asked for; M4–M6 cover the other two new
behaviours (the SQLite sibling modes and the two redaction fixes). The `-wal`/`-shm` modes are
inherited from the database file, so M4 fails on `index.db` first and the siblings never get a
chance to differ.

## ADR 0019：reader / writer 与唯一状态机

Base: `8858046` · one-shot worktree `/tmp/0019-mut`（detached，跑完删除）· 每个变异 `--no-fail-fast`
跑完即 `git checkout --.`，结束时 `git status --short` 为空。Core 侧三项（提交、待定段前缀、
段内第二个 marker）记在 `crates/agent-core/MUTATIONS-0019.md`。

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M4 | writer `resume` 不写 `TranscriptReplacementAborted`（`let _ = abort_revision`） | `replace::resume_aborts_a_pending_segment_and_the_next_append_is_normal` | ✅ failed |
| M5 | writer `resume` 只要有未提交段就写 abort（`.last()` 取代 `.find(EndOfFile)`） | `replace::resume_does_not_repeat_the_abort` | ✅ failed |
| M7 | reader 绕过状态机（`fold_push` 丢弃 `fold.push` 的错误） | `findings::a_second_replacement_inside_a_pending_segment_is_refused`、`replace::a_message_beyond_the_declared_len_is_corruption`、`a_commit_without_a_replacement_is_corruption`、`a_mismatched_commit_is_corruption`、`a_mismatched_abort_is_corruption` | ✅ failed |

3/3 caught。M7 一次改 reader，五个 corruption 用例同时失败，证明 reader 的结构检查确实走
Core 的状态机而不是第二套规则。只读打开不写盘这一条没有做人工变异：`a_crash_mid_replacement_recovers_the_old_history`
在 `Session::open` 前后比较文件字节，且 `resume_*` 用例在 resume 前比较字节；这里只记录未变异。

## ADR 0021 M1 第三片：`MemoryInjected`（2026-10-13）

Base `2d01d9b` · 一次性 worktree `/tmp/memsvcb-mut` · `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M13 | `MemoryInjected` 的 `critical` 改为 `true` | ✅ 抓住 | `runtime_events::memory_injected_round_trips_without_content_and_is_not_critical` |

## ADR 0019 收尾：resume 先校验后截尾

Base: `3035930` · one-shot worktree `/tmp/0019b-mut`（detached，跑完删除）· 每个变异 `--no-fail-fast`
跑完即 `git checkout --.`，结束时 `git status --short` 为空。

| # | Mutation | Caught by | Result |
|---|---|---|---|
| N1 | `resume` 恢复旧顺序（先 `set_len` 截 torn tail，后 `Session::open` 校验） | `replace::resume_refuses_a_corrupt_file_without_touching_it`（`replace-resume-corrupt-pending-event` 这一例：字节不再相同） | ✅ failed |
| N2 | `resume` 忽略写 abort 的错误（`let _ = journal.write_line(Aborted)`）并返回 `Ok` | `replace::resume_reports_a_failed_abort_write_and_appends_nothing` | ✅ failed |

2/2 caught。N2 的测试用 `testing` feature 的故障接缝（`fail_next_aborted_write()`，在
`write_locked` 真正 `write_all` 之前返回 `io::Error`），所以文件字节完全不变——测试断言的是
"不变"而不是"只多一段 torn tail"。跑 N2 用 `cargo test -p radian-session --features testing
--test replace`；默认 `cargo test --workspace` 不含该用例（接缝只在 `testing` feature 下编译）。

## compact-core：写者闸门（ADR 0018 §A）

Base: `b20cc3b` · 一次性检出 `/tmp/ccore-mutA`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| G1 | `append_runtime_event` 不等待段关闭（去掉闸门） | `gate::a_runtime_event_waits_for_the_replacement_segment_to_close`、`gate::a_failed_segment_write_fails_the_waiter_and_the_file_still_resumes` | ✅ failed |

1/1 抓住。两个测试都用 `testing` 接缝 `wait_for_gate_waiter()` 同步（外加 5 秒 watchdog 超时，
超时只是安全网）；去掉闸门后等待者不再注册，超时触发，测试失败而不是挂起。

## flake2：append 测试的 flock 继承窗口

不是代码变异，是一次**并发 flake 的修复前后测量**。根因：`flock` 属于打开文件描述；同一测试
进程里一个测试 `fork` 子进程时，子进程在 `[fork, exec)` 窗口继承父进程的 fd，父进程随后 close
再 reopen 同一文件时 `try_lock_exclusive` 看到假的 `Locked`。修法：`append.rs` 里一个进程级
`static SERIAL` 把「spawn 子进程的测试」与「持有/重开 journal 的测试」串行化（只有该二进制 spawn
子进程；每个集成测试文件是独立进程，跨二进制不会互相影响）。生产在启动时锁一次会话文件，任何子
进程创建之前，不受影响。

| 平台 / 命令 | 修复前 | 修复后 |
|---|---|---|
| Linux VM：`append <a_torn_tail…> <a_killed_writer…> --exact`，10 并行 ×5 轮（50 次） | 2 失败 | 0 失败 |
| Linux VM：整只 append 二进制，20 并行 ×3 轮（60 次） | （未单测） | 0 失败 |
| macOS：整只 append 二进制，20 并行 ×5 轮（100 次） | 1 失败 | 0 失败 |

`crates/radian-session/tests/append.rs` 的模块注释记了根因与生产为何不受影响。

## fork：ADR 0022 会话 fork

Base: `fcd0c26` · 一次性检出 `/tmp/fork-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| F1 | 允许 turn 中途 fork（去掉轮间边界检查） | `fork::a_seq_inside_a_turn_is_refused`、`a_seq_inside_a_replacement_segment_is_refused` | ✅ failed |
| F2 | 物化时不应用已提交的替换（`record_from_event` 丢掉 `TranscriptReplaced`，拿原始消息） | `fork::a_committed_compaction_is_in_effect_in_the_child` | ✅ failed |
| F3 | 先写父 `SessionForked` 再写子文件 | `fork::a_child_failure_leaves_the_parent_untouched` | ✅ failed |
| F4 | `SessionForked.parent_seq` 写成 `point.seq + 1` | `fork::the_parent_gains_only_the_forked_record_and_the_seqs_match` | ✅ failed |

4/4 抓住。

## tail：ADR 0023 U6 增量只读读取器

Base: `3653071` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short` 结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T1 | `LineScanner::push` 不校验 `seq` 连续（`if false && header.seq != self.expect_seq`） | `tail::a_bad_seq_line_is_refused_like_open` | ✅ failed |
| T2 | torn tail 推进偏移（`*offset += n` 后 `break`） | `tail::a_torn_tail_is_not_read_until_complete`、`tail::incremental_polls_equal_a_fresh_open_at_every_split` | ✅ failed |
| T3 | 每次 `poll` 重建 `LineScanner`（pending 状态不跨 poll 保留） | `tail::a_pending_segment_spans_polls`、`tail::a_torn_tail_is_not_read_until_complete`、`tail::a_replaced_or_truncated_file_resets`、`tail::incremental_polls_equal_a_fresh_open_at_every_split` | ✅ failed |
| T4 | 截断不触发 Reset（reset 条件去掉 `meta.len() < self.offset`） | `tail::a_replaced_or_truncated_file_resets` | ✅ failed |

4/4 抓住。T1 的校验与一次性 `Session::open` 共用同一个 `LineScanner`，所以变异同时也会让
`read_fixtures` 失败；这里记的是新增的 tail 侧测试。

## writer lock: the fork-to-exec false `Locked` (retry)

Base: `6aea27c` · worktree `.radian-worktrees/journal-lock` · each mutation applied, then
`cargo test -p radian-session --test append -- --exact <test> --nocapture` run with
`--no-fail-fast`, then `writer.rs` restored from a copy (`git status --short` clean after).

| # | Mutation | Caught by | Result |
|---|---|---|---|
| R1 | `open_locked` does not retry: a single `try_lock_exclusive`, exactly as before | `a_lock_inherited_by_a_forked_child_is_retried_and_succeeds` (its wait for a retry fails after the 5 s watchdog) | ✅ failed |
| R2 | the retry is unbounded (the 200 ms deadline check removed) | `a_held_lock_is_still_refused_after_the_retry_budget` (no `Locked` within the 2 s bound) | ✅ failed |

2/2 caught: R1 fails on the retry's absence, R2 on the budget's absence. The reproduction —
8 threads closing and reopening a journal while 4 threads spawn `git --version` — saw `13966`
false `Locked` in `24000` reopens before the fix and `0` in `72000` (3 × `24000`) after it.

## review-fixes-1 (2026-10-15): atomic fork child

One-shot detached worktree `.radian-worktrees/rf1-mutations` (from the branch HEAD, deleted
after); the mutation was restored with `git checkout --`, `git status --short` empty at the end.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RF1-FB | write the child directly to the final path and keep it on failure (no temp/rename) | caught (rc=101) | `fork::tests::a_failed_child_leaves_nothing_behind` (the half child existed) |

## ADR 0030 slice 1：`ForkParent.relation`、`TaskSpawned`/`TaskFinished`、`PermissionResolved.policy`

Base: `a4a00bf` · 在本任务工作树内逐条施加，改前把 `writer.rs`/`runtime.rs` 复制到
`/tmp/orch-s1-mut-backup/`，改后 `cp` 还原（本任务禁 `git checkout --`）；每条跑完后
`git status --short` 为空，`diff -q` 与副本一致。测试用 `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S1-M1 | writer：`ForkParent.relation` 加 `#[serde(skip_serializing)]`（relation 从线上消失） | `fork::a_task_child_records_relation_task`、`fork::the_parent_gains_only_the_forked_record_and_the_seqs_match` | ✅ failed |
| S1-M2 | writer：去掉 `#[serde(default = "default_relation")]`（没有 relation 的旧 parent 变报错而不是默认） | `read_fixtures::the_fork_fixtures_agree_with_the_reference_checker`（`missing field relation`） | ✅ failed |
| S1-M3 | runtime：`PermissionResolved` 移进 `critical() == true` 臂 | `permission_event::permission_resolved_round_trips_and_is_not_replayed` | ✅ failed |
| S1-M3b | runtime：`TaskSpawned`/`TaskFinished` 移进 `true` 臂 | `runtime_events::the_task_events_round_trip_and_are_not_critical` | ✅ failed |
| S1-M4 | runtime：`runtime_data` 的 `policy` 恒为 `None`（有 policy 也不写） | `permission_event::permission_resolved_round_trips_and_is_not_replayed` | ✅ failed |
| S1-M4b | runtime：去掉 `PermissionResolvedData.policy` 的 `skip_serializing_if`（写成 `"policy":null`） | `permission_event::permission_resolved_round_trips_and_is_not_replayed` | ✅ failed |

6/6 抓住。S1-M1/M2 是 brief 要求的「relation 掉线」「旧 parent 被拒」两条；S1-M3 是
「policy（即 PermissionResolved 事件）被标 critical」；S1-M4/M4b 是「有 policy 时省略」与
「None 时也序列化」两面。S1-M3b 是额外加的：两个新事件同样不能标 critical。

## ADR 0040 组提交（2026-10-16）

Base: `d2d0f7f` · worktree `.radian-worktrees/group-commit` · `writer.rs` 先 `cp` 到
`/tmp/gc-mut/writer.rs.bak`，每条变异施加后跑测试，再 `cp` 还原；结束时
`diff -q` 与副本一致、`git status --short` 为空。测试用 `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| GC1 | `must_sync` 去掉 `ToolExecutionStarted` | `a_crash_between_sync_points_keeps_the_invariant`（在工具已执行、其文件存在而 `ToolExecutionStarted` 不在盘上时断言失败，`radian-runtime/tests/group_commit.rs:196`）、`a_step_fsyncs_only_at_its_sync_points`，以及 lib 单测 `the_sync_set_is_exactly_the_boundaries` | ✅ failed |
| GC2 | `must_sync` 去掉 `TurnFinished` | `a_sync_point_makes_every_earlier_record_durable`（`TurnFinished` 一例）、`non_sync_records_ride_the_next_sync_point` | ✅ failed |
| GC3 | `must_sync` 去掉 `TranscriptReplacementCommitted` | `a_sync_point_makes_every_earlier_record_durable`（提交一例） | ✅ failed |
| GC4 | `seq` 计数器在写入成功之前前进（把 `next_seq = seq + 1` 挪到写入前） | `a_failed_write_does_not_burn_a_seq`（`next_seq` 在注入的失败后仍前进） | ✅ failed |

4/4 抓住。GC1 是 brief 要求的「工具派发前不 fsync」——它**不是**只靠参考回合的 fsync 计数
断言（那会让变异因"少了一次 fsync"这种元断言而失败），而是靠故障注入的不变式：在**每一条记录
边界**注入崩溃（是"每两个同步点之间"的超集），只要工具跑过、其副作用文件存在，其
`ToolExecutionStarted` 就必须在盘上。GC4 的变异必须放在失败接缝**之前**才被触发（接缝在
`write_locked` 顶部返回 `Err`），所以忠实变异是"在写入前就前进计数器"而不是"在 `write_all`
前一步前进"。

## ADR 0043 slice ⓪A：删掉编排后旧 journal 仍可读（2026-10-17）

`TaskSpawned`/`TaskFinished` 等编排事件从 `KNOWN_EVENTS` 移除后，带这些行的旧 journal 必须
按 ADR 0006 §7(d) 当作**未知非关键事件跳过**、会话照常打开。变异把 `reader.rs` 的跳过分支护成
「未知即拒」（`cp` 到 `/tmp/s0a-reader.rs.bak`，改后 `cp` 还原并 `cmp` 一致）。命令：
`cargo test -p radian-session --locked --test old_orchestration_journal`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| OA-1 | `reader.rs`：未知事件不论 `critical` 一律 `Err(UnknownCritical)`（不再 `skipped.push`） | ✅ 2 failed（`UnknownCritical { line: 4, event: "TaskSpawned" }`） | `an_old_lead_journal_with_task_events_still_opens_and_replays`、`the_resume_path_opens_the_old_journal_and_appends` |

## 会话存活探测 `session_is_live`（ADR 0043 D14 的 attach 用）

读侧探测：`File::open` + **共享** flock 立即释放；持有独占锁的 writer 在时得 `EWOULDBLOCK`
（活），否则不是（死）。只用共享锁，所以两个读者同时探测互不干扰，也绝不阻塞 writer。变异在
源码原地改，从 `/tmp/mut/` 的 `cp` 副本还原并以 `git diff --stat`
确认；运行带 `--no-fail-fast`。

命令：`timeout 300 cargo test -p radian-session --locked --no-fail-fast --lib the_liveness_probe`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| LIVE-1 | `session_is_live` 直接 `return false` | `writer::tests::the_liveness_probe_follows_the_writer_lock`（writer 持锁却说不是活） | ✅ 1 failed |
