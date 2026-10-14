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

## ADR 0018：状态栏显示 `T / B`（`pi/guard`）

一次性 worktree `/tmp/radian-guard-mut`（HEAD = `8825691`，独立 target），`--no-fail-fast`；
变异后 `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M11 | `budget_text` 的分母写成投影 `T` 而不是 `B` | ✅ failed | `the_status_bar_shows_the_projection_over_the_input_budget`、`a_request_started_event_sets_the_status_bar_report` |

「`c` 继续只放宽一次请求」的旧变异 M4 随 `BudgetRelax` 与继续提示一起删除（ADR 0018）。

## ADR 0021 M1 第四片：slash 命令框架（2026-10-13）

Base `4bf767d` · 一次性 worktree `/tmp/memtui-mut`（从 HEAD 建，做完删掉）· **独立
`CARGO_TARGET_DIR`** · 每个变异跑 `cargo test -p radian-tui --lib`、`cargo test -p radian-tui
--test command`、`cargo test -p radian-host memory`、`cargo test -p agent-cli --test cli memory_`
（都 `--no-fail-fast`），然后 `git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 去掉 `/` 拦截（`/` 开头的行发给模型） | `tests/command.rs::{help_and_unbalanced_quotes_are_notices_and_send_nothing, a_memory_read_runs_inline_and_is_never_sent, a_confirmed_forget_waits_for_y_and_n_keeps_the_record, a_pending_confirmation_keeps_keys_out_of_the_input_box}` | ✅ |
| M2 | 去掉 `//` 转义（`//hi` 当命令） | `tests/command.rs::a_double_slash_sends_the_message_with_one_slash` | ✅ |
| M3 | 确认被跳过（Confirm 直接执行） | `tests/command.rs::{a_confirmed_forget_waits_for_y_and_n_keeps_the_record, a_pending_confirmation_keeps_keys_out_of_the_input_box}` | ✅ |
| M5 | 未知命令发给模型（只拦 `/help`、`/memory`） | `tests/command.rs::help_and_unbalanced_quotes_are_notices_and_send_nothing` | ✅ |

4/4 全抓。M4（`confirmed=false` 仍执行写操作）在 `radian-host/MUTATIONS.md`。

## sbxnest：shell pty 测试不再“空过”

Base: `cd89c9d` · 一次性检出 `/tmp/sbxnest-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S1 | shell 工具不执行、直接返回空 `Outcome::default()`（`radian-coding/src/shell.rs`） | `pty::pty_yes_runs_an_ask_without_a_prompt_and_still_audits_it`、`pty::pty_a_compound_shell_command_always_asks` | ✅ failed |

2/2 抓住。改后的断言要求命令输出（`sum=42` / `ALLOWED-OUTPUT-42`，都不出现在命令文本里），
所以“命令没跑”不再能通过。

## compact-overflow：`/compact` 拦截（ADR 0018 §手动）

Base `dc307e6` · 一次性检出 `/tmp/covf-mut` · 独立 `CARGO_TARGET_DIR` · `git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M10 | `dispatch` 去掉 `/compact` 分支（当成未知命令） | `command::tests::compact_is_a_control_request_with_optional_instructions` | ✅ failed |

## Phase 4：composer（编辑、多行、历史、草稿）

Base `0994b19` · 一次性 worktree `/tmp/composer-mut`（从 HEAD 建，做完删掉）· **独立
`CARGO_TARGET_DIR`** · 每个变异跑 `cargo test -p radian-tui --lib`、
`cargo test -p radian-tui --test composer --test cursor`、
`cargo test -p radian-tui --test pty pty_a_draft_is_written`（都 `--no-fail-fast`），然后
`git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | Backspace 删末尾而不是光标前 | `composer::tests::backspace_deletes_before_the_cursor_not_at_the_end` | ✅ |
| M2 | 光标格按 UTF-8 字节而不是显示宽度 | `composer::tests::utf8_and_wide_characters_are_never_split`、`cursor::the_cursor_cell_counts_display_width_not_chars` | ✅ |
| M3 | Up 在非首行也取历史 | `composer::up_moves_inside_a_multiline_input_instead_of_the_history` | ✅ |
| M4 | Esc 非空也直接退出 | `composer::esc_clears_a_non_empty_input_and_quits_only_when_empty` | ✅ |
| M5 | `take_draft` 不删除文件 | `composer::tests::a_draft_round_trips_owner_only_and_an_empty_one_removes_it`、`pty_a_draft_is_written_on_exit_and_restored_by_resume` | ✅ |
| M6 | 草稿写到会话目录之外（`temp_dir`） | `pty_a_draft_is_written_on_exit_and_restored_by_resume` | ✅ |

6/6 全抓。

## mention：`@` 引用（Phase 4）

Base `c2a668d` · 一次性检出 `/tmp/radian-mention-mut`（detached，独立 `target`，跑完删除）·
`--no-fail-fast` · 每个变异 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M11 | 任何 `@` 都是候选（去掉“行首或空白之后”） | `mention::tests::only_an_at_at_a_line_start_or_after_whitespace_is_a_candidate` | ✅ failed |
| M12 | 不检查 canonical 路径仍在工作区内（符号链接逃逸被展开） | `mention::tests::a_symlink_that_leaves_the_workspace_is_not_attached_and_warns` | ✅ failed |
| M13 | 任何字节都当文本（二进制不再被拒） | `mention::tests::a_binary_file_makes_the_message_unsendable`、`a_utf8_file_with_a_nul_byte_is_binary` | ✅ failed ×2 |
| M14 | 关掉单文件与合计上限 | `mention::tests::a_file_over_the_per_file_limit_makes_the_message_unsendable`、`the_total_limit_makes_the_message_unsendable` | ✅ failed ×2 |
| M15 | `App::restore_input` 丢弃文本（失败时输入丢失） | `pty::pty_a_binary_attachment_is_not_sent_and_the_input_is_kept` | ✅ failed |
| M16 | `display_text` 永不折叠 | `render::a_user_file_attachment_is_folded_to_one_line` | ✅ failed |
| M17 | 去掉“同一文件只附一次”的去重 | `mention::tests::a_reference_to_an_existing_file_is_appended_once` | ✅ failed |
| M18 | 不再总是跳过 `.git` / `.radian-sandbox` | `mention::tests::hidden_entries_need_a_leading_dot_and_git_is_always_skipped` | ✅ failed |

8/8 全抓，无等价变异。M15 走 pty（发送路径），其余走纯函数单测。
## sessions：picker 与 /sessions //export（Phase 4）

Base: `9daed70` · 一次性检出 `/tmp/sessions-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | picker 忽略 workspace 过滤（`index.list(None)`） | `pty::pty_pick_lists_filters_and_resumes_the_chosen_session`（断言 `other` 不出现） | ✅ failed |
| M2 | `Esc` 返回 `New`（建会话文件） | `pty::pty_pick_starts_new_and_escape_leaves_no_session`（断言没有新 `.jsonl`） | ✅ failed |
| M3 | `/export` 跳过已存在检查 | `pty::pty_sessions_and_export_commands`（断言 `ready exists`） | ✅ failed |
| M5 | `/sessions` 落到 `Message`（发给模型） | `pty::pty_sessions_and_export_commands`（断言 `--resume cmd`） | ✅ failed |

## tab：`@` 补全接到 Tab（Phase 4）

Base `81ea117` · 一次性检出 `/tmp/radian-tab-mut`（detached，独立 target，跑完删除）·
`--no-fail-fast` · 每个变异 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M19 | 唯一候选的替换区间错位（`range.start + 1`） | `mention_tab::a_unique_file_candidate_completes_and_adds_a_space` | ✅ failed |
| M20 | 目录也加空格（不再保留尾 `/`） | `mention_tab::a_unique_directory_candidate_keeps_the_trailing_slash` | ✅ failed |
| M21 | 第二次 Tab 不显示候选列表 | `mention_tab::several_candidates_complete_to_the_common_prefix_then_list` | ✅ failed |
| M22 | 非 `@` 词也补全（`mention::complete` 不要求 `@`） | `mention_tab::tab_on_a_word_that_is_not_a_reference_does_nothing` | ✅ failed |
| M23 | `Esc` 不关闭列表（落到清空输入的分支） | `mention_tab::esc_closes_the_list_without_clearing_the_input` | ✅ failed |
| M24 | `truncate_columns` 不截断 | `render::completion_rows_truncates_to_the_width` | ✅ failed |

6/6 全抓。M22、M24 起初有漏：`plain`/`mail a@b` 本来就不匹配文件，而 `Paragraph` 自己会裁剪，
所以屏幕测试抓不住——补了“无 `@` 但匹配文件”的用例与 `completion_rows` 的直接单测（`81ea117`）
后才抓住，记录在此。

## Phase 4：主题与按键（ADR 0008 `tui.*`）

Base `8028d9e` · 一次性 worktree `/tmp/config-mut` · 独立 `CARGO_TARGET_DIR` · `git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M6 | 冲突的按键绑定被接受（不做 conflict 检查） | `keys::tests::a_second_action_on_the_same_key_is_a_startup_error`、`config::a_keymap_conflict_is_a_startup_error` | ✅ |
| M7 | `NO_COLOR` 被忽略 | `theme::tests::no_color_forces_mono`、`config::a_configured_theme_is_applied_and_no_color_forces_mono` | ✅ |

## config 第二轮：Tab 补全改为 `complete` 动作

Base `79f60cb` · 一次性 worktree `/tmp/config2-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M25 | `Action::Complete` 变空操作 | `mention_tab`（3 passed / 7 failed） | ✅ |
| M26 | `complete` 失去默认键（`DEFAULT_KEYS` 去掉 `("complete", &["tab"])`） | `mention_tab`（3 passed / 7 failed） | ✅ |

## fork：TUI `/fork` 的运行中拒绝

Base: `fcd0c26` · 一次性检出 `/tmp/fork-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| F5 | 去掉 `/fork` 的 `TurnState::Running` 检查（运行中也允许） | `pty::pty_fork_refuses_while_a_turn_runs` | ✅ failed |

1/1 抓住。

## config 第三轮：`[config]` 必须在进备用屏之前打印

Base `bb214e4` · 一次性 worktree `/tmp/config3-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M27 | 把 `[config]` 行/警告的 `eprintln!` 移回 `TtyGuard::enter()` 之后（在备用屏里打印） | `pty_the_config_line_is_not_written_inside_the_alternate_screen`、`pty_fork_creates_a_resumable_child_when_idle` | ✅ failed（两屏都渲染成 `> hello]parent…`） |

这个 bug 是 main 的 fork 测试抓出来的：mutation 证明**我自己的第一条回归测试漏了**它——
没有 resumed history 时用户行不是第一行，陈旧的 `[config]` 单元落在启动 notice 上。改成
`--resume` 一条历史（第一行就是用户行，和 fork 的情形一样）并同时断言字节顺序后，两个测试
都能抓住。

## find：Phase 5 transcript 内搜索

Base: `45947ad` · 一次性检出 `/tmp/find-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| N1 | 大小写敏感匹配（去掉 `to_lowercase`） | `find::matching_is_case_insensitive_and_per_line`（lib）、`find::ctrl_f_opens_and_typing_jumps_to_the_nearest_match_above`（app） | ✅ failed |
| N2 | 匹配折叠前的文件内容（`display_lines` 用 `entry.text`） | `find::a_folded_attachment_is_matched_by_its_display_text`（lib） | ✅ failed |
| N3 | 到头循环（`prev` 从第一个绕到最后一个） | `find::navigation_does_not_wrap_and_anchors_above`（lib）、`find::prev_walks_earlier_and_next_walks_later_without_wrapping`（app） | ✅ failed |
| N4 | `Esc` 关闭时跳回底部 | `find::escape_closes_and_leaves_the_viewport_where_it_is`（app） | ✅ failed |
| N5 | 跳转后仍 `follow` | `find::ctrl_f_opens_and_typing_jumps_to_the_nearest_match_above`（app） | ✅ failed |

5/5 抓住。N1/N2/N3 各自有 lib 单测与 app 断言两层（我第一轮误用 `--test find` 跑 lib 单测名，过滤器匹配 0 个测试而假通过；改到 `--lib`/app 后抓住）。

## find 纳入 Keymap（两个上下文）

Base `f2270e0` · 一次性 worktree `/tmp/config4-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout -- .` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M28 | find 层永不被查询（`if false && self.find.is_some()`） | `tests/find.rs`（6/8 failed：`ctrl_f_opens_…`、`prev_walks_…`、`escape_closes_…`、`backspace_…`、`a_query_with_no_match_…`、`a_search_over_fifty_thousand_…`） | ✅ |
| M29 | 搜索框里 `enter` 不再是 `find_prev`（去掉 `find_prev` 默认里的 `enter`） | `find::prev_walks_earlier_and_next_walks_later_without_wrapping`、`find::escape_closes_and_leaves_the_viewport_where_it_is` | ✅ |
| M30 | `find` 失去默认键 `ctrl-f` | `tests/find.rs`（6/8 failed） | ✅ |
| M31 | 冲突检查跨两个上下文（把两层并成一层查） | `keys::tests::a_binding_only_conflicts_with_its_own_layer`、`a_configured_action_replaces_its_default_keys`、`unknown_actions_and_keys_are_warnings_not_errors` | ✅ |

4/4 全抓。M31 是「不该报的冲突」这一类：分层之后 `enter` 同时是 `send` 与 `find_prev`，跨层检查会把
默认配置都判成冲突。

## orphan-spin：终端消失与 pty harness 的孤儿

Base `84ca71d` · 一次性检出 `/tmp/orphan-mut`（detached，独立 target，跑完删除）· `--no-fail-fast` ·
每个变异 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| O1 | `Pty::Drop` 只杀外层 shell（去掉 process-group kill） | `pty::a_panicking_pty_test_leaves_no_app_process` | ✅ failed（该变异运行本身也如预期留下孤儿，已清理） |
| O2 | 屏蔽两条退出路径（`Input::Gone` 什么都不做，且 draw 失败不再按终端消失处理） | `pty::pty_a_closed_master_ends_the_app_cleanly` | ✅ failed（5s watchdog 内不退出） |
| O3 | `tty_readiness` 把 `POLLHUP` 当成 `Readable`（进入 crossterm 的 EOF 死循环） | 同上 | ✅ failed（5s watchdog 内不退出） |

3/3 全抓。O1 走"panicking 测试不留进程"这条。**O2/O3 的记录在 orphan-spin-3 被更正**：上面的
`pty_a_closed_master_ends_the_app_cleanly` 当时其实是被**绘制失败**那条路救走的（app 每帧都会写
~30 字节的游标定位，与画面是否变化无关），所以 O3 时灵时不灵；`--freeze-draw` 之后绘制被冻结，
O2/O3 才确定性地由输入侧判据抓住。见下一节。

## orphan-spin-2：信号任务的兜底（同一分支第二个提交）

Base `f3f38d5` · 一次性检出 `/tmp/op2-mut`（detached，独立 target，跑完删除）· `--no-fail-fast` ·
变异后 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| O4 | 去掉信号任务的 3 秒兜底（`EXIT_WATCHDOG` 超时不再 `exit(128+signal)`） | `pty::pty_a_hung_main_loop_still_exits_on_a_signal` | ✅ failed（10s watchdog 内不退出） |

1/1 抓住。该变异运行本身靠新增的 `KillOnDrop` 守护清理，未留下孤儿。

## orphan-spin-3：让"关闭 master"的测试只走输入侧（同一分支第三个提交）

评审发现：`pty_a_closed_master_ends_the_app_cleanly` 并没有在测"读到 EOF 的路径"。评审的变异
（`tty_readiness` 里 `... != 0 && false`，屏蔽 `POLLHUP`）当时仍能在 0.82 s 内通过——进程是从
**绘制失败**那条路退出的（`draw_frame` → `terminal_gone(EIO)`），而且时灵时不灵。

**核实（评审的推断部分不成立）**：app 每帧都会写 ~30 字节（`\x1b[39m\x1b[49m\x1b[59m\x1b[0m
\x1b[?25h\x1b[28;2H`，即样式复位 + 显示光标 + 定位），**与画面是否变化无关**，所以"画面不变
就不写字节"不是原因；真正原因是**绘制路径一直活着**，关掉 master 之后它总是会撞上 EIO，和输入
侧的 `poll(2)` 抢跑。哪个先到取决于关闭时刻主循环在 `read` 还是 `draw`，所以旧测试 flaky。
（另有一个独立的坑：首帧比 pty 输出缓冲大，父进程不读时 app 会阻塞在 `draw` 里。）

**修法**：新增测试接缝 `--freeze-draw <FILE>`：第一帧画完后写 marker，之后 `draw_frame` 直接
返回、不再画。测试等 marker 出现（同时 drain master）、关掉 master，于是**只有**输入侧能退出。

Base `b60099b`（加 `--freeze-draw` 后未提交的工作树，独立构建）· `--no-fail-fast` · 每个变异
`git checkout --`/副本还原、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 | 备注 |
|---|---|---|---|---|
| O5 | `tty_readiness` 里 `fd.revents & (POLLHUP\|POLLERR\|POLLNVAL) != 0 && false`（评审那一行，屏蔽 HUP 检测） | ✅ failed ×3（5s watchdog 内不退出） | `pty::pty_a_closed_master_ends_the_app_cleanly` | 加 `--freeze-draw` 前：flaky（一次通过、一次超时），因为绘制路径可能救场；加之后 3/3 确定性失败 |
| O2′ | 上面 O2（屏蔽两条退出路径）在 `--freeze-draw` 下重跑 | ✅ failed（watchdog 内不退出） | 同上 | 更正 O2 的旧记录（旧记录写的是"退出码 1"，那是绘制路径的结果） |

变异 O5 用了两次结果：**未加接缝**时该测试通过（bug 没被抓住），**加了接缝**后 3/3 失败
（被抓住）。两次都记在这里。

## ADR 0023 Step 1 ①：Action/Effect 与纯 `App::update`

Base `e1d664b` · 一次性 worktree `/tmp/s1a-mut` · 独立 `CARGO_TARGET_DIR` ·
每个变异 `git checkout -- .` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M22 | `@` 展开失败时丢掉输入（去掉 `restore_input`） | `input::a_failed_expansion_puts_the_input_back_and_sends_nothing` | ✅ |
| M23 | `AnswerApproval` 被吞掉（清掉 prompt 但不产生 effect） | `approval::keys_answer_the_prompt` | ✅ |
| M24 | `/memory` 写命令跳过确认（`confirmed: false` → `true`） | `command::a_memory_read_runs_and_is_never_sent`、`a_confirmed_forget_waits_for_y_and_n_keeps_the_record`、`a_pending_confirmation_keeps_keys_out_of_the_input_box` | ✅ |
| M25 | `Steer`/`SendFollowup` 在运行/空闲时选反 | `input::enter_expands_then_sends_a_followup_when_idle_and_a_steer_while_running`、`ctrl_j_inserts_a_newline_and_enter_sends_the_whole_text` | ✅ |
| M26 | `app.rs` 重新直接调 `mention::complete`（读目录） | `purity::app_rs_is_pure` | ✅ |
| M27 | `app.rs` 重新 `use std::fs` | `purity::app_rs_is_pure` | ✅ |
| M28 | turn 结束时清 prompt 但不发 `AbandonApproval`（回复通道留在循环里） | `approval::a_finished_turn_abandons_a_prompt_that_was_never_answered` | ✅ |
| M29 | `/fork` 在 turn 运行时也被允许（去掉空闲检查） | `update::fork_is_refused_while_a_turn_runs` | ✅ |
| M30 | 输入字符绕过 find 框（直接进消息框） | `find`（6/8 failed） | ✅ |
| M31 | 候选列表只被 `escape` 关闭，带 action 的键不再关闭它 | `mention_tab::a_bound_key_dismisses_the_list_too` | ✅ |

10/10 抓住。**诚实补充（M31）**：第一轮它**没被抓住**——`typing_dismisses_the_list` 只覆盖了
「输入字符关闭列表」，而字符那条清理在 `Input::Char` 分支里，M31 只删掉带 action 的那条。
补了 `a_bound_key_dismisses_the_list_too`（列表显示时按 `Up`）之后抓住；这条测试是变异暴露出的
覆盖缺口，不是事后凑数。

## ADR 0023 Step 1 ②：事件驱动循环与脏通知

Base `9a62193` · 一次性 worktree `/tmp/s1b-mut` · 独立 `CARGO_TARGET_DIR` ·
每个变异 `git checkout -- .` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M32 | 生产者不检查原值，每个 delta 都发 `Dirty` | `events::tests::the_sink_sends_one_dirty_until_the_ui_looks`、`deltas_are_not_facts_and_a_commit_clears_the_stream` | ✅ |
| M33 | UI 先读投影再清标志（顺序反了） | `projection::tests::an_update_between_the_clear_and_the_read_is_not_lost` | ✅ |
| M34 | 脏标志用 `Relaxed` | `projection::tests::the_dirty_flag_is_release_and_acquire_never_relaxed` | ✅（见下） |
| M35 | 信号任务不发事件（只设标志，靠 3 秒兜底） | `pty_a_signal_while_idle_leaves_immediately`（3.95 s 才退出 = 兜底） | ✅ |
| M36 | 每轮不设事实上限（一次排空） | `pty_a_flooding_producer_still_draws_and_answers_keys`（2 秒窗口只剩 1 帧） | ✅ |
| M37 | 空闲时按超时重绘（回到轮询行为） | `pty_an_idle_screen_is_not_redrawn` | ✅ |
| M38 | 输入线程吞掉挂断（不发 `TerminalGone`） | `pty_a_closed_master_ends_the_app_cleanly` | ✅ |
| M39 | driver watcher 不发 `DriverStopped` | `pty_a_driver_panic_exits_orderly_and_restores_the_terminal`（15 s 超时） | ✅ |
| M40 | 提交不清空流式缓冲 | `events::tests::deltas_are_not_facts_and_a_commit_clears_the_stream` | ✅ |
| M41 | 效果结果不回送（`ExpandMentions` 丢结果） | `pty_end_to_end_conversation_and_clean_exit` | ✅ |

10/10 抓住。两点诚实说明：

- **M34**：本仓库没有 loom（不加依赖），单线程测试观察不到内存序。所以补了一个**结构测试**
  `the_dirty_flag_is_release_and_acquire_never_relaxed`（只扫非测试部分，避免自己引用到被禁的字符串），
  它把标志的两次 `swap` 钉成 `Release`/`Acquire`；**程序顺序**由 M33 的交替测试覆盖，**内存序**由这条
  结构测试覆盖。这是"loom 不可用"下能做到的最好，不是等价替代。
- **M40**：第一次只跑了 pty 套件，**没被抓住**（单轮会话里陈旧的缓冲不会被看见）；跑
  `--lib events` 后被单测抓住。记录在此，避免下次再误判。

## ADR 0023 Step 1 · PR ④：投影读取与通知（2026-10-13）

Base `7b811b7`（合并 PR ③ 后的 `5dcac58` 上原样重跑，同样 4/4 抓住）· 一次性检出（detached，跑完删除）· `--no-fail-fast`；
变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| S5 | `ProjectionSink` 不再写 usage 事实（只转发事件） | ✅ 抓住 | `events::tests::a_fact_event_writes_the_projection_and_wakes_the_ui` |
| S6 | `ProjectionSink` 写事实但不发 `Dirty` | ✅ 抓住 | 同上（少了唤醒） |
| S7 | `apply_dirty` 改成 `app.update(..).dirty`：空流的事实更新不再画帧 | ✅ 抓住 | `tests::a_dirty_notification_with_an_empty_stream_still_draws` |
| S8 | `App::update(Input::CommandRefused)` 不显示 Notice | ✅ 抓住 | `update::a_refused_command_is_shown_as_a_notice` |

4/4 抓住。S5/S6 是"投影写了"与"UI 被唤醒"两件事各自被破坏；S7 对应评审要求的"投影更新但 UI 不重画"
变异（`App` 旧字段已删，编译即可证明状态栏不再读它们，所以改用这条）；S8 钉住能力拒绝是**一条
Notice**而不是让循环失败。

## ADR 0023 Step 1 ③：层栈（U4）

Base `677ca35` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）·
`git status --short` 结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| L1 | `cancel` 放到栈之后处理（先走层） | `layers::ctrl_c_cancels_while_an_approval_modal_is_open`、`approval::ctrl_c_while_a_prompt_is_up_cancels_instead_of_answering` | ✅ failed |
| L2 | `push_overlay` 无视层级，直接压到栈顶 | `layers::an_overlay_pushed_while_a_modal_is_up_stays_below_it` | ✅ failed |
| L3 | 审批/确认键改走 `Keymap`（不再硬编码） | `approval::keys_answer_the_prompt`、`config::the_approval_keys_cannot_be_taken_over_by_the_keymap`、`layers::the_approval_keys_survive_a_keymap_that_rebinds_y`、`layers::an_overlay_pushed_while_a_modal_is_up_stays_below_it` | ✅ failed |
| L4 | 未消费的输入不往下传（`Pass` 变 `Swallow`） | `layers::the_find_overlay_passes_navigation_to_the_transcript`、`config::the_find_layer_is_rebindable` | ✅ failed |

4/4 抓住。L1 的“放到栈之后”是：去掉栈前的 `cancel` 检查、放到 while 循环之后；审批 Modal 会先吞掉
Ctrl-C，测试随即失败。L3 把模态的硬编码分派整段删掉，`y` 于是落到 `Keymap`/被吞，两条重绑测试失败。

## mouse（提交 1）：滚轮滚 transcript（ADR 0023 U2 方案 B）

Base `0063dae` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 开成 crossterm 的 `EnableMouseCapture` 风格（多开 `?1002h`/`?1003h` any-motion） | `pty_a_mouse_move_is_dropped`、`pty_every_exit_path_releases_the_mouse`、`pty_wheel_scrolls_the_transcript_not_the_input_history`（都断言无 `?1002h`/`?1003h`） | ✅ failed |
| M2 | panic 路径不关上报（`MOUSE_REPORTING` 在 hook 里先置 false） | `pty_every_exit_path_releases_the_mouse`（panic 分支） | ✅ failed |
| M3 | 滚轮映射成 `Up`（`Action::HistoryPrev`） | `mouse::a_wheel_up_scrolls_and_keeps_the_input_box`、`mouse::a_wheel_down_scrolls_back`、`pty_wheel_scrolls_the_transcript_not_the_input_history` | ✅ failed |
| M4 | `tui.mouse = false` 仍开上报（`enter` 忽略参数） | `pty_mouse_off_sends_no_reporting_sequence` | ✅ failed |

4/4 抓住。

## mouse（提交 2）：`--pick` 并入 `Overlay::Picker`

Base `4cb83ee` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| P1 | `--pick` 选中后用生成的 id（忽略选中的 id），header 不再按选择展开 | `pty_pick_resumes_the_selected_id_and_enters_once`（header `x-s` 不是 `beta`）、`pty_pick_lists_filters_and_resumes_the_chosen_session`（没有 BETA-HISTORY） | ✅ failed |
| P2 | `--pick` 选 Quit 后不早退，继续创建会话 | `pty_pick_starts_new_and_escape_leaves_no_session`（Esc 后会话目录多出文件） | ✅ failed |

2/2 抓住。“整个过程只进一次 alternate screen”由 `pty_pick_resumes_the_selected_id_and_enters_once`
统计 `?1049h == 1` 直接钉住。

## mouse（提交 3）：信号兜底强退也恢复终端

Base `1e3fec4` · 在本任务工作树内施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| W5 | 兜底分支 `exit` 前不做恢复（去掉 `restore_terminal_nonblocking()`） | `pty_the_signal_watchdog_releases_the_mouse`（输出里没有 `?1000l`/`?1006l`） | ✅ failed |

1/1 抓住。该测试用新的 `--slow-draw` 接缝让主循环真的卡在 `draw_frame`（ADR 0023 U2 之后读取在输入
线程，`--block-input` 不再卡住主循环），再发 SIGTERM，等 3 秒兜底退出。

| W6 | 选择器阶段丢掉 `ui_loop` 返回的信号（`?;`），改走 Quit 分支 | `pty_a_signal_during_pick_exits_128_plus_signal`（退出码来自 `received()` 仍是 143，但**没有原因行**） | ✅ failed |

2/2 抓住。W6 的测试同时断言退出码 143、原因行 `received SIGTERM`、`?1000l`/`?1006l` 与会话目录里没有新
`.jsonl`。

| W7 | 兜底写完不还原 fd 1 的 `O_NONBLOCK` | `pty_the_watchdog_restores_the_stdout_flags`（在子进程 stdout 的同一个打开文件描述上 `F_GETFL` 仍含 `O_NONBLOCK`） | ✅ failed |

3/3 抓住。W7 的测试用 `spawn_direct_keeping_slave` 保留 pty slave 的一个 `dup`——`dup` 共享同一个打开
文件描述，所以子进程在 fd 1 上设/还原的 `O_NONBLOCK` 标志在子进程退出后仍能在该描述上读到（新 `open`
得到的是新描述，证明不了）。变异去掉还原那步后该断言失败，说明测试确实观测到标志。

## ADR 0023 Step 1 · PR ⑤（TUI 侧）与 ADR 0024 的 TUI 部分（2026-10-13）

Base `6d22bf0` · 一次性检出 `/tmp/s1e-mut`（detached，跑完删除）· 独立 target ·
`--no-fail-fast`；每个变异后从副本还原，结束时 `git status --short` 只剩变异脚本。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| S1 | 恢复 supersede：新的请求到达时把旧的审批**拒绝**掉 | ✅ 抓住 | `approval::a_second_approval_does_not_supersede_the_first` |
| S2 | `LocalSession::answer` 去掉 `AnswerRequest` 校验 | ✅ 抓住 | `radian-host` 的 `session::a_local_session_without_the_capability_refuses_an_answer` |
| S3 | 队首消失时仍保留 Modal（通知/清理没接上） | ✅ 抓住 | `approval::the_queue_head_drives_the_prompt_not_the_turn`、`keys_answer_the_prompt_with_the_queue_id` |
| S4 | `Esc` 不确认、直接拒答 | ✅ 抓住 | `question::esc_asks_once_before_declining` |
| S5 | 必答未答也能提交 | ✅ 抓住 | `question::a_required_question_blocks_the_submit` |
| S6 | `--approval never` 时不注册 `ask_user` | ✅ 抓住 | `pty::pty_approval_never_still_registers_ask_user` |
| S7 | session 关闭时审批以 `AllowOnce` 完成（不是 fail-closed 的 `Deny`） | ✅ 抓住 | `radian-host` 的 `pending_questions::a_shared_queue_session_close_aborts_both_kinds` |

7/7 抓住。S1 是评审点名要恢复的旧行为：变异后 UI 会为旧请求发一条 Deny 效果，测试断言
"移动队首不产生效果"随即失败。S3 是"等待方放弃后 Modal 仍显示"：变异让 `head == None` 时不清
Modal，三条队列驱动测试失败。S7 说明 session 关闭的 fail-closed 语义没有被队列改造稀释。

## owner UI 验收第一轮（ui-r1，六条）

Base `613312c` · 一次性 worktree `/tmp/ui-r1-mut` · 独立 `CARGO_TARGET_DIR` ·
每个变异 `git checkout -- .` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| U1 | dark 的 `notice`/`hint` 仍是 `DarkGray` | `theme::tests::dark_notices_and_hints_are_not_dark_gray` | ✅ |
| U2 | `input_style = "box"` 被当成 `"lines"` | `config::the_input_style_is_applied_and_an_unknown_value_falls_back_to_lines` | ✅ |
| U3 | 模糊匹配退化成前缀匹配（`/cmp` 不出 `/compact`） | `command::tests::fuzzy_matching_is_a_subsequence_with_prefixes_first` | ✅ |
| U4 | 第二层不补全（`/memory` 的子命令层返回 Free） | `command::tests::the_tree_completes_every_level`、`update::the_slash_completion_walks_the_command_tree` | ✅ |
| U5 | cwd == `$HOME` 时不做替换（仍用 `$HOME`） | `workspace::tests::home_becomes_home_tmp_and_is_created_owner_only`、`a_symlinked_home_still_matches` | ✅ |
| U6 | `--resume` 不带值时直接新建会话 | `pty_resume_without_a_value_picks_a_session` | ✅ |
| U7 | 退出行漏掉 `--session-dir` | `pty_the_exit_line_is_a_copyable_resume_command` | ✅ |
| U8 | 去掉 `warn_if_wide` 的每路径一次去重（`radian-session`） | `pty_sessions_opens_and_closes_during_a_running_turn`（全量套件里） | ✅ |
| U9 | Enter 不选中候选（走发送路径） | `update::the_slash_completion_walks_the_command_tree` | ✅ |

9/9 抓住。**诚实补充（U3/U4 的第一轮）**：第一次的两个 patch 没有断言替换成功，实际是 no-op
（U3 的字符串在 `cargo fmt` 后换行了；U4 的 patch 让 match 不再穷尽、编译失败）——两次都"通过"了，
是假通过。加上 `assert old in s` 并按实际文本重做后，两个都抓住。**U8** 是本次修的 bug 的变异
（`/sessions` 在备用屏里打印 `[privacy]` 警告 → 渲染 diff 跳过那些 cell → picker 的标题/filter 行被糊掉）；
它只在**全量套件**里稳定复现（单跑 10 次全过），所以那次变异跑的是整个 pty 套件。

## Slice: theme files and per-role overrides (ADR 0029)

Base `9ee35b4` · 一次性 worktree `/tmp/tui-mut` · 每个变异 `git checkout -- .` 还原后跑对应测试,
`git status --short` 为空。变异用 `python3` 补丁 + `assert old in s`(吸取 U3/U4 的教训)。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| T1 | 变量不解析(所有 `vars` 引用一律报 missing) | 主题文件的变量链必须真的解析 | `theme::file::tests::chained_variables_resolve_and_a_loop_only_breaks_its_own_roles`、`the_kanagawa_wave_fixture_loads_with_all_roles_resolved` FAILED |
| T2 | oklch→OKLab 换算 a/b 通道互换 | 换算与 pi 逐字节一致 | `theme::color::tests::oklch_matches_pis_conversion`、`oklch_out_of_gamut_is_clipped_not_rejected`、`functional_notation_follows_pis_grammar` FAILED |
| T3 | 缺失的角色变无色(`Style::default()`),不回退内建 | 缺角色必须回退内建调色板 | `theme::file::tests::missing_roles_fall_back_by_appearance` 等 6 个 FAILED |
| T4 | `[tui.colors]` 覆盖整体丢弃 | 覆盖必须生效、格式错误必须警告 | `theme::file::tests::overrides_accept_radian_and_pi_names_and_tables`、`malformed_overrides_warn_and_are_ignored` FAILED |
| T5 | `NO_COLOR` 只在无主题名时强制 mono(配置了主题名就放行) | `NO_COLOR` 优先于主题文件与覆盖 | `theme::file::tests::no_color_beats_theme_files_and_overrides`、`config.rs::a_configured_theme_is_applied_and_no_color_forces_mono` FAILED |
| T6 | 内建 dark 的 `user` 从 Cyan 改成 Green | `dark` 截图/调色板逐字节钉住 | `theme::tests::dark_keeps_the_original_palette`、`config.rs::the_dark_theme_keeps_the_original_colors` FAILED |
| T7 | 256 色近似的灰阶分支被禁用(cube 永远赢) | 近似算法与 pi 一致(两个分支都有钉住值) | `theme::color::tests::ansi256_approximation_matches_pis_algorithm` FAILED |

7/7 抓住,无等价变异。

## approval-fit：审批框在内容太长时看不到按键提示＼�2026-10-13）

Base `d76df67` · 就地变异，每个变异 `git checkout -- crates/radian-tui/src/render.rs` 还原、
`git status --short` 只留本片的新测试。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| F1 | 按键提示拼回正文末尾（`approval_body` 末尾追加 `APPROVAL_KEYS`） | `approval::a_shell_approval_shows_the_command_once`（断言正文不含键行） | ✅ |
| F2 | 截断命令（`resolved_resource` 也过 400 字节截断） | `approval::the_args_cap_applies_after_the_dedup` | ✅ |
| F3 | 恢复 `clamp(5, 14)`（`approval_cap` 直接返回 14） | `approval::the_box_height_is_capped_at_sixty_percent_of_the_terminal` | ✅ |
| F4 | 不去重（`args` 的过滤条件改成恒真） | `approval::a_shell_approval_shows_the_command_once`、`args_are_omitted_when_the_resource_is_the_only_field` | ✅ |

诚实补充（F3 的第一轮）：第一次的 patch 写的是 `14;`（一条丢弃值的语句），函数照常返回，测试
"通过"是**假通过**；改成 `return 14` 后才真正抓住。这与上一节 U3/U4 的教训相同：patch 必须
`assert old in s`，并且要确认它真的改变了行为。

## Slice: `/login` / `/logout`(ADR 0026 D3/D10)

Base `d3b5163` · 一次性 worktree `/tmp/login-mut` · 每个变异 `git checkout -- .` 还原后跑对应
测试,`git status --short` 为空。编号 L1–L7(与 agent-cli 的 MUTATIONS.md 同一张表)。

| # | 变异 | 抓住它的测试 |
|---|---|---|
| L1 | 向导把 key 明文渲染(`wizard.key.clone()` 代替掩码) | `pty.rs::pty_login_custom_walk_saves_without_showing_the_key`(等 15 个掩码列出现的状态门 + 屏幕无明文;**诚实补充**:第一版测试只用 `pump()` 后的单次断言,逐字符 diff 渲染让明文连不成子串,变异"通过"是假通过;加状态门后才真正抓住) |
/`logout` 的确认与删除由 L6 覆盖(见 agent-cli 表)。
## ADR 0026 D9：`!` 自动批准键与 `/auto`（2026-10-13，owner 更正：键是 `!`，`Y` 保持 `y`）

Base `43ddbff` · 一次性检出 `/tmp/auto-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --test auto <test>`，变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| TM-A1 | `Y` 映射到 `Input::ApprovalAuto`（代替保持 `y`） | `Y` 永远只是 allow once，不开 auto | `auto.rs::y_stays_allow_once_and_never_enables_auto` FAILED |

1/1 抓住。`!` 本身的流程由 `auto.rs::the_auto_key_approves_this_request_and_turns_auto_on`
与 pty 层 `pty_auto_key_approves_once_and_enables_auto` 钉住；`/auto on|off` 与别名、用法
报错各有纯测试，未再单独变异（同一条代码路径 `Outcome::AutoToggle` 被 4 个用例共享）。
## Slice: transcript selection and copy (mouse drag, OSC 52)

Base `c8e1a35→`（本片 HEAD）· 一次性 worktree `/tmp/tui-mut-select`（`git worktree add` 自本片
HEAD，结束后删除）· 每个变异改完跑对应测试、`git checkout -- .` 还原、`git status --short`
为空。变异脚本带 `assert old in s`（吸取 U3/U4 的教训：no-op patch 是假通过）。

| # | 需求 | 变异 | 抓住它的测试 |
|---|---|---|---|
| SC1 | 多开 `?1002h`，`?1003h` 永不开 | 使能序列改成 `?1000h ?1003h ?1006h` | `pty_a_mouse_drag_copies_the_selection_via_osc52`（断言精确三元组 + 不含 `?1003`） |
| SC2 | 折行处不插入换行 | 拷贝改成 `display_lines(..).join("\n")` | `a_copy_across_a_soft_wrap_inserts_no_newline`、`press_drag_release_produces_the_copy_effect` |
| SC3 | 所有退出路径关闭 `?1002` | 关闭序列删掉 `?1002l` | `pty_quit_closes_button_motion_reporting`（另有 `assert_mouse_released` 双保险） |
| SC4 | Modal 上按下不开始选择 | 去掉 `mouse_down` 的 `has_modal` 门 | `a_press_on_a_modal_never_starts_one` |
| SC5 | `update` 不直接写 stdout | 在 `mouse_up` 里直接 `io::stdout().write_all(osc52(..))` | `purity::app_rs_is_pure`（本片把 `io::stdout`/`io::stderr` 加进禁止表） |

5/5 抓住。**审查补漏（SC6，merge main 之后）**：owner 审查发现 base64 表尾两个字符（索引
62/63 的 `+`/`/`）没有任何向量覆盖——把表换成 URL-safe 的 `-_` 全部测试仍绿。补 `osc52("~~~")`
→ `fn5+`（`~` = 六个 1，索引 62）与 `osc52("???")` → `Pz8/`（`?` = 六个 1，索引 63）后，同一
变异在 `clip::tests::osc52_encodes_base64_with_the_clipboard_selection` 被抓住（实测输出
`fn5-` ≠ 期望 `fn5+`）。

**过程记录（SC4 的两轮）**：第一版测试在 `set_viewport(2, 6)` 后按 (4, 5)——
视口已缩到 2 行，按下去本来就在 transcript 之外，变异等于没被实验；第二版按 (1, 5)（视口内）
仍没抓住，因为 follow 把 scroll 钉在底部，press/drag 都钳到同一条「末尾之后的缝隙」，选区为空。
最终 `scroll = 0; follow = false` 后变异才被抓住。教训：测「某个门存在」必须让门后路径真的产生
可观察状态。

**本片另加的两个行为断言**（不是变异，是回归钉）：`pty_pointer_motion_alone_draws_no_frame`
（无按键移动 40 次、帧数不涨——SC1 的行为面）和
`pty_quit_closes_button_motion_reporting`（退出序列精确为 `1006l 1002l 1000l`——SC3 的精确面）。

## ui-r2：启动画面、Tip、圆角框、Ctrl-C（2026-10-14）

Base `ba7568d` · 一次性检出 `/tmp/ui2-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
变异后 `git checkout --` 还原，提交前 `git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| U2-1 | 安全提示折进启动块（`EntryKind::Warning` → `Startup`） | 安全提示不折叠、用 warning 类 | `startup.rs::safety_lines_stay_visible_while_collapsed` FAILED |
| U2-2 | Tip 引用不存在的功能（`Ctrl-C twice quits` → `/warp jumps time`） | 每条 Tip 对应现有命令/按键 | `startup.rs::every_tip_names_a_real_feature` FAILED |
| U2-3 | 去掉 `BorderType::Rounded`（直角框） | `box` 是圆角框 | `render.rs::the_box_input_style_draws_the_rounded_box_with_the_prompt` FAILED |
| U2-4 | `cancel_or_quit_at` 的 arm 分支直接退出（一次就退） | 第一次只进入"待退出" | `quit.rs::idle_and_empty_the_first_ctrl_c_only_arms_the_window` FAILED |
| U2-5 | 不检查 2 秒时限（`Some(_) => quit`） | 窗口过期只重新提示 | `quit.rs::a_second_ctrl_c_after_the_window_re_arms` FAILED |
| U2-6 | 清空时不进历史 | 清掉的内容按 `Up` 找回 | `quit.rs::idle_with_text_the_first_ctrl_c_clears_it_into_the_history` FAILED |
| U2-7 | 运行中按 Ctrl-C 走清空输入（把 Running 判定放到清空之后） | 运行中仍是取消 | `quit.rs::ctrl_c_while_running_cancels` FAILED |
| U2-8 | `update_inner` 里直接 `std::fs::read_to_string` | `update` 保持纯（结构测试） | `purity.rs::app_rs_is_pure` FAILED |

8/8 抓住。`/theme` 的两条变异（丢掉注释、高版本照样写）改的是 `radian-host`，记录在
`crates/radian-host/MUTATIONS.md`（TW1/TW2）。
## Slice: ADR 0028 第 1 片（块模型、按块背景、thinking 折叠、代码块不折行）

Base `82b07de` · 一次性 worktree `/tmp/blocks-mut`（`git worktree add` 自本片 HEAD，结束后删除）·
每个变异 `python3` 补丁（带 `assert`，no-op 会显式报 PATCH FAILED）、跑对应测试后 `git checkout -- .`
还原、`git status --short` 为空。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| B1 | 用户块整行铺背景（含行尾与上下空行） | 去掉行尾填充 span，空行不铺 | `a_user_block_paints_its_band_across_the_whole_block` | ✅ |
| B2 | 工具卡片按状态铺背景 | `card_band` 对 ToolCall/ToolResult 一律返回 pending | `a_tool_card_paints_the_state_band` | ✅ |
| B3 | thinking 折叠成一行 | `layout`/`cheap_count` 不再处理 `Collapsed` | `thinking_folds_to_one_line_and_ctrl_t_expands_it` | ✅ |
| B4 | 代码块不折行、超宽截断带 … | `layout`/`line_count` 的 Code 分支改成按正文折行 | `a_code_block_does_not_wrap_and_truncates_with_an_ellipsis` | ✅ |
| B5 | 折叠状态进缓存键 | `layout_cache` 的键去掉 `ReasoningMode` | `folding_changes_the_transcript_line_count` | ✅ |

5/5 抓住。**过程记录（B5 的第一轮）**：第一版跑的 `thinking_folds_…` 只看屏幕文本——可见条目每帧
重新 `layout`（不走计数缓存），所以缓存键漏掉 mode 时屏幕内容仍然正确，是**假通过**。加
`folding_changes_the_transcript_line_count`（断言 `app.total_lines` 随折叠变化）后才真正抓住。
**B4 的第一轮**是 patch 文本与源码格式不符（`cargo fmt` 后换行），脚本的 `assert` 直接报
PATCH FAILED 并跳过——没有当成"抓住"。

### 审查第 1 轮的两条补充（blocks-r1，提交 `9afa1ff`）

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| C1 | 只对 notice 的第一行切标签（N1） | 去掉 `first_text` 条件，每行都切 | `a_wrapped_notice_continuation_is_not_a_label` | ✅ |
| C2 | 恰好等宽的代码行不算截断（N2） | 恢复"总按 `width-1` 判断"（`if total <= width` 恒假） | `a_code_line_that_fits_exactly_is_not_truncated` | ✅ |

2/2 抓住。**过程记录**：C2 第一轮跑的是 `cargo test --lib blocks a_code_line_that_fits` —— 两个位置参数，cargo 只把第一个当过滤器，于是"没抓住"是假象；改成单个过滤器 `--lib a_code_line_that_fits` 后立刻抓住。F0（fmt）在本轮之前的一次提交里已修好，本 HEAD 上 `cargo fmt --all -- --check` 通过。

## ADR 0026 D8 审查 M1：picker 先于模型解析（2026-10-14）

Base `0b9785b` · 一次性 worktree `/tmp/radian-pi-mut-m1`，跑完删除。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1a | 只认命令行带 id 的 `--resume`（`resumed_route` 读 `cli.resume` 而非解析后的 `resume`） | `pty_a_picked_resume_uses_the_journals_model` | ✅ |

## ADR 0028 R8：Mermaid 图（2026-10-14）

Base `a13dd6a` · 变异只在一次性 worktree 里做（`git worktree add --detach /tmp/radian-pi-mut-mmN <commit>`），
跑完 `git worktree remove --force`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 渲染失败时留空（不显示源码与原因行） | `mermaid::a_diagram_that_does_not_fit_keeps_the_source_and_names_the_reason` | ✅ |
| M2 | 图缓存键不含宽度（`diagram_outcome` 的查找忽略宽度） | `mermaid::a_width_change_recounts_and_redraws`（80 渲染后立即用不可变 `layout(16)` 断言回退） | ✅ |
| M3 | 未闭合的代码块也渲染（`is_mermaid` 忽略 `closed`） | `mermaid::an_unclosed_block_shows_the_source` | ✅ |
| M4 | `line_count` 不计图的行数（用源码行数代替） | `mermaid::a_width_change_recounts_and_redraws`（count == laid out） | ✅ |

诚实补充：M2 的第一版变异放在 `line_count_cached` 的新鲜度检查上，测试**没抓住**——那个检查与
`diagram_outcome` 的查找宽度检查冗余，只去掉它时两条路径仍会重渲；改成变异查找（并让测试在 80
渲染后立刻用不可变 `layout(16)` 断言）后抓住。
## Slice: ADR 0028 第 2 片 a（工具卡片数据、渲染器注册表、Ctrl-O）

Base `4208170` · 一次性 worktree `/tmp/tools-mut`（`git worktree add` 自本片 HEAD，结束后删除）·
每个变异 `python3` 补丁（带 `assert`）、跑对应测试后 `git checkout -- .` 还原。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| T1 | 折叠模式进布局缓存键 | 键去掉 `DisplayModes`（只留 reasoning） | `the_tool_fold_changes_the_line_count_and_ctrl_o_walks_it` | ✅ |
| T2 | 错误卡片在任何模式下都展开 | 去掉 `is_error` 的强制展开（collapsed 下也折） | `an_error_card_stays_expanded_under_collapsed` | ✅ |
| T3 | live/resume 都填工具名 | push 时工具名填空 | `a_tool_card_head_is_the_same_on_the_live_and_resume_paths` | ✅ |
| T4 | 注册表找不到时回退通用渲染器 | 未知工具 `panic!` | `tools::tests::the_registry_falls_back_to_generic_for_an_unknown_tool`、`the_fold_modes_decide_the_body` | ✅ |

4/4 抓住。**过程记录（T2 的两轮）**：第一轮"没抓住"有两个原因，都是测试太弱——(a) 错误正文只有
两行，本来就短于预览行数，折与不折看起来一样；(b) 我改测试时 `.replace` 没命中（没加 `assert`），
改动根本没落盘。修好后：正文 20 行、屏幕够高，变异立刻抓住；同时发现**实现本身也不对**——原来的
`collapsed` 会把错误正文折起来（ADR 说"错误一律展开"），已一并修正（提交 `0e5315a`）。

## Slice: ADR 0028 第 2 片 b（其余内建渲染器 + 审查两项）

Base `106a4d6` · 一次性 worktree `/tmp/tools2-mut`（`git worktree add` 自本片 HEAD，结束后删除）·
每个变异 `python3` 补丁（带 `assert`）、跑对应测试后 `git checkout -- .` 还原。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| U1 | 审查 T1：结果按 call_id 配对 | 配对条件恒真（配给最后一张卡片） | `a_batch_of_calls_pairs_its_results_by_id_in_any_order` | ✅ |
| U2 | 审查 T2：`collapsed` 与 `errors` 不同 | `collapsed` 恢复成与 `errors` 相同 | `collapsed_and_errors_differ_for_a_successful_card` | ✅ |
| U3 | diff 行按角色着色 | `diff_body` 全部用 `Normal` | `a_git_diff_body_colors_its_line_kinds` | ✅ |
| U4 | patch 的加/删方向 | `old`/`new` 的前缀对调 | `a_patch_card_shows_the_replacement_with_diff_roles` | ✅ |
| U5 | `@@` 与上下文不同 | `@@` 归为 context | `a_git_diff_body_colors_its_line_kinds` | ✅ |
| U6 | 记忆工具有自己的渲染器 | 四个 memory 渲染器不注册（回落通用） | `the_list_and_memory_cards_have_their_own_heads` | ✅ |

6/6 抓住。**过程记录（U6 的第一轮）**：`run_mut` 那一次给了两个位置参数（`--test render the_list_and_memory --lib tools`），
cargo 只认第一个，跑出来"没抓住"是假象；改成单个过滤器 `--test render the_list_and_memory` 后立刻抓住。
另：本片实现时发现 `base_name` 按单个 `_` 切分，导致 `git_status`/`ask_user`/`memory_*` 全部落到通用渲染器
（已修：只有 `__` 或 `mcp__` 前缀才算命名空间），这一条由头部的单测与渲染测试共同钉住。
## skills 片 3：`/skill:` 展开（2026-10-14）

Base `c5a84f6` · 与 `radian-host` 同一一次性 worktree `/tmp/radian-pi-skills-mut3`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SL-M1 | `/skill:<name>` 解析成 `Outcome::Message`（不展开，当普通消息发） | `pty_skill_commands_expand_list_notice_and_escape`（请求正文没正文/args，journal 断言也挂） | ✅ |
## ui2-review1 U1：Ctrl-C 待退出只由用户输入解除（2026-10-14）

Base `eeadb11` · 一次性检出 `/tmp/ui2b-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --test quit <test>`，变异后 `git checkout --` 还原，提交前
`git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| R1-1 | 恢复"任何非 Cancel/QuitDeadline 输入都解除"（去掉 `is_user_input`） | 后台事件不解除待退出 | `quit.rs::background_events_do_not_disarm_the_quit_window` FAILED |

1/1 抓住。

## ADR 0028 R2：Markdown（2026-10-14）

Base `eac7463` · 变异只在一次性 worktree 里做（`git worktree add --detach /tmp/radian-pi-mut-mdN <commit>`），
跑完 `git worktree remove --force`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MD1 | 链接只显示文字、丢掉 ` (url)` | `markdown::the_block_kinds_and_inline_roles_render` | ✅ |
| MD2 | `md_line_count` 与 `md_lines` 不一致（+1） | `markdown::line_count_matches_the_layout` | ✅ |
| MD3 | 表格不回退（两处 min_column 守卫都短路） | `markdown::tables_wrap_and_fall_back` | ✅ |
| MD4 | 行内代码不用 `mdCode` 角色（用 body 样式） | `markdown::the_block_kinds_and_inline_roles_render` | ✅ |

未做：**"已闭合的块被重新解析"**（需要一个解析计数器的测试，本片预算内没写）；
`md3` 的第一/二轮只短路了一处守卫，另一处仍会回退，测试通过——两处都短路后才抓住。
## ADR 0013 出网/脱敏复审：TUI 脱敏列表漏掉其它 auth key（2026-10-14）

Base `a9c88c8` · 一次性 worktree `/tmp/radian-pi-priv-mut`，每条跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| P1 | TUI 的 `secrets` 退回"只用启动 provider 的 `credential.key` + header 值" | `pty_the_panic_log_redacts_every_auth_key_not_only_the_startup_one`（另一个 auth.json 条目的 key 原样进 panic log） | ✅ |
| P2 | `AuthStore::resolved_keys` 只返回第一个 key（不排序去重、不覆盖全部条目） | `auth::tests::resolved_keys_carries_every_login_sorted_and_deduplicated` | ✅ |

2/2 抓住。修复见 `crates/radian-host/src/auth.rs` 的 `resolved_keys`；ADR 0013 两节的复审记录在 `a9c88c8`。
### 审查 T3 的变异（tools2-review1，提交见 tools-b2 回报）

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| V1 | MCP 别名（`server__tool`）只走通用渲染器 | `renderer_for` 恢复"按 `__` 取后缀" | `an_mcp_alias_never_reaches_a_builtin_renderer`（单元）、`an_mcp_alias_renders_generic_and_the_builtin_keeps_its_card`（TestBackend） | ✅ |

**说明**：T3 的代码与两条测试落在了 `Merge main into pi/tools2` 那个提交的树里（我先解冲突、后写 T3，提交时一起带上了）。为便于检索，T3 的变异记录与这两条测试的关键词都写在以 T3 命名的测试里；流程上这是本片的一个失误：合并提交应当先落盘，再做后续改动。
## pty 偶发测试：pointer-motion 的帧数基线（2026-10-14）

Base `a1d1343` · 复现 harness：20 个 `yes > /dev/null`（记 pid，末尾只 kill 这些 pid）+ 4 个并发循环各
跑 `pty_pointer_motion_alone_draws_no_frame` 12 次（每次一个全新 pty/进程）。

| # | 变异 | 观察 | 结果 |
|---|---|---|---|
| HT1 | 去掉 `settled_frame_count`，退回 `let before = frame_count(&pty)`（基线不等输出安静） | 同一 harness 下 `assert_eq!(after, before)` 报 `left: 2, right: 0`：启动帧的 `[?25h` 在基线之后才被测试的 reader 线程读到，被误算成 pointer motion 画的帧 | ✅ 复现 |

**修前 1/48，修后 0/48**（同一 harness）。另外 plain 顺序单跑 20 次（同样 20 个 `yes`）：修前 0/20、
修后 0/20——偶发需要并发 TUI 争用才明显。结论：**测试的时序假设**，不是产品多画帧（产品
`input_event` 把 `MouseEventKind::Moved` 映射为 `None`，见 `events.rs:83-118`）。
## flaky-master-fix：closed-master 卡死的两层防护（2026-10-14）

Base `fdfc2cb` · 一次性检出 `/tmp/flaky-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
负载：40×`yes > /dev/null`（记下 pid，只杀这些），load average 262–457 ·
`cargo test -p radian-tui --test pty pty_a_closed_master_ends_the_app_cleanly --exact`。

| # | 变异 | 应体现 | 结果 |
|---|---|---|---|
| F1 | 去掉主循环兜底（两处 `if terminal_is_gone()` → `if false && …`），保留收窄 | 卡死重新偶发 | 负载下 **0/100** 失败：收窄本身已把窗口缩到 100 次内不复现；**兜底的必要性此变异无法演示**（如实记录）。修前同一测试在 load ~245 是 2/20 |
| F2 | 去掉收窄（Idle 分支恢复修前：无条件 `event::poll(0)`），保留兜底 | 测试仍通过，且兜底真的救场 | 负载下 **0/30** 失败，其中 **1/30** 的打点证明兜底救场：`input-thread-start`、`poll-idle` 之后直接 `loop-done`（**无 `pollhup`、无 `terminal-gone`**），退出耗时 256.7 ms（一个 250 ms tick 内）；输入线程当时卡在 crossterm 里 |

结论：收窄是主要修复（把"每次空闲 tick 都是一次机会"降到"仅在上一次 crossterm 读取可能留缓冲时"，
再加调用前的零超时 `tty_readiness`）；兜底覆盖收窄仍留的微秒级窗口（F2 的那一次即证据）。两层都在。

`--exit-trace`（4e607ef）保留为测试诊断：不传时不打开文件、不输出；`exit_trace_once` 在 `OnceLock`
未设置时直接返回，连原子操作都不做。

## Slice: ADR 0028 第 4 片（压缩摘要 R5、搜索自动展开 R6）

Base `26ddda9` · 一次性 worktree `/tmp/s4-mut`（`git worktree add` 自本片 HEAD，结束后删除）·
每个变异 `python3` 补丁（带 `assert`）、跑对应测试后 `git checkout -- .` 还原。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| S1 | 按条目的展开状态进布局缓存键 | 键里覆盖恒为 `None` | `a_hit_in_a_folded_tool_body_expands_only_that_entry` | ✅ |
| S2 | 搜索匹配完整内容 | `search_modes` 返回当前（折叠）几何 | 同上 | ✅ |
| S3 | 命中只展开一条 | 命中时把所有折叠条目都展开 | 同上 | ✅ |
| S4 | resume 的摘要不是普通用户消息 | 摘要判定恒假（走普通 User 分支） | `a_compaction_summary_is_one_line_that_expands` | ✅ |

4/4 抓住。**过程记录（S1/S3 的第一轮）**：两个补丁都让 crate 编译失败（S1 改了类型别名却没改元组；
S3 留下了 `was_overridden` 的悬空引用），而当时的 `run_mut` 只 grep `FAILED`，把编译失败也当成
"没抓住"——是假象。改成先 grep `^error` 判编译失败、并修好补丁后，两条都抓住。顺带删掉了真实代码里
那个无用的 `was_overridden` 局部变量（提交 `a642b17`）。
## small-fixes：`/login` 脱敏列表刷新（2026-10-14）

Base `a90ed5c` · 一次性检出 `/tmp/small-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --test pty pty_a_login_key_is_redacted_from_an_export_in_the_same_session`。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| S1 | `Effect::LoginSave` 成功后不刷新脱敏列表（去掉 `refresh_secrets`） | 会话中 `/login` 的新 key 在 export 里被遮 | 测试 FAILED（"the export did not redact the mid-session key"） |

1/1 抓住。注意第一版测试 key 用了 `sk-` 前缀，被 export 的**启发式**脱敏（`sk-`/`Bearer`，见
`radian-session/src/export.rs`）遮住，变异因此假通过；换成不带 `sk-` 的 `late-secret-PTY-7` 后，
测试只依赖已知 secret 列表，变异被抓住。sleep 变异没有——这是行为测试，不是时序测试。

## small-fixes-2：脱敏列表的锁中毒不放开（2026-10-14）

Base `0d1dde1` · 一次性检出 `/tmp/small2-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --lib terminal::`，变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| S2 | `shared_secrets` 恢复 `.read().map(..).unwrap_or_default()`（中毒即空列表） | 锁中毒时仍取出内容、仍脱敏 | `terminal::tests::a_poisoned_shared_secrets_list_still_redacts` FAILED（`left: []`，`right: ["late-secret-PTY-7"]`） |

1/1 抓住。读侧四处（`record_panic`、运行期错误路径、picker、export）都走 `shared_secrets`；写侧
`refresh_secrets` 用 `into_inner` 追加，第二个测试覆盖中毒后的追加。

## footer-s1：footer 的默认预设、阈值、截断与容错（ADR 0027 C2，2026-10-14）

Base `e6c7013` · 一次性检出 `/tmp/footer-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --test footer <test>`，变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| FS-1 | `Preset` 的默认从 `Pi` 改成 `Classic` | 默认是 pi 的两行 | `footer.rs::the_default_footer_is_two_rows` FAILED |
| FS-2 | 忽略 `thresholds`（只用 item 的 style/默认角色） | 70/90 换样式 | `footer.rs::the_context_thresholds_change_the_style` FAILED |
| FS-3 | 窄终端先从 **左** 组省（`drop_lowest(left) || drop_lowest(right)`） | 先省右侧，再省左侧末尾 | `footer.rs::truncation_drops_the_right_group_then_the_left_and_priority_wins` FAILED |
| FS-4 | 未知条目 panic（而不是原样显示） | 未知条目原样显示 + 警告 | `footer.rs::unknown_items_placeholders_and_styles_warn_once_and_show_raw` FAILED |

4/4 抓住。绘制路径不调用 git 由结构测试 `footer.rs::the_draw_path_does_not_call_git` 覆盖（无需变异）。

## footer-s1b：无配置时的默认预设（2026-10-14）

Base `c2e835f` · 一次性检出 `/tmp/footer-mut3`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-tui --test pty pty_the_shipped_default_footer_is_two_rows_without_a_config`，
变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| FS-5 | `Footer::from_config` 的 `None => Preset::Pi` 改成 `None => Preset::Classic`（没有配置时默认不是 pi 两行） | 无配置时是 pi 的两行 | `pty.rs::pty_the_shipped_default_footer_is_two_rows_without_a_config` FAILED（第一行成了输入框的横线） |

1/1 抓住。注意：`Preset` 的 `#[default]`（FS-1）走的是 `App::new` 的 `Footer::default()`，二进制实际
默认由 `from_config` 的 `None` 分支决定，所以 FS-5 必须改那里才能打到 pty 路径。
## ADR 0028 片 5：代码块语法高亮（2026-10-15）

Base `608c2b7`（键一致性修复后 `0b035d6` 复跑 M4）· 一次性 worktree `/tmp/radian-hl-mut`，每条
跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| HL-M1 | `syntax_lines_cached` 不看 `closed`（未闭合的流式块也高亮） | `an_unclosed_block_is_not_highlighted` | ✅ |
| HL-M2 | `storage.type` 映射成 `String`（Rust 的 `fn`/`let` 用字符串色） | `rust_keyword_string_and_comment_take_their_theme_roles`（另 2 条也挂） | ✅ |
| HL-M3 | `render.rs` 把 `SyntaxRole::Keyword` 映射成 `theme.md_code`（不用主题的 `syntax_keyword` 角色） | `rust_keyword_string_and_comment_take_their_theme_roles`（另 2 条也挂） | ✅ |
| HL-M4 | 缓存键去掉 `text_len`（查找与 fill 的 fresh 判断都忽略） | `appending_a_line_rehighlights_it` | ✅（修复键宽度不一致前抓不住——那时缓存永不命中） |

4/4 抓住。**过程记录**：第一轮 M4 没抓住，暴露出 `fill_syntax_cache` 用 `body_width` 存、查找用完整
`width` 的真 bug（键永不命中）；修在 `0b035d6`，M4 复跑抓住。

## ADR 0028 片 5 审查：H1 懒高亮 + H2 上限 + vendored 语法（2026-10-15）

Base `d7f663d` · 一次性 worktree `/tmp/radian-hl2-mut`，每条跑完 `git checkout -- .`，`git status --short`
为空。命令 `cargo test -p radian-tui --test highlight`（基线 13 passed）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| H1-M1 | 测量阶段（`line_count_cached`）对每个已闭合块调 `highlight::highlighted` | `the_measure_pass_never_highlights`（计数器断言） | ✅ |
| H1-M2 | 缓存键加回宽度（`highlighted` 收 width 并进键，`syntax_lines_cached` 传真实 width） | `a_resize_reuses_the_highlight_cache` | ✅ |
| H2-M1 | 去掉 `MAX_HIGHLIGHT_LINES`/`MAX_HIGHLIGHT_BYTES` 上限 | `a_block_over_the_cap_is_not_highlighted` | ✅ |

3/3 抓住。**过程记录**：H1-M2 第一轮写成了从 `syntax_lines_cached` 传常量 `0`，键不随真实宽度变化，
测试当然抓不住——改成把真实 `width` 一路传下去后抓住（变异要真的改变行为，不能只改签名）。

## pty 偶发测试：memory 启动行（2026-10-14，结论：负载下无法复现）

Base `a6a9cce` · worktree `../radian-pi-flakymem` · 负载进程：24 个 `yes > /dev/null`（记 pid，跑完
按 pid 立刻 kill，核对过只剩测试进程）。

**原始目击**：s4 的全量 `cargo test --workspace` 在 load ~300 下，
`pty_the_memory_notice_names_the_project_state_and_the_budget` 失败一次（109 个 pty 测试里的一个）。
失败时的屏幕：启动块已展开（`▼ startup information (Ctrl-P to fold)` + profile/shell/context 行都在），
输入框与状态栏也在，但 `[memory]` 那几行不在屏幕上。

**复现尝试**：

| harness | 负载 | 次数 | 失败 |
|---|---|---|---|
| 顺序单跑 | 24×`yes`，load 122–155 | 20 | **0/20** |
| 4 并发循环各 5 次 | 24×`yes`，load 190–260 | 20 | **0/20** |

合计 **0/40**。原始目击只在"全量套件 + load ~300"（几十个 TUI 进程同时争用）下出现过一次。

**静态分析（未能定案，如实记录）**：
- 测试的时序假设：`expand_startup()` 等的是**代理**（`▼ startup information` 或第一行内容 `profile:`），
  不是它随后断言的那几行；`screen()` 只反映 reader 线程此刻已读到的字节。
- 但"帧只画了一半"解释不了那个屏幕：`rebuild_startup_entries`（`app.rs`）把整个启动块**一次性**
  splice 进 `entries`（`set_startup`/`toggle_startup` 各一次 update → 一帧），所以任何一帧要么是
  折叠块要么是完整展开块；而且屏幕里输入框/状态栏（字节流更靠后）都在，唯独中间几行缺失。
- 因此**机制未确定**：既不能证明是产品少画了行，也不能用已读到的字节解释它。没有产品侧证据
  （`App` 的启动块重建是原子的、有单元测试钉住），故不改产品代码；也没有可复现的"修前"计数，
  故不改测试（改了也无从验证）。
- **给下一片的建议**（未实施）：把该测试的等待从代理改成它真正断言的文本（`wait_for_screen("[memory]")`
  或 `wait_until(|s| s.contains("[memory]"))`）——这类"等状态"改动本身不会引入回归，但需要有复现
  才能证明它治的是这个偶发；本次 0/40 不构成证据。

**流程记录（我的失误）**：第一版 harness 把 20 个 `yes` 和测试循环放在同一个 shell 里，末尾的
`wait` 把永不退出的 `yes` 也等上了，命令挂了 7 小时 42 分，那些 `yes` 一直占着 CPU（整晚 load
150–450 有它们一份），最后由 owner 按 `/tmp/flaky-pids.txt` 的 pid 逐个核对后杀掉。**以后起负载
进程：记 pid，`wait` 只等测试循环的 pid（或把负载放进 `wait` 范围外的子 shell），跑完立刻按 pid kill。**
## footer-s2：贡献者机制、git 后台刷新、worktree/status 与 [tui.widgets]（ADR 0027 C3，2026-10-16）

Base `f699306` · 一次性 worktree `/tmp/radian-mut-s2`（`git worktree add --detach`），每条跑完
`git checkout -- .`，`git status --short` 为空。命令 `cargo test -p radian-host …` /
`cargo test -p radian-tui --test footer …`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| FS2-A | 刷新失败时用空值覆盖（`write(item, vec![])` 换掉 `mark_stale(item)`） | `a_failure_keeps_the_value_marks_it_stale_and_notices_once` | ✅ |
| FS2-B | 值没变也发脏通知（`write` 去掉"值相同就返回 `false`"的早退） | `a_refresh_writes_the_store_and_an_unchanged_value_is_not_dirty` | ✅ |
| FS2-C | 绘制路径里调 git（在 `footer::lines` 加 `std::process::Command::new("git")`） | `the_draw_path_does_not_call_git`（结构测试，正对照钉住调用在 `contrib.rs`） | ✅ |
| FS2-D | 连续失败每次都发 notice（`failures == 1` 换成 `>= 1`） | `a_failure_keeps_the_value_marks_it_stale_and_notices_once` | ✅ |
| FS2-E | 每个仓库都当成 worktree（`dir != common` 换成恒真） | `the_git_contributor_detects_a_worktree`（主检出必须是 `None`） | ✅ |
| FS2-F | untracked 计入 modified（去掉 `??` 分支） | `the_git_contributor_publishes_the_change_counts` | ✅ |

6/6 抓住。**过程记录**：FS2-C 第一轮把改动插到 `pub fn lines(` 签名处，编译不过（不是"测试抓住"，
是红），改成插进函数体后由结构测试抓住。FS2-A 的第一次断言（变 stale 也是重绘）依赖脏标志被消费，
说明 `write`/`mark_stale` 的返回值只表达"值是否变化"，脏标志单独由 `take_dirty` 消费——这正是
ADR 0023 U2 的协议，不改。

## footer-s2c：后台 git 加 `--no-optional-locks`（ADR 0027 C3，2026-10-16）

Base `d483ec9` · 一次性 worktree `/tmp/radian-mut-s2c`（`git worktree add --detach`），每条跑完
`git checkout -- .`，`git status --short` 为空。命令 `cargo test -p radian-host --lib the_background_git`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| FS2C-A | 去掉 `--no-optional-locks` 参数 | `the_background_git_disables_optional_locks` | ✅ |
| FS2C-B | 去掉 `GIT_OPTIONAL_LOCKS=0` 环境变量 | `the_background_git_disables_optional_locks` | ✅ |

2/2 抓住。两个都断言，是因为新 git 认参数、旧 git 只认环境变量：只留一个就是给旧版本留洞。

## bang：输入框的 `!` 与 `!!`（ADR 0033，2026-10-16）

Base `aec64f0` · 一次性 worktree `/tmp/radian-mut-bang`（`git worktree add --detach`，独立
`CARGO_TARGET_DIR`，避免 build.rs 的 manifest 路径污染共享 target），每条跑完 `git checkout -- .`，
`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| BANG-A | `!!` 也进上下文（`bash_done` 的 `if !exclude_from_context` 换成 `if true`） | `a_bang_done_injects_and_audits_and_a_bangbang_only_audits` | ✅ |
| BANG-B | `safe` 档照样执行（`begin_bash` 的 `if !self.bash_available` 换成 `if false`） | `a_bang_on_the_safe_tier_does_not_run` | ✅ |
| BANG-C | 不写 journal（删掉 `Effect::AuditUserBash` 的 push） | `a_bang_done_injects_and_audits_and_a_bangbang_only_audits`（audit 断言/索引） | ✅ |
| BANG-D | 绕过沙箱（`main.rs` 的 RunBash 用 `std::process::Command::new("/bin/sh")` 直接跑，替换 `run_user_bash`） | `the_bang_command_runs_through_the_sandboxed_executor`（结构测试） | ✅ |
| BANG-E | `!` 当普通 turn 发出去（`begin_bash` 发 `SendFollowup`，即走模型/工具/审批那条路） | `a_bang_routes_to_a_run_bash_effect` | ✅ |

5/5 抓住。**过程记录**：BANG-D 第一次只把 `run_user_bash(...)` 换成 `Command::new(...).status()`，
类型不匹配、根本没编译（不是"测试抓住"）——换成构造 `UserBash` 的完整裸跑后才由结构测试抓住。
BANG-E 是"走审批"这一条的最近代理：真正的审批框需要把命令交给模型当工具调用，最便宜的等价变异
就是把它发成 turn；ptytest `pty_bang_runs_and_enters_the_context` 另有一条断言"屏幕上不出现
`approve tool call`"，两层一起把"`!` 不走审批"钉住。
