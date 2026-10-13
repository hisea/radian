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

## Slice D: MCP, shared header parsing and the full status bar

Base: `82f5849` · one-shot worktree `/tmp/tuid-mut` (created from HEAD, deleted after) · a
private `CARGO_TARGET_DIR` (`/tmp/tuid-mut-target`) so no mutation artifact can leak into the
main worktree's build · each mutation run with `--no-fail-fast`, then `git checkout -- .`;
`git status --short` empty at the end. The pty tests spawn the real binary under a pty.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| M1 | `{session_id}` expands to the session id | the placeholder replacement becomes `value.to_string()` | `headers::tests::a_value_can_reference_the_session_id`, `pty_a_header_value_expands_the_session_id`, `pty_a_resumed_session_header_uses_the_resumed_id` |
| M2 | a malformed header never echoes its value | the error appends `{item}` | `headers::tests::a_malformed_header_never_echoes_its_value`, `pty_a_malformed_header_error_does_not_echo_its_value` |
| M3 | `authorization` is refused | the refusal block is deleted | `headers::tests::an_authorization_header_is_refused_without_its_value`, `pty_an_authorization_header_is_refused_without_echoing_its_value` |
| M4 | a duplicate tool name fails instead of shadowing | the check in `merge_tool_specs` is deleted (MCP silently wins) | `tools::tests::a_duplicate_tool_name_is_refused_not_shadowed` |
| M5 | the workspace-write notice names the MCP sandbox gap | the sentence is deleted from the meaning line | `pty_the_workspace_write_notice_names_the_mcp_sandbox_gap` |
| M6 | no reported usage says `none`, never `0` | `usage none` becomes `usage 0/0` | `a_session_without_reports_says_none_and_never_invents_a_zero` |
| M7 | a narrow bar keeps the key segments | `mcp` and `context` swap places, so 60 columns drops `mcp` | `a_narrow_status_bar_keeps_the_key_segments_and_does_not_wrap` |
| M8 | the status reports what connected | the `McpStatus` assignment in `open_session` is deleted | `the_provider_reports_what_the_opened_session_connected` |

8/8 caught; no misses and no equivalent mutants.

**Note on M4's reachability.** With the current alias scheme (`server__tool`) a real MCP
server cannot produce a built-in tool name, so the collision is not reachable end-to-end. The
check stays as defence in depth and is tested directly through `merge_tool_specs`, which is
public for exactly that reason. A silent override would be the dangerous failure, so the rule
is held by a test rather than left implicit.

## D1/D2/D4：启动顺序、退出原因、panic 日志（2026-10-13）

一次性检出 `/tmp/dogfix-mut`（`git worktree add --detach HEAD`，HEAD = `fb6780b`），每个变异用
`git checkout --` 恢复，`git status --short` 为空；`cargo test -p radian-tui --test pty --no-fail-fast
<精确测试名>`。pty 测试连跑 3 次全绿（在未变异的 HEAD 上）。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M9 | 会话文件在检查之前创建（`open_journal` 提到 `resolve_workspace` 之前） | 抓住 (rc 101) | `pty_a_missing_workspace_is_a_workspace_error_and_leaves_no_session`（`left files behind: ["missing-workspace.jsonl"]`） |
| M10 | 工作区错误仍用 sandbox 文案（`resolve_workspace` 的 metadata 错误改成 "sandbox backend is unavailable..."） | 抓住 | `pty_a_missing_workspace_is_a_workspace_error_and_leaves_no_session`（"must not blame the sandbox"） |
| M11 | 正常退出不打印原因（`Ok(Ok(reason))` 分支不 `eprintln`） | 抓住 | `pty_clean_exit_prints_one_reason_line`（"no exit reason on the normal screen"） |
| M12 | panic 不写日志（`record_panic` 去掉文件追加） | 抓住 | `pty_a_panic_writes_a_redacted_panic_log`（"no panic log at ..."） |
| M13 | 日志不脱敏（日志行用 `full` 而不是 `redacted`） | 抓住 | `pty_a_panic_writes_a_redacted_panic_log`（断言日志不含 header 值/ key） |

5/5 抓住，无漏网、无等价变异。M9 是「先建会话再检查」的实例；M10 是「工作区错误仍走 sandbox 文案」；
M11 是「退出不打印原因」；M12 是「panic 不写日志」；M13 是「日志里带 header 值」。

## `--version` (2026-10-13)

Base HEAD = `8e5fc47`. Source restored from a copy in `target/mut-bak/` and compared with
`cmp` (`git status --short` shows only the intended edits afterwards); the mutation was run
in place with `cargo test -p radian-tui --test version --no-fail-fast`.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| V1 | `--version` exists and prints the package version | `version` removed from the `#[command]` attribute (clap then has no `--version` at all) | `version.rs::version_prints_the_program_name_and_the_cargo_package_version` FAILED (exit code 2 instead of 0: clap rejects the unknown flag) |
| V2 | the version comes from `Cargo.toml`, not a literal | `version = "7.7.7"` written into the attribute instead of the bare `version` (which is `crate_version!`) | `version.rs::version_prints_the_program_name_and_the_cargo_package_version` FAILED (`--version must print the package version 0.1.0: stdout "radian-tui 7.7.7\n"`) |

2/2 caught. The test also pins the exact line (`radian-tui <version>` on stdout, empty
stderr), so a version that is printed to stderr, or with a wrong name, fails too.

V2's complement (the version really tracks `Cargo.toml`) was checked by setting both
packages to `9.9.9`: the binaries then print `9.9.9` and the tests stay green — no assertion
is pinned to the literal `0.1.0`.

## 追加（smallfixes，D15 报错）

Base `f552250` · one-shot worktree `/tmp/small-mut` · `--no-fail-fast`；变异后 `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | 已存在会话的报错不含 `--resume <id>` | ✅ 抓住 (rc=101) | `pty_session_id_already_exists_points_at_resume`（还断言未进备用屏、已有文件字节不变） |

## ADR 0016：档位别名（2026-10-13）

一次性检出 `/tmp/tiers-mut`（base `9c060d2`），变异后 `git checkout --` 恢复。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | `parse_tier` 去掉 `workspace-write` / `full-access` 两个别名分支 | ✅ failed | `profile::tests::tier_names_and_aliases_parse` |
