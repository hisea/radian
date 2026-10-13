# 变异记录：radian-tui 骨架（Phase 4 / A）

日期：2026-10-12。变异在一次性检出 `/tmp/tui-mut`（`git worktree add`，HEAD = 本提交）里做：
改完跑对应测试、`git checkout` 恢复，提交前 `git status --short` 为空。测试用 `--no-fail-fast`。
全屏程序**从不**在测试运行器自己的终端里跑；`pty` 组经 `portable-pty` + `vt100` 模型。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| TM1 | 推理用正文样式渲染（`EntryKind::Reasoning => Style::default()`） | 推理与正文可区分 | `render.rs::reasoning_and_assistant_text_are_visually_distinct` FAILED |
| TM2 | `set_viewport` 永远把 `scroll` 拉到底（忽略 follow） | 往上翻后不被拽回底部 | `scroll.rs::scrolling_up_pauses_following_and_new_content_does_not_drag_back` FAILED |
| TM3 | 退出不写尾部换行（删掉 `restore` 里的 `\r\n`） | 退出后补换行（修 spike 的 `%`） | `pty.rs::pty_end_to_end_conversation_and_clean_exit` FAILED |
| TM4 | panic hook 不恢复终端（去掉 `guard.restore()`） | panic 路径恢复终端 | `pty.rs::pty_panic_path_restores_the_terminal` FAILED |
| TM5 | 运行中 Enter 也走 `Followup`（去掉 `Steer` 分支） | 运行中输入应 `steer` | `input.rs::enter_sends_a_followup_when_idle_and_a_steer_while_running` FAILED |
| TM6 | 去掉 `[interrupted]` 标记 | 半截消息有明确标记 | `render.rs::an_interrupted_prefix_is_marked` FAILED |
| TM7 | 提交消息时不丢弃 live 视图（`retain(!live)` 置空） | live delta 被提交消息替换、不重复 | `render.rs::live_deltas_are_replaced_by_the_committed_message_not_duplicated` FAILED |

结论：7 个变异全部被抓住，没有等价变异或测试漏洞。TM3/TM4 是 pty 层的断言（`\u{1b}[?1049l\r`
之后有换行、panic 后同 pty 里 `stty -a` 为 cooked），TM1/TM6/TM7 是 `TestBackend` 的样式/内容断言。

**一处实现决定记录**：panic hook 在恢复终端后调用 `std::process::exit(101)`，而不是让 panic
继续展开。原因是展开过程中 drop `tokio` runtime 与 session 会阻塞（评审时实测进程不退出），
而终端此时已经恢复。TM4 验证了恢复路径。

## Slice B: session resume, built-in tools and the approval dialog

Base: `a4e2ac1` · one-shot worktree `/tmp/tui-mut` (created from HEAD, deleted after) · each
mutation run with `cargo test -p radian-tui --test <file> <test> -- --exact --no-fail-fast`;
files restored after each; `git status --short` empty at the end.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| M1 | the panic path cleans up children | the hook calls `process::exit(101)` again (skipping the bounded runtime shutdown) | `pty_panic_path_does_not_orphan_a_child` |
| M2 | `--resume` renders the history | the `push_message` loop over the recovered history is removed | `pty_resume_renders_the_stored_history` |
| M3 | Ctrl-C cancels while an approval waits | the Ctrl-C branch is skipped when a prompt is up (so it is swallowed) | `pty_ctrl_c_while_waiting_for_approval_cancels_the_turn` |
| M4 | "always this session" is a real grant | `a` answers `AllowOnce` | `pty_approval_always_skips_the_next_prompt` |
| M5 | the prompt shows the resolved path | `approval_text` prints `resource` (the raw argument) | `the_prompt_shows_the_resolved_path_not_the_raw_one` |
| M6 | denying denies | `n`/Esc answer `AllowOnce` | `pty_approval_deny_is_fed_back_to_the_model` |
| M7 | every decision is durable | `JournalAudit::record` becomes a no-op | `pty_approval_allow_once_runs_the_tool` |

7/7 caught; no misses.

## Slice C: driver crash, shell and the context budget

Base: `6233e97` · one-shot worktree `/tmp/tui-c-mut` (created from HEAD, deleted after) · each
mutation run with `cargo test -p radian-tui --test <file> <test> -- --exact --no-fail-fast`;
files restored after each; `git status --short` empty at the end.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| M1 | the UI leaves when the driver dies | the `is_finished()` watch becomes `if false` | `pty_a_driver_panic_exits_orderly_and_restores_the_terminal` |
| M2 | a failed probe refuses | `build_sandbox` returns `Ok(None)` on a probe error (a downgrade) | `pty_a_failed_sandbox_probe_refuses_instead_of_running_unsandboxed` |
| M3 | an estimate is not a report | the `Estimated` wording loses `~` and says `(reported)` | `the_three_usage_sources_read_differently` |
| M4 | "continue" relaxes exactly one request | the driver *loads* the flag instead of swapping it (every later turn relaxed) | `pty_a_budget_stop_sends_nothing_and_continue_relaxes_once` |
| M5 | a compound command cannot be allowed | the allow rule matches `Any` instead of the command prefix | `pty_a_compound_shell_command_always_asks` |
| M6 | no window shows no number | `budget_status_text(None, Some(report))` prints the report | `no_window_says_the_guard_is_off_and_claims_no_number` |
| M7 | the shell prompt shows the whole command | `approval_text` truncates the resolved resource to 40 chars | `pty_shell_approval_shows_the_command_and_the_truncation_marker` |

7/7 caught; no misses.
