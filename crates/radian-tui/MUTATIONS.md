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

## 手测第三轮（ui-r3：光标、❯、工具输出的 Tab、Shift+Enter）

Base `1a6f211` · 一次性 worktree `/tmp/ui3-mut*`（跑完删除）· 每个变异 `python3` 补丁（带 `assert`）、
跑对应测试后 `git checkout -- .` 还原。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| I1 | 提示符按**显示宽度**算列 | box 光标恢复 `BOX_PROMPT.len()`（字节） | `the_box_cursor_counts_the_prompts_display_width` | ✅ |
| I3a | 工具正文显示前展开 Tab | `prose_lines` 不再把 `	` 展开成四个空格 | `a_tool_body_expands_its_tabs_for_display_and_copies_the_source` | ✅ |
| I3b | Tab 按四列参与折行 | 三处 `ch_width` 都退回"Tab 宽 0" | `a_tab_wraps_as_four_columns` | ✅ |
| I4 | Shift+Enter 换行不发送 | `newline` 的默认键去掉 `shift-enter` | `shift_enter_inserts_a_newline` | ✅ |

4/4 抓住。**过程记录（三处假象）**：①I3 第一版只改了 `wrap_char_offsets`，而测试调的是 `wrap`
（三处宽度规则各一份）→ "没抓住"是补丁没覆盖；②`run_mut` 用 `^error` 判编译失败，而失败测试的输出里
也有 `error: test failed` 一行 → 把"抓住"误判成编译失败，已改成只匹配 `^test .* FAILED` 与
`^error[`/`^error:`；③两次 `cargo test --lib tools a_...` 又给了两个位置参数（只认第一个）→ 空跑。
④I3b 的正则忘了源码里是字面 `\t` 两个字符而不是制表符 → 0 处匹配（`assert` 如实报 PATCH FAILED，
没有当成"抓住"）。
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

## Slice: unified block layout (ADR 0028 rules 1/2/5, `blocks2`)

Base: `f0f327a` · one-shot worktree `/tmp/b2-mut` (created from HEAD, deleted after) · each
mutation applied by a patch that asserts its anchor is present, then `cargo test -p radian-tui
--test <file> <filter>`; files restored after each; `git status --short` empty at the end.

| # | Rule mutated | Mutation | Caught by |
|---|---|---|---|
| B1 | 1: the container owns the gap | `block_gap` returns 0 for ordinary blocks (no blank lines) | `render.rs::the_block_layout_snapshot` FAILED (the golden rows shift) |
| B2 | 1: the strip spares the banded pads | the strip also eats a `pad` line | `render.rs::the_block_layout_snapshot` FAILED (the user block loses its pads) |
| B3 | 4: the result hugs its call | the tool result is appended instead of inserted after its call | `render.rs::the_block_layout_snapshot` FAILED (call/result order) |
| B4 | 1: the count includes the gaps | the total in `visible_lines` sums only the entry counts | `virtual.rs::every_window_matches_the_full_wrap` FAILED |

4/4 caught; no misses. B3's rule was later superseded by ADR 0028 (the merged card,
`blocks3`), which reuses the same pairing and the same status glyphs.

## Slice: the merged tool card (ADR 0028, `blocks3`)

Base: `1a3b798` · one-shot worktree `/tmp/b3-mut` (created from HEAD, deleted after) · each
mutation applied by a patch that asserts its anchor is present, then `cargo test -p radian-tui
--test card`; files restored after each; `git status --short` empty at the end.

| # | Rule mutated | Mutation | Caught by |
|---|---|---|---|
| C1 | 1: a call and its result are one card | the pairing marks both entries `Solo` (two cards) | `card.rs::the_ascii_glyph_set_draws_ascii_borders_and_brackets` FAILED |
| C2 | 6: the meta writes only existing data | `duration_ms.or(Some(0))` invents a duration | `card.rs::other_tools_are_their_head_and_their_output_section` FAILED |
| C3 | 2: the frame characters are not text | the body row's text gets a leading `│ ` | `card.rs::a_failed_call_labels_its_section_error` FAILED |
| C4 | 4: a folded hint is not a section | `plan_has_output` returns true for any non-empty plan | `card.rs::a_call_with_no_output_draws_no_output_section` FAILED |

4/4 caught; no misses. C3's assertion is the copy: `a_copy_of_a_card_has_no_border_characters`
pins the clipboard directly.
## footer-s3：opencode-go 用量、deck、逐字段配置（ADR 0027 C3/C4、修订 3，2026-10-16）

Base `6cd0879` · 一次性 worktree `/tmp/radian-mut-f3`（`git worktree add --detach`，独立
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
## resize-fix：空闲时也要重新布局＼�2026-10-16）

Base `ea67319` · 一次性 worktree `/tmp/radian-mut-resize`（`git worktree add --detach`，独立
`CARGO_TARGET_DIR`）· 每条跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| F3-A | 显示已用 percent 而非剩余（`100 − percent` 换成 `percent`） | `the_display_is_the_remaining_share` | ✅ |
| F3-B | `items` 覆盖不生效（`resolve_item` 忽略 items 表） | `items_config_sets_symbol_style_and_format` | ✅ |
| F3-C | 分隔符用正文默认色（`row.separator_style` 换成 `Style::default()`） | `the_separator_is_configurable_and_styled` | ✅ |
| F3-D | 没有用量数据时留空行（去掉空行过滤） | `the_deck_usage_row_appears_only_with_data`（行数断言） | ✅ |
| F3-E | key 泄漏进展示文本（把 `self.key` 拼进 contribution） | `the_contributor_fetches_and_publishes`（假 key 搜索） | ✅ |
| F3-F | 颜色写法被当成角色名忽略（`parse_item_style` 只认角色） | `starship_colors_parse` | ✅ |

6/6 抓住。**过程记录**：F3-A 第一轮 anchor 没对上（`cargo fmt` 把 `remaining` 折成多行），
改成折行后的文本才变异成功；F3-D 第一轮**没抓住**——测试只查文本，加一行空白文本测试照样过，
把断言改成 `footer.lines(...).len()`（2 行 vs 3 行）后才抓住。
| RESIZE-A | Idle 分支不做尺寸比较（`size_changed()` 恒 `None`） | `pty_a_bare_resize_re_lays_out` | ✅ |
| RESIZE-B | 恢复无条件 `event::poll(0)`（去掉 flaky-master 的收窄） | `pty_a_closed_master_ends_the_app_cleanly` | ❌ 未抓住 |

RESIZE-B **如实记录**：直接跑 5/5 通过；按 brief 加 4×`yes` 负载再跑 5/5 仍通过。这与本文件
`flaky-master-fix` 的 F2 结论一致——**兜底（主循环每 250 ms 的 `terminal_is_gone`）会救场**，
把"输入线程卡在 crossterm"变成"仍在 250 ms 内干净退出"，所以 closed-master 测试抓不到收窄的缺失
（收窄的价值是省 CPU，不是保证退出）。这次修复没有动收窄，两层防护保持原样。

RESIZE-A 的过程记录：第一轮把 `size_changed()` 的**诊断调用**和真实调用写成了两次，
诊断那一次把变化吃掉了（trace 里 `changed=Some` 但 `detected` 为 0），改成只调用一次后才由
`pty_a_bare_resize_re_lays_out` 抓住——变异要真的改变被测行为，不能只加观察。

## completion-menu（可选补全菜单 + 生效思考等级，��

Base `8789877` · 一次性 worktree `/tmp/radian-cm-mut`，每个变异跑完 `cp /tmp/cm-backup` 还原，最后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| CM-M1 | 菜单打开时 `Action::Send` 不再被菜单接住（Enter 会发送） | `mention_tab::enter_accepts_the_highlighted_candidate_without_sending` | ✅ |
| CM-M2 | 去掉菜单对 `HistoryPrev`/`HistoryNext` 的接管（Up/Down 落回历史/关闭菜单） | `mention_tab::the_menu_opens_on_the_first_row_and_up_down_wrap` | ✅ |
| CM-M3 | `effective_thinking_level` 直接返回请求值（footer 显示配置值而非钳制值） | `command::the_footer_thinking_level_is_the_clamped_one` | ✅ |
| CM-M4 | 去掉 `completion_set` 里的高亮重置（打字后高亮不回到第一行） | `update::typing_filters_the_slash_menu_and_resets_the_highlight` | ✅ |
| CM-M5 | `accept_slash_candidate` 固定取第 0 行（忽略高亮） | `update::the_slash_menu_moves_its_highlight_and_enter_accepts_it` | ✅ |
| CM-M6 | `thinking_candidates` 的 description 丢掉 wire 值，或 `current` 不按等级设置 | `command::thinking_candidates_name_the_wire_value_and_mark_the_current_level` | ✅ |

6/6 抓住。
## trust-Esc：Esc 不得信任（2026-10-14）

Base `f1993bb` · 一次性 worktree `/tmp/radian-esc-mut`，跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ESC-M1 | 启动信任 modal 的 Esc 映射从 `UntrustHere` 改成 `TrustHere` | `update::tests::escaping_the_trust_modal_does_not_trust` | ✅ |

1/1 抓住。

## trust-Esc＼�2026-10-14）

Base `edfa320` · 一次性 worktree `/tmp/radian-esc-mut2`，跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ESC-M2 | Esc 映射回 `UntrustHere`（持久，会写 `trust.json`） | `update::tests::escaping_the_trust_modal_untrusts_this_session_only` | ✅ |

1/1 抓住。（早先 ESC-M1「Esc → `TrustHere`」见上一节。）

## ask-ui：问答界面照 omp（ADR 0024/2，2026-10-16）

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ASK-A | Other 编辑时吞掉 ↑↓（把 Other 行当多行字段） | `other_keeps_its_text_while_moving_with_arrows` | ✅ |
| ASK-B | `⏎` 在单选时不前进 | `a_single_question_selects_and_advances` | ✅ |
| ASK-C | 多选 `⏎` 变成单选 | `a_multi_question_toggles_and_other_is_its_own_answer` | ✅ |
| ASK-D | 必答题未答也能提交 | `a_required_question_blocks_the_submit` | ✅ |
| ASK-E | 备注不进答案（`note_of` 恒 `None`） | `n_opens_a_note_and_the_note_reaches_the_answer` | ✅ |
| ASK-F | 两个 `recommended` 不报错 | `every_question_limit_is_an_invalid_arguments_failure` | ✅ |
| ASK-G | 标签不截断（超 16 宽） | `a_long_tab_label_is_truncated` | ✅ |

7/7 抓住。**过程记录**：ASK-G 第一轮 anchor 没对上（`cargo fmt` 折行），改成折行后的文本才变异成功。
## glyphs（品牌字形与 unicode 档字形覆盖，ADR 0027）

Base `45723f6` · 一次性 worktree `/tmp/radian-glyphs-mut`，每个变异跑完 `cp /tmp/glyphs-backup` 还原，最后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| G-M1 | unicode 品牌改回 `㎭` | `footer::tests::the_brand_mark_is_per_glyph_set` | ✅ |
| G-M2 | reasoning 的 unicode 图标改回未覆盖的 `◐` | `footer::tests::unicode_tier_glyphs_are_all_verified_covered` | ✅ |
| G-M3 | spinner 改回未覆盖的 braille `⠋⠙⠸⠴` | 同上 | ✅ |
| G-M4 | host 的 unicode 图标改回未覆盖的 `⌂` | 同上 | ✅ |

4/4 抓住。

（blocks2 合并后，在 `90dfd27` 重跑 G-M1..M4，并加 G-M5：unicode `tool_status` 改回未覆盖的 `⟳ ✔ ✘`；5/5 抓住。一次性 worktree `/tmp/radian-glyphs-mut2`，还原后 `git status --short` 为空。）

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| G-M5 | `Glyphs::Unicode.tool_status()` 改回 `⟳ ✔ ✘` | `footer::tests::unicode_tier_glyphs_are_all_verified_covered` | ✅ |
## bang-cancel：Ctrl-C 停止运行中的 `!`（ADR 0033，2026-10-16）

Base `3ccd4db` · 一次性 worktree `/tmp/radian-mut-bangcancel` · 跑完 `git checkout -- .`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| BC-A | 去掉 `Effect::Cancel` 分支里的 `bash_cancel.cancel()` | `pty_ctrl_c_stops_a_running_bang` | ✅ |

Ctrl-C 只有在 turn 运行时才表示取消（否则是 arm quit），所以测试用一个永不结束的
`Reply::SseThenHang` 起一个 turn，再发 0x03，断言 `!` 块显示 `[cancelled]`。
## startup-line（摘要行写真实沙箱后端，2026-10-14）

Base `62ae29e` · 一次性 worktree `/tmp/radian-sl-mut`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S-M1 | TUI 摘要行改回搜索 `sandbox_facts` 文本（macOS 上写 `none`） | `pty::pty_the_startup_summary_names_the_real_sandbox_backend` | ✅ |

1/1 抓住。

## brand-config（[tui] brand，ADR 0027，2026-10-15）

Base `4772113` · 一次性 worktree `/tmp/radian-bc-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| B-M1 | footer 的 brand 条目忽略配置，用 glyphs 默认 | `footer::the_brand_config_replaces_the_glyph_default` | ✅ |
| B-M2 | `App::brand_mark` 忽略配置（banner 用 glyphs 默认） | `pty::pty_the_brand_config_is_the_banner_fallback` | ✅ |
| B-M4 | `App::brand_mark` 缺省不走 glyphs 档（返回固定值） | `footer::the_brand_default_follows_the_glyph_set` | ✅ |

3/3 抓住。
## select-list：共享列表原语与 /model 平铺（ADR 0035）

Base `3c1c579` · 一次性 worktree `/tmp/radian-sl-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SL1 | `window()` 恒返回 0（窗口不跟随） | `select_list::the_window_follows_the_selection_and_shows_a_scrollbar` | ✅ |
| SL2 | `scrollbar_thumb()` 恒返回 0（滑块位置错） | 同上 | ✅ |
| SL3 | 接受候选时插入 `description`（provider）而不是 `provider/model` | `the_flat_model_list_filters_by_provider_and_accepts_the_pair` | ✅ |
| SL4 | `select_list::matches` 只匹配 label（丢掉 description） | `select_list::the_filter_matches_the_label_and_the_description` | ✅ |

4/4 抓住。
## startup-speed（摘要行说 "macOS sandbox" / "Linux sandbox"，2026-10-15）

工作区 `<home>/Code/radian-dogfood-ss`，分支 `dogfood/startup-speed`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SP-T1 | `radian_tui` 摘要行不再用 `SessionFacts::sandbox_word`，改回显示后端名（或搜索 `sandbox_facts`） | `pty::pty_the_startup_summary_names_the_real_sandbox_backend`（同一条用例现在同时断言摘要里的 `macOS sandbox` 与展开后 `profile:` 行里的 `macos-seatbelt`） | ✅ |

## ime-latency（输入法一次提交的整串汉字，2026-10-15）

Base `e2a0392` · 一次性 worktree `/tmp/radian-ime-mut*`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| IM-M1 | `read_input` 恢复 `tty_readiness(timeout)` 的非零等待 | `pty_a_committed_ime_string_reaches_the_input_in_one_frame`（整串 687 ms > 150 ms 界限） | ✅ |
| IM-M2 | 去掉 `MAY_HAVE_BUFFERED` 收窄（Idle 分支无条件走 crossterm 的 `poll(0)`） | `pty_a_closed_master_ends_the_app_cleanly` | ❌ 未抓住（0/5） |
| IM-M3 | `tty_readiness` 不检查 `POLLHUP`（该测试本来的目标变异） | `pty_a_closed_master_ends_the_app_cleanly`（卡死，exit 124） | ✅ |

IM-M2 与既有记录一致（见上文 `RESIZE-B`：closed-master 测试抓不到收窄的缺失——收窄省的是 CPU，
不是保证退出；退出由第一次 `tty_readiness` 的 Gone 与主循环兜底覆盖，两者都用同一个
`tty_readiness`）。IM-M3 证明这次改动没有削弱该测试对 POLLHUP 的检查。

## markdown-b：间距与 Markdown 各元素（ADR 0035，2026-10-16）

Base `2ecb02b` · 一次性 worktree `/tmp/radian-mut-mdb` · 每条跑完 `git checkout -- .`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MD-A | 某元素自己多加一行空白（块间补两行） | `markdown_elements_have_one_blank_line_between_them` | ✅ |
| MD-B | 续行固定 2 格（`marker_width` 恒 2） | `lists_use_a_dash_and_align_continuations_to_the_marker` | ✅ |
| MD-C | 表格只画表头线（`row_index == 0`） | `a_table_has_a_border_between_every_row` | ✅ |
| MD-D | 不画围栏行 | `a_code_block_shows_its_fence` | ✅ |
| MD-E | `<br>` 不换行 | `html_br_hr_and_blockquote_render` | ✅ |

5/5 抓住。

### 补：任务勾选框的 nerd 档（并入 main 之后的交接，2026-10-16）

`MdGlyphs::for_glyphs` 给 unicode 与 nerd 各一张表（`task_unchecked`/`task_checked`），但原来只有
unicode 快照与 ascii 两处覆盖，**nerd 表改掉没有测试变红**（unicode 与 nerd 的勾选框字形相同，
nerd 那条断言缺失）。新用例 `markdown::every_glyph_tier_renders_its_own_task_checkbox_prefix`
对三档各渲染一个 `- [ ] todo` 与一个 `- [x] done`，断言各自的前缀。

Base = 合并 origin/main 后的 `6db84c1` · 就地变异：改前 `cp crates/radian-tui/src/blocks.rs
/tmp/mdb-blocks-backup.rs`，改后跑测试，再从副本写回并 `cmp`（逐字节相同），`git status --short`
为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MD-F | nerd 表的 `task_unchecked` 从 `"☐ "` 改成 `"[?] "`（unicode 与 ascii 两档不动） | `markdown::every_glyph_tier_renders_its_own_task_checkbox_prefix`（`tier nerd: missing "☐ todo"`） | ✅ |

1/1 抓住。本条的基线是合并 main 之后的树，与前面的 MD-A…MD-E（基线 `2ecb02b`）不同。
## small-fixes-2：footer 状态与 Esc 取消

Base `e157bfb` · 一次性 worktree `.radian-worktrees/sf2-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SF4 | 去掉 `ToolFinished` 清 note 的分支 | `tests/render.rs::the_note_clears_when_the_tools_finish` | ✅ |
| SF5 | `Action::Escape` 在回合运行时直接返回（不取消） | `pty_escape_cancels_a_running_turn` | ✅ |

SF2（高亮计数改线程局部）与 SF3（build.rs 用运行时 `CARGO_MANIFEST_DIR`）没有常规变异测试：
SF2 是把测试自身的干扰源去掉，SF3 是编译期路径问题；两者的证据分别是「高亮测试单独跑 5/5」
与「把 worktree 改名后 build script 仍找得到 `third_party/syntaxes`」。
## fork-commit（/fork 等落盘, 2026-10-15）

Base `52265c1` · 一次性 worktree `/tmp/radian-fc-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| FC-M1 | `/fork` 不看 `turn_committed`（间隙下立即 fork, 空历史） | `pty_a_fork_waits_for_the_turn_to_be_committed` | ✅ |
| FC-M2 | `ForkDeadline` 超时也执行 fork（生成空子会话） | `pty_a_fork_gives_up_when_the_turn_is_still_being_written` | ✅ |

2/2 抓住。
## spinner（运行指示一处定义 + 输入框上沿状态线，ADR 0027/9/10，2026-10-15）

Base `c0ae44c` · 一次性 worktree `/tmp/radian-sp-mut2`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SP-M1 | 旋转档节拍 250 ms 改 100 ms | `spinner::tests::the_frame_advances_with_time` | ✅ |
| SP-M2 | 状态线的两个空格变一个 | `render::the_input_rule_shows_the_spinner_status_while_running` | ✅ |
| SP-M3 | `spinner_tick` 不再判断 `spinner_visible`（空闲仍驱动重绘） | `pty::pty_an_idle_screen_is_not_redrawn` | ✅ |
| SP-M4 | `glyphs = ascii` 时仍用弧字形 | `spinner::tests::the_ascii_set_falls_back` | ✅ |

4/4 抓住。（SP-M1 第一次用「节拍改 50 ms」未被抓，因为 `index` 当时重复硬编码了 250 ms；
改成 `index` 用 `kind.interval()` 后，100 ms 变异被抓住。）
## effect-settle（超过 MAX_EFFECT_ROUNDS 时 carry，2026-10-15）

Base `9cb727d` · 一次性 worktree `/tmp/radian-es-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ES-M1 | `run_effect_rounds` 恢复 `return Err`（超过上限就退出） | `effect_rounds_carry_the_rest_in_order` | ✅ |
| ES-M2 | 超过上限后 `effects.clear()`（丢掉剩余 effect） | 同上（顺序/完整性断言） | ✅ |

2/2 抓住。

## bar-input（ADR 0035 片 C：bar 输入框 + 上方状态行，2026-10-15）

Base `d72d17c`（本片三个提交的最后一块）· 就地变异：改前 `cp` 到 `/tmp`，改后跑测试，
再从副本写回，逐个 `diff` 确认与副本逐字节相同；`git status --short` 只留本片的新测试。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| BI-M1 | `bar` 的高度空闲时不带状态行（`rows + u16::from(spinner_status().is_some())`） | `render::the_bar_status_row_never_moves_the_band_or_the_transcript`、`render::the_bar_input_takes_exactly_the_status_row_and_its_text_rows` | ✅ |
| BI-M2 | `bar` 高度按 `rows + 2`（多一个空行） | `render::the_bar_input_takes_exactly_the_status_row_and_its_text_rows` | ✅ |
| BI-M3 | 缺省回到 `lines`（`#[default]` 挪回 `Lines`） | `config::the_input_style_is_applied_and_an_unknown_value_falls_back_to_bar`、`render::the_empty_input_shows_the_placeholder_after_the_prompt` | ✅ |
| BI-M4 | 竖条不是强调色（`rail_style = band`） | `render::the_bar_input_style_draws_a_full_width_band_with_the_rail`（竖条的 fg 断言） | ✅ |
| BI-M5 | 背景带不到整行宽（去掉尾部补白） | `render::the_bar_band_fills_the_terminal_width`、`the_bar_input_style_draws_a_full_width_band_with_the_rail` | ✅ |
| BI-M6 | spinner 后只留一个空格 | `render::the_bar_status_row_never_moves_the_band_or_the_transcript` | ✅ |
| BI-M7 | `esc to interrupt` 对任何状态都显示（不只 turn 运行中） | 同上（`Running a command` 那一段） | ✅ |
| BI-M8 | 状态行的时间保留一位小数（`12.0s` 而不是 `12s`） | `timing::tests::the_numbers_format_short` | ✅ |

8/8 抓住。两处过程记录：

- BI-M7 第一次（测试在 40 列下渲染）**没抓住**：整行状态在 40 列被裁到
  `… · esc to`，`!contains("esc to interrupt")` 因截断而成立。把该用例的三次渲染改成 80 列后
  M7 被抓住——**断言前先确认被断言的字串真的完整落在屏幕上**。
- 与 bar 无关但同片验证的一条：去掉 `signals.rs` 里信号任务的强制退出（不再
  `std::process::exit(128 + signal)`）后 `pty::pty_a_hung_main_loop_still_exits_on_a_signal`
  FAILED（该用例的挂起机制本片改成了 `--slow-draw`，见下），还原后逐字节相同、
  测试重新通过。这证明改用 `--slow-draw` 没有削弱该用例对看门狗的检查。
## panels-adopt：其余弹出层改用共享面板与列表（ADR 0035/2，2026-10-16）

Base `d47e876` · 一次性 detached worktree `/tmp/pa-mut`（跑完 `git worktree remove --force`）。
每个变异用 `--no-fail-fast`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| PA-M1 | 审批框 `y` 答 `Deny` 而不是 `AllowOnce` | `approval::keys_answer_the_prompt_with_the_queue_id` | ✅（left `Deny`、right `AllowOnce`） |
| PA-M2 | 信任弹窗 Esc 选持久的 `UntrustHere`（会写盘） | `update::escaping_the_trust_modal_untrusts_this_session_only` | ✅（left `UntrustHere`、right `UntrustSession`） |
| PA-M3 | 会话选择器过滤只看 id（不看 summary） | `picker::tests::filtering_narrows_by_id_and_summary_and_escape_quits` | ✅（可见行 0，期望 1） |

3/3 抓住。三个变异都是**原有行为测试**，证明这次外观迁移没有削弱它们（审批的 `y`、信任 Esc
只限本次不写盘、选择器按 id 或 summary 过滤）。外观本身由新的 TestBackend 快照覆盖。

## glyphs2: ADR 0027 items 1/2/4/5 (2026-10-15)

Base `1155b98` · one-shot worktree `/tmp/g2-mut` (from HEAD, deleted after) · each mutation
applied by a patch with an anchor assertion, then `cargo test -p radian-tui --test footer
<name>` / `--test render <name>` / `--lib <name>` (one positional filter at a time) under
`timeout 300`; files restored after each; `git status --short` empty at the end.

| # | Rule mutated | Mutation | Caught by |
|---|---|---|---|
| G1 | 2: the unicode token arrows | `Glyphs::token_arrows` returns `^`/`v` for unicode | `footer.rs::the_unicode_footer_snapshot_at_80_and_120_columns` FAILED |
| G2 | 2: the unicode fold marker | `Glyphs::fold_marker` returns `>` for unicode | `render.rs::the_block_layout_snapshot` and `render.rs::thinking_folds_to_one_line_and_ctrl_t_expands_it` FAILED |
| G3 | 5: transparent clears the background | the post-pass is skipped (`if false && self.transparent`) | `footer.rs::transparent_clears_the_footer_background` FAILED |
| G4 | 1/2: the coverage rule | the unicode `reasoning` icon becomes `◐` (no reference font has it) | `footer::tests::unicode_tier_glyphs_are_all_verified_covered` FAILED |

4/4 caught. G2's glyph is drawn in the transcript (the folded-reasoning marker), so its
catching tests are the render snapshots, not the footer ones.
## md-test-speed：中文 Markdown 选区测试从 N² 改成每格 4 次（2026-10-15）

`a_chinese_markdown_selection_does_not_panic` 原来对每两个格子都做一次选区（N²，每次重新排版），在工作区测试里约占 700 s；
改成每个格子分别与第一个格子、最后一个格子、自己组成选区（每个格子都当过起点和终点），同机 debug 构建约 14 s。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MS-M1 | `md_chars` 给所有合成格子一个假偏移（上一个源字节 + 1） | `markdown::a_chinese_markdown_selection_does_not_panic`（`select.rs:213` panic） | ✅ |
| MS-M2 | 只给列表符号 `•` 这一个合成格子假偏移 1（落在「能」的 UTF-8 中间） | 同上 | ✅ |

另记一个无效变异：「只给第 200 字节之后的第一个合成格子假偏移」——那一处的上一个字符是 ASCII，`+1` 正好是合法边界，不会 panic，不能说明覆盖缺口；MS-M2 用确定落在多字节中间的单格变异代替。

## resume-perf2：`push_message` 的 O(n²) 与 Markdown 只数不建（2026-10-16）

Base `oc/resume-perf2` · 就地变异：改前 `cp` 到 `.radian/mut/`，改后跑测试，再从副本写回，
逐个 `diff` 确认与副本逐字节相同（`git status --short` 只剩本片的五个改动文件）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| RP2-M1 | 工具结果配对从前端扫（`iter_mut().enumerate()`，索引取 `rev`），即修复前的第二次 O(n²) | `app::tests::tool_result_pairing_does_not_walk_the_transcript`（400 个结果走了 240 200 条） | ✅ |
| RP2-M2 | 计数路径改成 `md_lines(..).len()`（量测即排版） | `blocks::tests::the_measure_pass_never_lays_out_markdown` | ✅ |
| RP2-M3 | 去掉 `md_lines_cached` 的懒填充（行缓存永不写入） | `blocks::tests::a_repeat_frame_reuses_the_markdown_line_cache` | ✅ |
| RP2-M4 | `wrap_line_count` 的 ASCII 快路径给错宽度（+1） | `virtual::every_window_matches_the_full_wrap`（总数与实际排版不符） | ✅ |
| RP2-M5 | `md_line_count` 去掉「补回被 pulldown 吃掉的结尾换行」 | `blocks::tests::md_line_count_matches_the_layout` | ✅ |

5/5 抓住。两处过程记录：

- RP2-M2 第一次变异放在 `md_line_count_cached` 的兜底分支，**没抓住**：量测时
  `fill_md_count_cache` 先填好计数缓存，`md_line_count_cached` 总是命中，兜底分支在生产路径里
  几乎不可达。改成变异真正的计数入口 `fill_md_count_cache` 后抓住——即变异要落在会被执行的路径上。
- RP2-M1 的变异保留了配对的正确性（前端扫也找到同一个 call id），只放大扫描量；测试断言的是
  「走过的条目数」，不是结果，所以抓住了「修复后的算法没被还原」这层回归。

## band-padding：带的半格留白与 thinking 贴住下一块（ADR 0035，2026-10-15）

Base `a36a099` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`，每条只跑列出的测试，
`cargo test -p radian-tui --no-fail-fast <目标> -- --exact`）。变异脚本带 `assert old in s`
（吸取 U3/U4 的教训：no-op patch 是假通过）。结束后 `git status --short` 只剩本节的文档改动。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| BP1 | 留白行画整行空格（`glyph.repeat(width)` → `" ".repeat(width)`） | 半格留白必须真的是 `▄`/`▀` | `render::a_user_block_pads_its_band_with_half_cells_at_80_and_120` FAILED |
| BP2 | 留白用文字色（把 `theme.text` 当带传给 `band_pad_line`） | 留白的前景色是**带的背景色**，不是文字色 | 同上 FAILED |
| BP3 | `ascii` 档也加留白（`banded_pad` 恒为 true） | `ascii` 档没有留白行 | `blocks::tests::a_user_block_gets_padding_rows_and_they_map_to_gaps` FAILED |
| BP4 | thinking 与下一个块之间多一行（`prev.kind == Reasoning` 返回 1） | thinking 贴住下面的块（回复或工具卡片） | `render::thinking_hugs_the_block_below` FAILED |
| BP5 | 留白行可被复制（把 `▄` 混进 user 条目的 display text） | 留白行不参与复制 | `selection::a_selection_from_a_padding_row_copies_no_pad_characters_with_cjk` FAILED |
| BP6 | 运行中卡片仍用 `~`（去掉 live spinner 替换，画回烘焙的字形） | 工具卡片运行中用 pulse spinner | `card::a_running_call_is_a_card_with_a_spinner_and_no_output_section` FAILED |
| BP7 | unicode 成功图标改回 `+` | unicode 档成功图标是 `✓` | `render::the_unicode_tool_success_glyph_is_a_check_mark` FAILED |

7/7 抓住，无等价变异。**BP5 说明**：字形只由 renderer 画出时，`logical_text` 只读条目的
display text，留白永远复制不到；能进入复制的唯一路径是把半格字形当成文本实现（BP5 正是这种朴素写法），
所以用「把 `▄` 混进 display text」模拟，`selection` 那条测试抓住。**BP6** 是既有测试
（`card.rs` 的 `a_running_call_is_a_card_with_a_spinner_and_no_output_section`）本次一并入册：
修订 3 决策 7 的「运行中用 pulse spinner 而不是 `~`」早已实现，之前没有变异记录。
## access-cmd：`/access` 确认、按项目记住、resume（ADR 0038 切片 ②，2026-10-15）

Base `d8b7688`（worktree `.radian-worktrees/access-cmd`）· 每个变异在就地改完后跑
`cargo test -p radian-tui --test <file> --no-fail-fast`，再从 `/tmp/access-mut/` 的副本还原并用
`diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| AC-M1 | `run_slash_command` 的 `/access full` 不再弹确认（条件改 `false && …`，直接切） | `access::access_full_asks_first_and_the_default_is_cancel`、`esc_cancels_the_full_confirmation`、`access_full_choices_switch_with_the_right_remember_flag` | ✅（3 条 FAILED） |
| AC-M3 | `radian_host::access::remembered_for` 从**工作区**读 `access.json`（而不是 `~/.radian/access/`） | `pty::pty_a_remembered_full_applies_at_start_with_the_project_marker` | ✅（home 里记住的 full 不再生效，首屏是 `workspace`） |
| AC-M4 | `resumed_access` 直接返回 `None`（resume 忽略 journal 里的档位） | `pty::pty_resume_restores_the_access_tier`、`radian_host::access_switch::resumed_access_reads_the_last_change` | ✅ |

3/3 抓住。AC-M3 的两半都在同一个 pty 用例里：写 `~/.radian/access/access.json` 时必须生效并带
`(project)` 标记；把同样内容写进工作区文件、用干净 HOME 启动时**必须不生效**。

## build-script：合并 dump 预先提交、build script 只校验与拷贝（2026-10-15）

Base `39c67ad`（worktree `.radian-worktrees/build-script`）。本片没有改产品行为（新 dump 与 main 的
build script 产出**逐字节相同**，md5 `34b85cf9241b1ceb6672355035c27c40`），新增的是构建脚本的两道
校验与一条冻结测试。前两条是**构建期**的负向验证（手工跑，不是 `cargo test`），第三条是新测试的变异
检查；每条跑完从 `/tmp/bs/` 的副本还原并用 `md5` 确认逐字节相同。

| # | 变异 / 负向场景 | 抓住它的东西 | 结果 |
|---|---|---|---|
| BS-M1 | `assets/syntaxes.packdump.digest` 改成别的值（dump 与语法不同步） | `build.rs` 的 digest 校验：构建失败并打印 `RADIAN_REGENERATE_SYNTAXES=1 cargo build -p radian-tui` | ✅（`cargo build -p radian-tui` 报错退出） |
| BS-M2 | dump 截断到 2000 B（坏文件 / syntect 格式变了的等价物） | `build.rs` 的 `from_uncompressed_dump_file` 校验：构建失败并打印同一命令 | ✅ |
| BS-M3 | `third_party/syntaxes/TOML.sublime-syntax` 的注释 scope 改成 `constant.numeric.toml`，重新生成 dump | `highlight::the_vendored_grammars_highlight_a_fixed_snippet_identically`（Comment → Number） | ✅（1 FAILED） |

BS-M1/M2 覆盖「旧的 dump 不会被静默用掉」，BS-M3 覆盖「重新生成真的改变了行为时看得见」。
## mode-cmd：`/mode` 的查询与切换（ADR 0038 切片 ③，2026-10-15）

Base `e19e6cf`（worktree `.radian-worktrees/mode-cmd`）· 每个变异在就地改完后跑
`cargo test -p radian-tui --test mode --no-fail-fast`（或 `--test pty pty_mode_lead`），再从
`/tmp/mut/app.rs.bak` 还原并用 `diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MD-M1 | `/mode <name>` 忽略名字，永远发出 `SetMode { General }` | `mode::mode_switches_without_confirmation` | ✅ |
| MD-M2 | `/mode`（无参数）不读投影，固定打印 `mode: general` | `mode::mode_with_no_argument_reports_the_current_mode` | ✅ |

2/2 抓住。`pty::pty_mode_lead_switches_the_role_prompt_from_the_next_turn`（真 TUI、mock 模型）
覆盖上一层的接线：`/mode lead` 的 layer 1 在下一次请求里变成 lead 角色提示词，`<role>` 之外的字节
不变；它连跑 5 次全绿。
## sandbox-unix-sockets：TUI 把 `allow_unix_sockets` 交给沙箱（ADR 0016，2026-10-15）

Base `acbc455`（`origin/main`）· 就地变异：改前 `cp` 到 scratchpad，改后从副本写回并 `cmp` 逐字节相同。
`cargo test -p radian-tui --test structure --no-fail-fast main_forwards_the_allowed_unix_sockets_to_the_sandbox`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| US-M1 | `build_sandbox` 调用的第四个实参改成 `&Vec::new()`（配置里的 socket 列表到不了沙箱） | `structure::main_forwards_the_allowed_unix_sockets_to_the_sandbox` | ✅ |

1/1 抓住。这条是**接线**变异：白名单本身的行为由 `radian-sandbox` 的逃逸矩阵覆盖（`unix_socket` 组的
`whitelisted_allowed` 对照），而「TUI 把解析后的列表交给 `build_sandbox`」只有源码结构能钉住
（`run` 在 `main.rs` 里、测试够不到），所以按 `structure.rs` 的既有做法做结构断言。

## pulse：呼吸 spinner（ADR 0027，2026-10-15）

Base `19dc0d7`（worktree `.radian-worktrees/pulse`，分支 `dogfood/pulse`）。每个变异改完后跑
`cargo test -p radian-tui --lib -- spinner`（或 `--bin radian`），再从 `.radian/scratch/` 的副本还原并用
`diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| P1-M1 | `PULSE_STEPS` 12 → 2（步数退回两档） | `spinner::tests::the_breath_walks_one_sine_period_in_twelve_steps`（取样序列 `[0, 2]` ≠ `[0, 1, 3, …, 1]`，以及 ADR 两个常量的断言）、`spinner::tests::a_pulse_that_cannot_interpolate_keeps_the_two_colour_switch`（一步 ≈117 ms 的断言）、`tests::the_breath_redraws_once_per_step_and_only_when_a_spinner_is_visible`（循环唤醒 250 ms ≠ 700 ms） | ✅（3 条 FAILED） |
| P1-M2 | `breath_color` 不再要求两端都是 RGB（非 RGB 端按 `Rgb(0, 0, 0)` 也插值） | `spinner::tests::the_breath_colour_interpolates_only_between_two_rgb_ends`（4 个 `None` 断言；`Indexed(5)` 处得到 `Rgb(45, 45, 45)`） | ✅ |
| P1-M3 | `spinner_frame_at` 去掉 `spinner_visible()` 检查（不可见时也刷新） | `tests::the_breath_redraws_once_per_step_and_only_when_a_spinner_is_visible`（空闲那次走完两个周期得到 24 帧，应为 0） | ✅ |

高亮的**正常**路径没有变：`pty_the_pulse_breathes_through_rgb_colours` 在真实 pty 上看到 `⏺` 前的
24-bit 颜色按呼吸变化（一个周期里 7 个不同颜色、包含主题的强调色与暗色两端），而
`pty_an_idle_screen_is_not_redrawn`（空闲零重绘）与 `pty_a_visible_spinner_redraws_on_its_interval`
（`arc` 250 ms 不变）都仍然通过。

## pulse：bar 竖条加高到半格、状态行点移到竖条列（ADR 0035，2026-10-15）

同上一节的工作区。改完后跑 `cargo test -p radian-tui --test render`（`--test cursor` 一并跑），再从
`.radian/scratch/render.rs.orig` 还原并用 `diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| R4-M1 | 留白行仍画 `▄`/`▀` 而不是 `▖`/`▘` | `render::the_bar_band_fills_the_terminal_width`（第一格 `▄` ≠ `▖`，两档宽度都查） | ✅ |
| R4-M2 | 竖条仍是 `▎`（`BAR_RAIL` 回退） | `render::the_bar_input_style_draws_a_full_width_band_with_the_rail`、`the_bar_band_fills_the_terminal_width`、`the_bar_input_takes_the_status_row_the_pad_rows_and_its_text_rows`、`the_bar_status_row_never_moves_the_band_or_the_transcript`、`the_empty_input_shows_the_placeholder_after_the_prompt`（5 条 FAILED），以及 `pty::pty_the_slash_completion_walks_the_command_tree` 的 `▌ /memory` | ✅（5 条 FAILED） |
| R4-M3 | 点回到正文列（状态行恢复两个前导空格与两个空格分隔） | `render::the_bar_status_row_never_moves_the_band_or_the_transcript`（第 0 格不是 `◜`）、`the_bar_rail_and_spinner_fall_back_to_ascii`、`the_input_rule_shows_the_spinner_status_while_running`、`the_input_rule_shows_compacting` | ✅（4 条 FAILED） |

光标列不动的断言在 `cursor::the_bar_cursor_stays_at_the_text_column`（空输入 x=2、`ab` x=4）；
`▖`/`▘` 这一格的前景是强调色、背景是终端默认（`the_bar_band_fills_the_terminal_width` 的第 0 格断言）。

## pulse：thinking 与下面的块之间空一行（ADR 0035，2026-10-15）

同上一节的工作区。变异改 `blocks.rs::block_gap`，跑 `cargo test -p radian-tui --lib` 与
`--test render`，再从 `.radian/scratch/blocks.rs.orig` 还原并用 `diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T5-M1 | `block_gap` 恢复「thinking 贴住下面」（`prev.kind == Reasoning` → 0） | `blocks::tests::the_container_gap_is_one_line_except_within_a_block`（pair 规则）、`render::thinking_has_one_blank_line_on_both_sides`、`render::the_block_layout_snapshot`、`render::folding_changes_the_transcript_line_count` | ✅（1 + 3 条 FAILED） |

行数计数与布局的一致性由结构保证（`line_count`、`md_line_count` 与绘制都走同一个 `block_gap`），
`blocks::tests::md_line_count_matches_the_layout` 与 `tests/virtual.rs` 的窗口测试在改前改后都通过。
另一个观察（不是本变异的缺口）：`push_message` 对没有正文的 assistant 回合也会压一个空的 Assistant
条目，它是独立的一块，所以「thinking + 无正文的工具调用」在屏幕上会是两行空行（thinking 自己的一行
+ 空块两侧的行）；`thinking_has_one_blank_line_on_both_sides` 直接压条目来测 thinking 这条规则本身，
并在注释里写明这一点。

## thin ⓪：新角色与内建取值（ADR 0029/6，ADR 0028 的片 ⓪）

Base：本片 HEAD（`oc/thin-roles`）· 就地变异，每个变异后从 `.radian/scratch/theme.rs.new`
还原并用 `cmp` 确认逐字节相同（`git status --short` 只留本片改动）。测试 `--no-fail-fast`。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| R0-M1 | `frame` 取 `accent` 的值而不是 `dim`（dark/light 两处） | 细线是弱一级 | `theme::tests::the_thin_roles_and_the_changed_builtin_values` FAILED（"frame is the dim level"） |
| R0-M2 | `user` 仍是背景角色（恢复 `text.bg(band)`，dark/light 两处） | user 是前景角色、无底色 | `theme::tests::the_thin_roles_and_the_changed_builtin_values`（"user is the accent foreground"）与 `theme::tests::the_input_bar_keeps_the_user_band_on_the_main_level` FAILED |
| R0-M3 | `reasoning` 仍斜体（dark/light 两处加回 `Modifier::ITALIC`） | reasoning 不斜体 | `theme::tests::the_thin_roles_and_the_changed_builtin_values` 与 `render::reasoning_and_assistant_text_are_visually_distinct` FAILED |

3/3 抓住，无等价变异。新角色的映射/解析另有正向断言（`theme::file::tests::the_thin_roles_map_to_their_pi_keys`、
`the_thin_roles_resolve_from_a_theme_file`、`the_thin_roles_are_settable_from_tui_colors`）。
## status：footer 的 `tool <name> running` note 与 Esc（2026-10-16）

Base `d8e4c28`（worktree `.radian-worktrees/build-script`，分支 `dogfood/status`）· 就地变异：改前把
`crates/radian-tui/src/app.rs` 复制到 `.radian/scratch/app.rs.status-orig`，变异脚本带 `assert old in s`
（no-op 会显式 PATCH FAILED）；每条跑完从副本写回并用 `cmp` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S1-M1 | `SessionView::finish_tool` 只清 note、不把 note 移到仍在运行的调用（退回旧行为：`if note_is_the_tools && self.running_tools.is_empty() { self.note = None }`） | `render::the_note_names_a_tool_that_is_still_running`（`left: Some("tool read running")`，应为 `tool shell running`） | ✅ |
| S1-M2 | `ModelRequestStarted` 不再调用 `forget_running_tools()` | `render::the_tool_note_does_not_survive_the_next_model_request`（note 停在 `Some("tool shell running")`，应为 `None`） | ✅ |
| E1 | `Action::Escape` 的空输入分支退回 `UpdateResult::none()`（2026-10-14 报告的症状：Esc 什么都不做） | `input::esc_on_an_empty_input_cancels_a_running_turn`（`left: []`，应为 `[Cancel]`） | ✅ |

前两条是修复各自的配对变异，且都只由目标测试抓住（同一文件里另两条 note 测试保持绿色）。E1 没有
对应的修复（Esc 在 `2b55c50` 与今天都是 `cancel_or_quit_now()`，那条报告复现不出来），它证明这条回归
测试不是空转：把 Esc 的空输入分支改回「什么都不做」，它立刻失败。

**S1-M3（修一个自己造的回归）**：`start_tool` 最初带一次 `running_tools.iter().any(...)` 去重扫描，
在 20 000 个 fact 的洪水下是二次的。变异把它加回来（`if !...any(...) { push }`），
`cargo test -p radian-tui --test pty pty_a_flooding_producer_still_draws_and_answers_keys`
连跑 5 次：带扫描 **2 次失败**（load 166–178：一次 `the key was not handled`，一次
`the flood starved the draw: 92 frames in 2 s`），去掉扫描 **5 次全过**（load 194–251）。同一条 pty
测试也是先抓住这个回归的那一个：合入 `origin/main` 后的整套 `radian-tui` 测试里正是它红。
去重本身不是必需的：每个 call 的 `ToolStarted` 只有一次，而 `finish_tool` 的 `retain` 会删掉该 id 的
**每一条**，所以重复的 start 也留不下残渣。

## ask-box：transcript 的 Ask 块、面板标题、待答一行、标记与两个按键（ADR 0024，2026-10-15）

Base `origin/main` `6accfe0`（worktree `.radian-worktrees/ask-box`）。每个变异就地改完后跑
`cargo test -p radian-tui --lib -- <名字>` / `--test render` / `--test question`（`--no-fail-fast` 对
单条用例等价），再从 `.radian/scratch/*.fixed`、`*.p2`、`*.p3`、`*.p4` 的副本写回并 `cmp` 逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ASK1-M1 | `ask_body` 去掉每行末尾的右边框 | `the_ask_block_is_closed_at_every_width` | ✅ |
| ASK1-M2 | 底边改回 `repeat(width - 3)`（三格） | `the_ask_bottom_border_is_as_wide_as_the_top` | ✅ |
| ASK1-M3 | 面板标题 `origin` 非空就加（去掉 own-session 判断） | `the_ask_panel_title_omits_the_own_session` | ✅ |
| ASK2-M1 | 待答时仍画完整块（不走一行） | `a_pending_ask_is_one_line_and_becomes_the_block_once_answered` | ✅ |
| ASK2-M2 | 闭合的结果条目（`Tail`）也画块 | `a_pending_ask_is_one_line_and_becomes_the_block_once_answered`、`a_declined_ask_shows_the_status_inside_the_block` | ✅ |
| ASK2-M3 | 调用条目不再保留结果文本（状态行拿不到） | `a_declined_ask_shows_the_status_inside_the_block` | ✅ |
| ASK3-M1 | 面板标记用加粗的强调样式（不再去掉 BOLD） | `the_ask_panel_marker_is_never_bold` | ✅ |
| ASK3-M2 | `Other` 行改回画圆圈 | `the_ask_panel_uses_one_checkbox_pair_for_single_multi_and_other` | ✅ |
| ASK3-M3 | `single_on` 改回 `◉` | `the_ask_glyphs_use_one_checkbox_pair` | ✅ |
| ASK4-M1 | 去掉 ←/→ 的切题拦截（无字段时什么都不做） | `left_and_right_switch_questions_without_a_focused_field` | ✅ |
| ASK4-M2 | `form_select` 永远只做选中（空格不进字段） | `typing_in_the_other_row_and_a_text_question_keeps_the_text` | ✅ |
| ASK4-M3 | 底边提示永远写 `⇥/←/→ question` | `the_hint_row_shows_the_cursor_keys_while_a_field_has_focus` | ✅ |

12/12 抓住。ASK2 的两个变异在同一条用例里：待答一行、答完同一条目成块且只有一个块。ASK4-M1/M3 是
owner 反馈的两个按键问题的回归钉：←/→ 切题（无字段时）与底边提示跟随聚焦状态。`check-glyph-coverage.py`
的结果（☐/☑ 缺 SF Mono / JetBrains Mono Nerd / Cascadia Code）写在 ADR 0024 的「实施补充」里。

## markdown-③：标题不画 `#`、加粗不上色、代码块线框、表格表头与单元格行内样式（ADR 0035/6/7/8、ADR 0028；2026-10-16）

Base `origin/main` `28f4703`（worktree `.radian-worktrees/thin-md`，branch `oc/thin-md`）。每个变异就地改完
`blocks.rs` / `render.rs` 后跑 `cargo test -p radian-tui --locked --no-fail-fast --lib <名字>`
与 `--test markdown <名字>`，再从 `.radian/scratch/blocks.rs.orig`、`.radian/scratch/render.rs.orig`
的副本写回并用 `cmp` 确认逐字节相同（每个变异都打印 `RESTORED IDENTICAL`）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MD3-M1 | 标题又画回 `# ` 前缀（`md_blocks` 的 `Heading` 分支恢复合成前缀） | `blocks::tests::headings_have_no_hash_and_three_roles`、`markdown::headings_show_no_hash_and_level_attributes` | ✅ 两条都 FAILED |
| MD3-M2 | 加粗又借 `md_heading` 的颜色（`MdStyle::Bold` 回到 `theme.md_heading.patch(BOLD)`） | `markdown::bold_uses_the_text_role` | ✅ FAILED |
| MD3-M3 | 代码块又画围栏行（`code_frame_top` 顶线改回原文 ```lang） | `markdown::a_code_block_draws_a_frame`、`blocks::tests::a_code_block_draws_an_open_frame` | ✅ 两条都 FAILED |
| MD3-M4 | 表格单元格丢掉行内样式（单元格又用一段平文本拼平） | `markdown::table_header_and_body_cells_keep_their_roles` | ✅ FAILED（行内代码格不是 `md_code` 色） |
| MD3-M5 | 代码块的行数记账不含线框（`code_line_count` 的 `frame` 置 0） | `markdown::line_count_matches_the_layout` | ✅ FAILED |

5/5 抓住。MD3-M5 起初没有抓住：`line_count_matches_the_layout` 的正文以 `\n` 收尾，`has_blank_edge`
为真，`line_count` 于是走「整段排版后取行数」的退路，压根没碰 `code_line_count`。去掉正文末尾的换行
（并保留 `code_line_matches_the_layout` 里那块代码）后，该用例走逐块计数，变异即失败；这一处测试修正
作为第 5 个提交（`2dfc4e1`）落盘，不改前面的四个提交。
## thin ④：diff 正文的行号、符号、高亮（ADR 0028）

Base：本片 HEAD（`dogfood/thin-diff`）· 就地变异：每个变异先把文件复制到
`.radian/scratch/mut.<变异>.orig`，用 `python3` 打补丁（带 `assert`），跑命名的测试，再从副本写回并用
`cmp` 确认逐字节相同（`git status --short` 只剩本片改动）。脚本 `.radian/scratch/mutate.sh`（不提交）。
测试都带 `--no-fail-fast`。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| D1 | `+` 与上下文行用新文件行号、`-` 行用旧文件行号 | `DiffKind::Added` 改用 `old` 计数 | `diff::a_patch_body_numbers_its_rows_once_per_hunk`、`tools::tests::a_git_diff_body_numbers_from_its_own_hunk_headers` | ✅ |
| D2 | `@@` 与文件头是弱一级（`frame`），不是强调色 | `diff_role` 的结构行返回 `LineRole::Accent` | `diff::a_diff_row_colors_its_gutter_sign_and_code`、`render::a_git_diff_body_colors_its_line_kinds` | ✅ |
| D3 | diff 的代码按文件后缀高亮 | `diff_line` 的 highlight 恒为 `None`（`Some` 分支不可达） | `diff::a_diff_row_colors_its_gutter_sign_and_code`（`.rs` 的 `let` 是 `syntax_keyword`） | ✅ |
| D4 | `patch` 结构化结果的起始行 | `patch.rs::line_of_offset` 的 `+ 1` 改成 `+ 2` | `radian-coding tests/patch.rs` 的四条 hunk 测试（一/多区间、CRLF、首行与末行） | ✅ |

4/4 抓住。**过程记录（D2 的第一轮没抓住，是实现的问题）**：`diff_body` 里 `@@` 与文件头两处直接把角色
写成 `LineRole::Dim`，没有走 `diff_role`，所以变异 `diff_role` 对带 `@@` 的正文没有影响（只有"无起始行
退路"那一支会变）。两处改成调用 `diff_role`（结构行的角色只有一个定义处）后立刻抓住。

## thin-ask：问答记录改开口框、标记改圆圈（ADR 0028，2026-10-16）

Base `origin/main` `28f4703`（worktree `.radian-worktrees/thin-ask`）。每个变异就地改完跑
`cargo test -p radian-tui --lib --no-fail-fast -- <名字>` 与 `--test render ...`，再从
`.radian/scratch/blocks.rs.new` 写回并 `cmp -s` 逐字节相同。测试先改到新期望、对未改的实现跑过一遍，
12 条全 FAILED（4 lib + 7 render + 1 question），再动实现。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| ASK5-M1 | 开口框又封口（`ask_top` 的延伸段末尾加回 `╮`） | `blocks::tests::the_open_frame_rows_sit_in_the_marker_column`、`blocks::tests::the_ask_bottom_is_the_three_cell_tail` | ✅ |
| ASK5-M2 | 多选改回 `☐`/`☑`（`AskGlyphs` 的 `multi_on`/`multi_off`） | `blocks::tests::the_ask_glyphs_use_one_circle_pair`、`render::the_ask_panel_uses_one_circle_pair_for_single_multi_and_other` | ✅ |
| ASK5-M3 | 选项标记又加粗（`ask_body` 的 `{mark} ` 前缀改 `MdStyle::Accent`） | `render::the_transcript_ask_marker_is_never_bold` | ✅ |
| ASK5-M4 | 正文从第 2 列起（`ask_body` 的 `│ ` 去掉标记后的空格） | `blocks::tests::the_open_frame_rows_sit_in_the_marker_column`、`render::a_declined_ask_shows_the_status_inside_the_block` | ✅ |

4/4 抓住。面板的「标记不加粗」仍由既有的 `the_ask_panel_marker_is_never_bold` 钉住（ASK3-M1）；
本片把 `AskGlyphs` 的单选/多选/Other 从 `☐`/`☑` 改成 `○`/`◉` 后，这条用例找的格子也从 `☑` 改成 `◉`。
## thin ⑥：用户行、默认输入框、压缩线（ADR 0035/9、ADR 0028，2026-10-16）

Base：本片 HEAD（`oc/thin-user`）· 就地变异，每个变异后从 `.radian/scratch/render.rs.orig` /
`blocks.rs.orig` 还原并用 `cmp` 确认逐字节相同。`--no-fail-fast`。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| U6-M1 | 用户行仍带底色（`render.rs::body_style` 的 User 加回背景） | 用户行是前景角色、无底色 | `render::the_user_row_is_a_chevron_and_the_words_with_no_band`、`render::the_user_row_has_no_band_at_80_and_120`、`render::a_selection_draws_the_selection_role_on_its_cells`、`render::the_mono_selection_is_reversed_not_colored`（4 条 FAILED） |
| U6-M2 | 用户行又加回上下半格留白行 | 用户行只有文字，块间距由容器给 | `blocks::a_user_block_has_no_padding_rows_at_any_glyph_tier`、`blocks::a_blank_edge_line_is_stripped`、`blocks::md_line_count_matches_the_layout`、`render::the_user_row_is_a_chevron_and_the_words_with_no_band`、`render::the_block_layout_snapshot`、`render::thinking_has_one_blank_line_on_both_sides` 等（lib 5 + render 7 条 FAILED） |
| U6-M3 | 默认输入框回到 `bar`（`InputStyle` 的 `#[default]` 移回 Bar） | 默认是 `lines` | `config::the_input_style_is_applied_and_an_unknown_value_falls_back_to_lines`、`render::the_default_input_style_is_lines`、`render::the_user_row_has_no_band_at_80_and_120`（config 1 + render 2 条 FAILED） |
| U6-M4 | 压缩线又退回第 3 列（folded 行前加两格） | 压缩线从第 1 列起 | `render::the_compaction_line_starts_at_column_one_in_the_frame_role`、`render::the_block_layout_snapshot`（2 条 FAILED） |

4/4 抓住，无等价变异。行数一致性（`blocks.rs::line_count` 的 User 分支）由
`blocks::md_line_count_matches_the_layout` 与窗口/选择测试在改前改后都通过来保证。
`block_gap` 去掉了「用户块靠自己的 pad 分隔」的旧特例（thin 用户行没有 pad，改由容器的 1 行空白
分隔，与原型 `render-prototype.py` 一致），由 `blocks::the_container_gap_is_one_line_except_within_a_block`
与 `render::thinking_has_one_blank_line_on_both_sides` 钉住。`apply_selection` 顺带修正：选中段保留它
来自的 span 的前景色（原先最后一段回退到 `Style::default()`，会把选区尾部的前景色抹掉），由
`render::a_selection_draws_the_selection_role_on_its_cells` 的「文字保留自己的前景色」断言钉住。

## thin ①：工具块（ADR 0028，2026-10-16）

Base：本片 HEAD（`dogfood/thin-tools`）· 就地变异：脚本 `.radian/scratch/mutate.py`（不提交）先把
`render.rs`/`blocks.rs`/`tools.rs` 复制到 `.radian/scratch/mut-backup/`，用带 `assert` 的精确文本替换打补丁，
跑命名的测试（`--no-fail-fast`），再从副本写回并用 `cmp` 确认逐字节相同（脚本结尾对三个文件各 `cmp` 一次，
`git status --short` 只剩本片改动）。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| T1-M1 | head 行没有底色 | `render_head_row` 的 `• ` 标记加 `patch(theme.tool_success_bg)` | `card::every_tool_row_has_no_background_and_the_marker_column_holds`、`render::a_tool_block_never_paints_a_band`、`render::the_block_layout_snapshot` | ✅ |
| T1-M2 | 元信息在 head 行上，不单独成行 | `head_rows` 把 meta+状态另起一行 | `card::the_head_line_is_the_name_the_title_the_meta_and_the_status` 等（card 12 条全 FAILED） | ✅ |
| T1-M3 | 小工具成功时只有一行 | `body_plan` 去掉 `renderer.compact` 分支 | `card::a_small_tool_is_one_line_until_it_is_expanded` | ✅ |
| T1-M4 | 状态符号在行尾 | `styled_row` 把 tail（meta+状态）旋到行首 | `card::the_head_line_is_the_name_the_title_the_meta_and_the_status`、`card::every_tool_row_has_no_background_and_the_marker_column_holds`（card 11 条 FAILED） | ✅ |
| T1-M5 | `collapsed` 不画输出 | `body_plan` 的 collapsed 分支返回全部行 | `card::the_fold_modes_decide_the_body_and_the_collapsed_meta`、`render::collapsed_and_errors_differ_for_a_successful_card`、`render::the_tool_fold_changes_the_line_count_and_ctrl_o_walks_it` | ✅ |

5/5 抓住。行数记账（`line_count` 与 `layout`）由新增的
`card::line_count_matches_the_layout_for_a_tool_block`（三种折叠 × 80/24 列）与既有的
`markdown::line_count_matches_the_layout`、`tests/virtual.rs` 的窗口测试共同钉住。

**「测试先于实现」的核对**：把 `origin/main` 的 `crates/radian-tui/src/{tools,blocks,render,app}.rs`
拷回来（本片实现先另存，改完 `cmp` 逐字节还原）再跑 `cargo test -p radian-tui --test card`：
**11 条 FAILED / 12**。唯一通过的是 `line_count_matches_the_layout_for_a_tool_block`，它断言的是
改前改后都成立的不变量（`line_count` == `layout` 的行数），是本片改动引入的计数镜像的回归钉，
不声称新行为。

**一处要记录的缺口（review fix 1 已关闭，保留原文）**：状态符号 `✗`（U+2717）不在 ADR 0027 的
六款参考字体核对集里（那张表用 `×`，`✓` 在表内）。ADR 0028 与原型 `render-screen.py` 都
写 `✗`，所以 transcript 当时用了它；review 提给 owner 后，owner 在 ADR 0035 的实施补充
（2026-10-16，`5e8eccb`）里定：工具失败用 `×`，thinking 用 `▼`/`▶`——见下一节。

## thin ① review fix 1：字形＼�ADR 0035 实施补充）

Base：本片 HEAD（`dogfood/thin-tools`）· 就地变异，脚本仍是 `.radian/scratch/mutate.py`（每个变异前把
`render.rs`/`blocks.rs`/`tools.rs`/`footer.rs` 复制到 `.radian/scratch/mut-backup/`，跑命名的测试，再从
副本写回并用 `cmp` 逐字节确认；`git status --short` 只剩本片改动）。`--no-fail-fast`。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| M6 | 工具失败的符号是 `×`，不是 `✗` | `status_marks` 的 unicode 档改回 `('✓', '✗')` | `card::a_failed_call_ends_the_head_with_a_cross_and_shows_the_error_body`、`card::a_small_tool_is_one_line_until_it_is_expanded`（read 出错那条也以失败符号结尾）、`render::the_block_layout_snapshot` | ✅ |
| M7 | 折叠的 thinking 标记是 `▶`（unicode/nerd），不是 `›` | `Glyphs::fold_marker` 改回 `Unicode => "›"`、`Nerd => ">"` | `thinking::the_folded_thinking_is_one_line_with_the_line_count`、`render::thinking_folds_to_one_line_and_ctrl_t_expands_it` | ✅ |
| M8 | 展开的 thinking 标记是 `▼`，不是 `▾` | `REASONING_MARKER` 改回 `"▾ "` | `thinking::the_expanded_thinking_has_a_header_and_a_rail`、`thinking::the_duration_is_shown_for_a_live_entry_and_absent_for_a_resumed_one`、`thinking::streaming_shows_the_spinner_not_the_duration`（render 的整屏快照那条**没抓住**：它里面的 thinking 是折叠的，走的是 M7 的标记） | ✅ |

3/3 抓住，无等价变异。同一次复跑里 slice ① 的 T1-M1..M5 仍然全部抓住（字形改了不影响它们的判据）。

**方法记录（踩到过，值得记）**：还原变异时用 `shutil.copy2` 会把**副本的旧 mtime** 一起写回，于是还原后的源文件看起来比 cargo 用变异版编译出的产物更旧——cargo 判定「新鲜」，**变异留在构建缓存里**，
后续的测试跑的是变异代码（这一次的表现是 `card::the_fold_modes…` 与两条 render 折叠用例在源码与 HEAD
逐字节相同的情况下失败）。`mutate.py` 现在用 `copyfile` + `os.utime(dst, None)` 落回当前时间，并且每次
变异结束都重跑一遍整包测试确认回到绿；也可以 `touch` 相关源文件或 `cargo clean -p radian-tui` 来强制重建。

覆盖检查的三处同步：`scripts/check-glyph-coverage.py` 的 `GLYPHS`、`footer.rs` 的
`unicode_tier_glyphs_are_all_verified_covered` 的 `VERIFIED_COVERED`、以及 README 里那张脚本输出的表。
`▼ ▶ • ╭ ╰` 加入，`›` 移出（没有任何一行再画它）；`× ✓ │` 本来就在表里。用 venv 里的 `fonttools 4.67.0`
跑脚本核对：六款字体（本机都装了）对 `× • │ ╭ ╰ ▼ ▶ ✓` 全是 ✅，与 owner 的结论一致。
`◉`/`○`（问答标记）**没有**加进任何一个清单——owner 明知它们缺字并接受，README 的「kept」一段记下了这一点。

## Theme format：TOML 主题、一次性 pi 导入、过渡期（ADR 0029/3/4，2026-10-16）

Base：本片 HEAD（`oc/theme-format`；决定 1 = `cff5d35`，决定 4 = `e9f29ef`，决定 3 = 本提交）·
就地变异，每个变异前把 `file.rs`/`importer.rs`/`toml_file.rs` 复制到 `.radian/scratch/mut-bak/`，
变异后跑对应测试、从副本还原并用 `cmp` 逐字节确认（脚本 `.radian/scratch/mutate.sh`）。测试都
`--no-fail-fast`，只跑命中的那条（`cargo test -p radian-tui [--lib|--test …] <name>`）。

| # | 需求 | 变异 | 抓住它的测试 |
|---|---|---|---|
| TH-M1 | importer 把 pi 键映射到**正确的** Radian 角色 | `file.rs` 的 `fg_source` 把 `Role::User => Some("userMessageText")` 改成 `Some("toolTitle")` | `theme::importer::tests::every_fixture_converts_to_its_committed_golden`（golden 里 `user = "#dcd7ba"` 变成 `tool_call` 的 `#938aa9`） |
| TH-M2 | `<name>.toml` 优先于 `<name>.json` | `file.rs` 的 `load_file` 把 `.toml` 分支的判据改成 `if false && toml_path.is_file()`（等于永远先读 `.json`） | `theme::file::tests::a_toml_theme_wins_over_a_json_one_and_the_json_fallback_hints`（「the .toml must win」失败） |
| TH-M3a | 覆盖已有 `<name>.toml` 必须 `--force` | `importer.rs` 的 `write` 把 `if force {` 改成 `if true {`（永远走覆盖分支，不再 `create_new`） | `theme::importer::tests::write_refuses_an_existing_file_without_force` |
| TH-M3b | 同上，走 `radian theme import` 的整条路 | 同 TH-M3a | `theme_import::import_refuses_to_overwrite_without_force_and_force_wins`（第二次导入退出码 0，期望 1） |
| TH-M4 | 变量循环只让相关角色退回内建 | `toml_file.rs` 的 `Variables::resolve_var` 把循环的 `return Err(VarError::Cycle(..))` 改成 `return Ok(ColorValue::Default)`（循环不再报错、角色拿到终端默认色） | `theme::toml_file::tests::a_cycle_only_breaks_its_own_roles`（工具行的颜色与警告条数都不符） |

5 条运行全部抓住（TH-M3 的变异同时打中 a、b 两条测试），无等价变异。

**诚实补充（TH-M1 的边界）**：round-trip 测试
`an_imported_fixture_round_trips_to_the_json_loader_role_by_role` **抓不住**映射错位——它比较的是
JSON loader 与 TOML loader，而两者共用 `fg_source`/`bg_source` 同一张表，映射一起错、两者照样相等。
钉住映射的是金标准（golden）那条：它是「转换器的输出」而不是「两个 loader 互相一致」。这正是决定 3
把映射放进 importer、又把转换结果入库为 golden 的原因。
## thin ②：thinking 的头行、时长、单色 Markdown 正文、折叠行（ADR 0028，2026-10-16）

Base：本片 HEAD（`oc/thin-think`）· 就地变异，每个变异先把文件复制到 `.radian/scratch/mut.orig`，
用 `python3` 打补丁（带 `assert old in s`），跑命名的测试，再从副本写回并用 `cmp` 确认逐字节相同
（脚本 `.radian/scratch/mutate-think.sh`，不提交）。`--no-fail-fast`。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| TH1 | 正文不斜体 | `render.rs::body_style` 的 Reasoning 加回 `ITALIC` | `thinking::the_thinking_body_is_single_colour_markdown` | ✅ |
| TH2 | 正文按 Markdown 排版 | `blocks.rs::reasoning_body_lines` 的 Paragraph 分支改回 `prose_lines`（`**`/反引号露出） | 同上（断言屏幕无 `**`、无反引号） | ✅ |
| TH3 | 正文只有一个 reasoning 颜色 | `render.rs::body_spans` 去掉 reasoning 分支（走 Assistant 的 MdStyle 映射） | `thinking::every_thinking_body_cell_is_the_reasoning_colour` | ✅ |
| TH4 | 折叠行带行数 | `blocks.rs::placeholder` 的 resumed 分支改成只写 `thinking` | `thinking::the_folded_thinking_is_one_line_with_the_line_count` | ✅ |
| TH5 | resume 的条目没有时长 | `app.rs::push_message` 无条件给提交的 reasoning 条目写 `self.now` / `self.now` | `thinking::the_duration_is_shown_for_a_live_entry_and_absent_for_a_resumed_one` | ✅ |

5/5 抓住，无等价变异。

**诚实补充（TH1 的第一轮没抓住）**：第一次的变异改的是 `reasoning_md_style` 的兜底分支
（`_ => base.add_modifier(ITALIC)`），只让 code/link 那些 piece 斜体，而测试断言的 `bold` 词走
`Bold` 分支，所以测试仍绿；改成变异 `body_style`（整个 body 的基样式）后立刻抓住，因为 `**bold**`
的基色也变斜体。这说明「正文仍斜体」这条需求针对的是**整个正文的基样式**，不是某一种 piece。

另外，`blocks.rs::strip_edge_blanks` 的空白判据从 `chars.is_empty()` 改成 `text.is_empty()`：新增的
`▾ thinking · …` 头行是合成行（没有源字符）但**画了文字**，旧判据把它当空白边缘行删掉，整个头行消失。
只有这一种形状受影响，由 `thinking::the_expanded_thinking_has_a_header_and_a_rail` 与
`blocks::reasoning_modes_change_the_line_count` 钉住。

## thin ② × slice ③：thinking 正文的代码块用 slice ③ 的 open frame（2026-10-16）

切片 ③ 把围栏代码块改成 open frame（ADR 0035）后，thinking 正文的代码块跟随同一形状，
由新增测试 `thinking::a_thinking_code_block_draws_the_open_frame_and_counts_the_drawn_lines` 钉住。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| TH6 | thinking 正文的代码块画 open frame | `blocks.rs::reasoning_body_lines` 去掉 `code_frame_top(...)` 的 push | `thinking::a_thinking_code_block_draws_the_open_frame_and_counts_the_drawn_lines`（屏幕没有 `╭─ rust `） | ✅ |
| TH7 | 计数的行数等于画出的行数 | `blocks.rs::line_count` 的 Reasoning 分支改成 `code_line_count(&text[start..end], closed) - 1` | 同上（`left: 5, right: 6`） | ✅ |

2/2 抓住。测试正文**不带**结尾换行，所以 `line_count` 走镜像分支而不是 `has_blank_edge` 的 layout
兜底（第一版带了结尾换行，TH7 没抓住：兜底直接返回 layout 的长度，绕过镜像）。

## 空回合：一条只有 reasoning/tool call 的回复不再留两行空白（ADR 0028 规则 1，ADR 0035，2026-10-16）

Base：本片 HEAD（`dogfood/empty-turn`，父提交 `3d44139`）· 就地变异：先把 `crates/radian-tui/src/blocks.rs`
用 `cp` 存到 `.radian/scratch/mut/blocks.rs.orig`，用 `python3` 做带 `assert` 的精确文本替换，跑命名的测试
（`--no-fail-fast`），再用 `cp` 从副本写回并 `cmp` 逐字节确认相同（脚本内联，不提交；`git status --short`
只剩本片改动）。

**先核对的事实**：空（或纯空白）的已提交 assistant 条目在当前代码上**已经**排成 0 行（`strip_edge_blanks`
删掉了那段无背景空白边缘行；探针在 `blocks.rs` 里临时加了一条 `eprintln!`，看完即删：`layout("") = 0`、
`layout(" ") = 0`、`layout("  \n ") = 0`）。所以本片的实现只改**间隙**：`is_empty_assistant` + `gap_at`。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| EM-M1 | 已提交的空回复被间隙跳过（零行保持不变，间隙不再多一行） | `blocks.rs::is_empty_assistant` 恒返回 `false`（等于回到旧几何，零行仍在） | `blocks::tests::an_empty_committed_assistant_is_skipped_by_the_gaps`、`render::a_text_less_assistant_reply_leaves_one_blank_row`、`render::a_skipped_reply_does_not_shift_the_selection_or_find_rows`、`render::thinking_has_one_blank_line_on_both_sides`（改成真实流程后的下半段）、`render::collapsed_and_errors_differ_for_a_successful_card`、`selection::a_selection_over_a_tool_head_copies_its_text` | ✅ |
| EM-M2 | **live** 的空回复**不**被跳过 | `blocks.rs::is_empty_assistant` 去掉 `!entry.live` 守卫（live 也被跳过） | `blocks::tests::an_empty_committed_assistant_is_skipped_by_the_gaps`、`render::a_live_empty_assistant_keeps_its_blank_line` | ✅ |

2/2 抓住，无等价变异。EM-M1 下上述 6 条一起 FAILED（`--no-fail-fast` 全跑：
`blocks.rs` 那条在 `assert_eq!(gap_at(&entries, 2), 0)`，`render.rs` 各条在「一行空白」的绝对行号上）。
EM-M2 下 `a_live_empty_assistant_keeps_its_blank_line` 在 `gap_at(...) == 1` 上 FAILED。

**旧代码上先跑过的核对**：本片新增/改写的断言在旧实现（EM-M1 的语义）上失败；只有
`a_live_empty_assistant_keeps_its_blank_line` 按设计在旧代码上**通过**（它钉的就是「live 不变」的今天行为）。
两条既有测试（`render::collapsed_and_errors_differ_for_a_successful_card` 的 `[22,7,2]`、
`selection::a_selection_over_a_tool_head_copies_its_text` 的第 1 行 head）承载的是旧几何，本片随实现一起
更新到 `[21,6,1]` / 第 0 行——它们各自也被 EM-M1 抓住，说明改动后仍有效。
## thin ④ × `write`：`write` 正文按行编号的 diff（ADR 0028，2026-10-16）

Base：本片 HEAD（`oc/write-diff`）· 就地变异（改 `tools.rs`），每个变异前把文件复制到
`.radian/scratch/mut/tools.rs.orig`，跑完用**不带 `-p`** 的 `cp` 还原（mtime 落回当前）并 `cmp`
逐字节确认；结束 `git status --short` 只剩本片自己的改动。每个变异用
`cargo test -p radian-tui [--lib|--test card|--test diff] --locked --no-fail-fast -- <test>`。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| W4 | 行号从 1 起 | `write_body` 的 `number: Some(index as u64 + 1)` → `Some(index as u64)` | `tools::tests::a_write_body_numbers_every_content_line_as_an_addition`、`card::a_new_file_write_numbers_each_content_line`、`diff::a_write_body_numbers_the_content_and_copies_it_without_the_gutter` | ✅ |
| W5 | 覆盖时头部写 `+M −N` | `diff_meta` 的 `write` 分支丢掉 `previous_lines`（`Some(removed) => …` → `Some(_) => "+{added}"`） | `card::an_overwrite_write_head_shows_the_removed_count` | ✅ |
| W6 | `write` 行的符号是合成的（正文就是 content，无符号列） | `write_body` 两处 `signed: false` → `true`（首字符被当成 diff 符号吃掉） | `card::a_new_file_write_numbers_each_content_line`、`diff::a_write_body_numbers_the_content_and_copies_it_without_the_gutter` | ✅ |

3/3 抓住。W6 复现本片之前的形状（`write` 的正文直接就是 content、没有符号与行号）：变异后
`diff_line` 把每个内容行的首字符当作 diff 符号，行变成 `1 o ne`，编号与复制都不对。W4/W6 在旧代码上
必失败——旧 `write` 的 renderer body 是 `plain_body`，`write_body` 当时并不存在，没有任何行号。
## Slice ⑦: thin snapshots, the hand-test list and the mutation list (2026-10-16)

Base `1d58517` (the slice's own tests/README) · mutations applied in place by
`.radian/scratch/mutate.py`, which backs each file up under `.radian/scratch/mut/`, asserts the
anchor is present, runs the named test with `--exact` and `timeout`, then restores with a plain
`cp` and `cmp`s the file byte-for-byte. `git status --short` empty after every mutation; none
restored to a different byte.

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| SL7-1 | the head row is a rail, not a card | `render.rs::render_head_row` returns the head `Line` with a `bg` | `thin::no_transcript_row_has_a_background_in_any_builtin_theme` | ✅ |
| SL7-2 | the meta stays on the head line | `blocks.rs::head_rows` pushes the meta as its own `BodyLine` instead of the tail | `thin::the_shell_blocks_snapshot_at_80_120_dark_light` (the golden rows shift) | ✅ |
| SL7-3 | a `+` row carries the **new** file's number | `tools.rs::diff_body`'s `DiffKind::Added` arm numbers from `old` | `thin::a_three_hunk_diff_numbers_each_row_by_its_own_file` | ✅ |
| SL7-4 | `@@`/file headers are the weak `frame` role | `tools.rs::diff_role` returns `LineRole::Accent` | `diff::a_diff_row_colors_its_gutter_sign_and_code` (`@@` == the accent) | ✅ |
| SL7-5 | the thinking body is Markdown (no `**`/backticks) | `blocks.rs::reasoning_body_lines`' Paragraph branch back to `prose_lines` | `thinking::the_thinking_body_is_single_colour_markdown` | ✅ |
| SL7-6 | a table cell keeps its inline style | `blocks.rs::md_table` flattens the body cell's pieces to `MdStyle::Plain` | `markdown::table_header_and_body_cells_keep_their_roles` | ✅ |
| SL7-7 | the ask record is an **open** frame | `blocks.rs::ask_top`/`ask_bottom` add a `╮`/`╯` corner | `thin::the_ask_record_snapshot_is_an_open_frame` | ✅ |
| SL7-8 | a folded shell block draws no output | `tools.rs::body_plan` drops `|| fold == ToolFold::Collapsed` from the compact branch | `card::the_fold_modes_decide_the_body_and_the_collapsed_meta` | ✅ |

8/8 caught, no misses and no equivalent mutants. Each mutation is a real behaviour change (the
script asserts its anchor, so a `cargo fmt` shift or a no-op patch fails loudly rather than
passing as "caught" — the U3/U4 trap); the restoring `cmp` is the guard that nothing leaked.

## empty-turn review fix 1：被中断的空回复保留 `[interrupted]` 标记（ADR 0003，2026-10-16）

Base：`ed858d8`（评审已接受的 head）· 就地变异：先把 `crates/radian-tui/src/blocks.rs` 用 `cp` 存到
`.radian/scratch/mut/blocks.rs.fix1orig`，用 `python3` 做带 `assert` 的精确文本替换，跑命名的测试
（`--no-fail-fast`），再 `cp` 写回并 `cmp` 逐字节确认。`--no-fail-fast`。

事实核对：`[interrupted]` 标记画在条目的**最后一行** body 上（`render.rs` 的 `entry_lines` /
`visible_lines`）。空文本的已提交 assistant 条目 body 本来就是 0 行，所以即使在 skip 之前，这个标记也
画不出来（没有「最后一行」）。因此修复不能只是「不跳过」：`is_empty_assistant` 排除 interrupted 之外，
`layout_with` 还要给这种条目留一行（否则它仍然是 0 行、标记无处可画）。

| # | 需求 | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|---|
| FX1-M1 | 被中断的空回复不被跳过（保留它的空白行） | `blocks.rs::is_empty_assistant` 去掉 `!entry.interrupted` | `render::an_interrupted_reply_with_no_text_still_shows_the_marker`（标记行的行号 `left: 4, right: 5`）、`blocks::tests::an_empty_committed_assistant_is_skipped_by_the_gaps`（`!is_empty_assistant(&interrupted)`） | ✅ |
| FX1-M2 | 被中断的空回复留一行给标记 | `blocks.rs::layout_with` 去掉那段 `lines.push(BodyLine::prose(...))`（保留 interrupted 条件） | `render::an_interrupted_reply_with_no_text_still_shows_the_marker`（屏幕没有 `[interrupted]`）、`blocks::tests::an_empty_committed_assistant_is_skipped_by_the_gaps`（`line_count == 1`） | ✅ |

2/2 抓住。新测试在 `ed858d8`（未修复）上先跑过：`contains("[interrupted]")` FAILED。
## ADR 0030 片 6b2：TUI 的任务视图（2026-10-16）

Base `2f41f69` · 直接改源码、跑单个测试、从 `.radian/scratch/mut/` 副本 `cp` 还原，
`git status --short` 跑完为空。

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| S6B2-1 | 宿主不再传 builder，lead tools 不挂载 | `cli.rs` 的 `task_builder: Some(task_builder)` 改回 `None` | `pty_the_tui_mounts_the_task_tools` | ✅ |
| S6B2-2 | 没有 task 时不显示计数 | `footer.rs` 的 `tasks` item 从 `task_counts().map(..)` 改成恒 `Some(0/0)` | `task_events_become_notices_and_move_the_count`（无 task 时屏幕出现 `tasks`） | ✅ |
| S6B2-3 | `/tasks cancel` 要真的 cancel | `command.rs` 的 `cancel` 分支返回 `TasksRequest::List` | `tasks_parses_list_cancel_and_kill`、`a_submitted_tasks_line_is_the_loop_effect` | ✅ |

3/3 抓住。S6B2-1 的 pty 测试会重编 `radian` 二进制并检查第一次模型请求的 tools 含全部
`LEAD_TOOL_NAMES`。
## pulse-slow: the breath's period 1.4 s -> 2.0 s and 12 -> 16 samples

Worktree `.radian-worktrees/pulse-slow`, branch `oc/pulse-slow`. In-place mutation: `cp` the file
to `.radian/scratch/spinner.rs.fixed` first, then a `python3` patch replaces the const line (with
an `assert` that the anchor occurs once and `assert old in s`, so a no-op patch fails loudly), the
suite runs `cargo test -p radian-tui --lib --locked --no-fail-fast`, and finally the file is
restored with a plain `cp` (no `-p`, so the mtime is "now" and cargo rebuilds) and `cmp` reports it
byte-identical (`git status --short` empty).

| # | Mutation | Caught by | Result |
|---|---|---|---|
| PS-M1 | `PULSE_PERIOD` 2_000 -> 1_400 (the period back to the old value, samples left at 16) | `spinner::tests::the_breath_walks_one_sine_period_in_sixteen_steps` (the pinned `2_000 ms`), `spinner::tests::a_pulse_that_cannot_interpolate_keeps_the_two_colour_switch` (one 87.5 ms step is outside the `100..130` ms assertion), `cli::tests::the_breath_redraws_once_per_step_and_only_when_a_spinner_is_visible` (the two-colour switch counts 6 frames over the now 2.8 s window, not the asserted 8) | ✅ (3 FAILED) |

1/1 caught. The three different tests fail for three different reasons — the pinned constant, the
step length, and the frame count over two periods — so the slower period is not pinned by a single
assertion. After the plain-`cp` restore the whole lib suite is green again (272 passed).
## current-mark: the current menu row carries ` ✓` after its name

Base `01e7b40` (the code and its tests; the docs follow in `c6c0ea0`). In-place mutations: each
file backed up under `.radian/scratch/`, the mutated source restored with a plain `cp` and
`cmp`ed byte-for-byte, `git status --short` empty after each one.

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| CMK-1 | the description never carries the marker | `command.rs::thinking_candidates` prefixes the current level's description with `● current · ` and clears `current` | `render::the_theme_model_and_thinking_menus_mark_the_current_row` (no row may contain `current`) | ✅ |
| CMK-2 | the ` ✓` must not push the description column | `select_list.rs::row_line` drops `used +=` around the `draw_marker` call (the mark is drawn but not counted before the padding) | `select_list::tests::the_current_item_is_marked_after_its_label` (the description columns differ) | ✅ |
| CMK-3 | a truncated label keeps its ` ✓` | `row_line` truncates the label without reserving the mark (`used + marker_width + 1` → `used + 1`) and passes `label_column` to `draw_marker` | `select_list::tests::a_long_current_label_is_truncated_before_its_mark` (the mark is gone) | ✅ |

3/3 caught. CMK-1 also fails `command::thinking_candidates_name_the_wire_value_and_mark_the_current_level`;
CMK-2 and CMK-3 fail only their `select_list` test. The mark is new behaviour, so these are its
first mutations.

## fold-mark: one fold marker for thinking and tool heads (ADR 0035's 2026-10-16 note)

Base `2646c17` (the code and its tests; this record follows). In-place mutations: each touched
source backed up under `.radian/scratch/backup/`, the mutated file restored with a plain `cp` and
`cmp`ed byte-for-byte (`git status --short` empty after each one). Each mutation ran
`cargo test -p radian-tui --test <file> --no-fail-fast`.

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| FM-1 | a one-line small tool keeps `•`, never folds | `blocks.rs::head_marker`: `hidden > 1` -> `hidden > 0`, so a `read` whose hidden body is a single line folds | `card::the_tool_head_fold_marker_follows_the_drawn_body`, `card::a_small_tool_is_one_line_until_it_is_expanded`, `card::a_cancelled_call_says_cancelled_in_its_meta_and_draws_no_status_glyph` | ✅ (3 FAILED) |
| FM-2 | a drawn body makes the head `◻`, never `◼` | `blocks.rs::head_marker`: the `body_follows` branch returns `Folded` | `card::the_head_line_is_the_name_the_title_the_meta_and_the_status`, `card::a_failed_call_ends_the_head_with_a_cross_and_shows_the_error_body`, `card::a_cjk_command_and_output_keep_the_marker_column`, `card::the_ascii_glyph_set_draws_ascii_markers_and_status`, `card::a_small_tool_is_one_line_until_it_is_expanded`, `card::the_tool_head_fold_marker_follows_the_drawn_body`, `card::the_fold_modes_decide_the_body_and_the_collapsed_meta`, `card::a_patch_block_shows_its_diff_body`, `card::an_overwrite_write_head_shows_the_removed_count`, `card::a_new_file_write_numbers_each_content_line` | ✅ (10 FAILED) |
| FM-3 | the ascii one-liner is `* `, not the expanded `- ` | `blocks.rs::HeadMarker::glyph`: `Ascii => "* "` -> `"- "` | `card::the_tool_head_fold_marker_follows_the_drawn_body` | ✅ (1 FAILED) |
| FM-4 | the expanded thinking header is `◻` (the shared pair), not `▼` | `render.rs`: `REASONING_MARKER` back to `"▼ "` and `reasoning_marker` back to `v`/`▼ ` | `thinking::the_thinking_fold_marker_is_the_shared_pair`, `thinking::the_expanded_thinking_has_a_header_and_a_rail`, `thinking::the_duration_is_shown_for_a_live_entry_and_absent_for_a_resumed_one`, `thinking::streaming_shows_the_spinner_not_the_duration`, `thinking::the_thinking_markers_follow_the_ascii_glyph_tier` | ✅ (5 FAILED) |
| FM-5 | a compact tool's head is a `•` whatever its result hides (reviewer fix 1) | `blocks.rs::head_marker`: the compact early-return removed and the old `hidden > 1` fold restored, so a `read` hiding 100 lines folds to `◼` | `card::the_tool_head_fold_marker_follows_the_drawn_body` (the 100-line read row: `◼ read a.rs ✓`), `render::the_block_layout_snapshot` (the `list` head) | ✅ (2 FAILED) |

5/5 caught, no misses and no equivalent mutants. FM-3 is pinned by exactly one test (the ascii row
of the table); FM-1's one-line-fold mutant also trips the cancelled-call head, which shares the
`hidden > 0` branch. FM-5 (reviewer fix 1, base `c2b4abd`) is the one mutation that the original
suite missed: `a_small_tool_is_one_line_until_it_is_expanded` uses a **one-line** `read`, so the
old `hidden > 1` clause was never exercised — the new test uses a 100-line result to pin it.
Files restored with `cp` + `cmp` after the run.

## ask-keys: `Space` selects, `Enter` only advances (ADR 0024 note)

Base `1442672` (the implementation commit; the run also carries the new
`the_other_cursor_follows_the_text_cursor` test). In-place mutations: each touched source backed
up under `.radian/scratch/mut/`, the mutated file restored with a plain `cp` and `cmp`ed
byte-for-byte (`git status --short` clean apart from the new test). Each mutation ran
`cargo test -p radian-tui --test <file> --no-fail-fast` (or `--lib` for the composer).

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| AK-1 | `Enter` never selects | `question.rs::FormSubmit` selects-and-advances a `single` again | `question::enter_advances_without_selecting_and_space_selects_without_advancing` | ✅ (1 FAILED) |
| AK-2 | `Space` on `Other` shows the text cursor | `render.rs::other_line`: `answer.other && focused` -> `false` (no inline `▏`) | `question::space_on_other_starts_typing_and_the_cursor_is_visible` | ✅ (1 FAILED) |
| AK-3 | a chosen `Other` field takes the space as text | `question.rs::field_takes_space`: the `Other` clause -> `false` | `question::space_on_other_starts_typing_and_the_cursor_is_visible` | ✅ (1 FAILED) |
| AK-4 | `Enter` on the last question goes to `Submit`, it does not submit directly | `question.rs::FormSubmit`: the last question returns `self.submit()` | `question::{enter_walks_to_the_overview_and_submits, an_unanswered_question_is_marked_in_the_overview, a_required_question_blocks_the_submit, drafts_are_kept_between_questions, other_keeps_its_text_while_moving_with_arrows}` | ✅ (5 FAILED) |
| AK-5 | `Enter` never toggles a `multi` | `question.rs::FormSubmit`: a `multi` toggles again | `question::a_multi_question_toggles_with_space_and_enter_only_advances` | ✅ (1 FAILED) |
| AK-6 | the overview marks an unanswered question | `render.rs::question_panel` Submit tab: `mark` always `"✓"` | `question::an_unanswered_question_is_marked_in_the_overview` | ✅ (1 FAILED) |
| AK-7 | choosing `Other` puts the composer cursor at the end | `composer.rs::to_end` body -> a no-op | `composer::tests::to_end_moves_the_cursor_to_the_buffer_end_from_the_middle` | ✅ (1 FAILED) |
| AK-8 | the inline `Other` cursor follows the composer cursor | `render.rs::other_line`: `answer.text.cursor()` -> `text.chars().count()` (always the end) | `question::the_other_cursor_follows_the_text_cursor` | ✅ (1 FAILED) |

8/8 caught, no misses and no equivalent mutants. AK-2/AK-3 cover the two halves of "`Space` on
`Other` focuses it"; AK-7/AK-8 cover the new `Composer::to_end` and the inline cursor it drives.
**Process note (AK-7):** the first run used `--lib to_end_moves_... -- --exact`, which matched 0
tests and looked like a pass (the U3/U4 trap); the full path
`composer::tests::to_end_moves_...` catches it.
## multiline-cmd: a multi-line command folds with its block (ADR 0035's 2026-10-16 note, option A)

Base `d151bd7` (the code and its tests; this record follows). In-place mutations: the source
backed up under `.radian/scratch/mut/`, the patched file restored with a plain `cp` and `cmp`ed
byte-for-byte (`git status --short` empty after each one). Each mutation ran
`timeout 600 cargo test -p radian-tui --locked --test card --no-fail-fast <test>`.

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| ML-1 | the fold marker is on the first head row only | `blocks.rs::styled_head_rows` / `styled_body_rows`: the command's continuation rows become `Frame::Head`, so `render_head_row` draws the marker on each | `card::a_multiline_command_folds_with_its_block` (the expanded rows read `◻ print(1)`, not `│ print(1)`) | ✅ (1 FAILED) |
| ML-2 | a folded block hides the command's other lines | `blocks.rs::call_body_expanded` returns `true` unconditionally, so `collapsed` draws the 39 lines too | `card::a_multiline_command_folds_with_its_block` (the folded head gains 39 `│ ` rows) | ✅ (1 FAILED) |
| ML-3 | the meta stays on the first row | `head_rows` passes no tail to `styled_head_rows` and appends it to the last row instead | `card::a_multiline_command_folds_with_its_block` (the meta lands on `│ PY · exit 0 ✓`, the head row loses it) | ✅ (1 FAILED) |
| ML-4 | N counts the command's remaining lines, never the output's | `head_rows` passes `card.result_lines` to `head_tail` instead of `last` | `card::a_multiline_command_folds_with_its_block` (the note reads `… (+2 lines)` for a 40-line command) | ✅ (1 FAILED) |
| ML-5 | a result row's selection offsets are the entry's own | `blocks.rs::body_plan_lines`: the lift by `source.start` removed (the pre-existing relative-offset bug) | `card::a_copy_of_a_multiline_command_has_no_marker_rail_or_note` (the copy loses the output's second line) | ✅ (1 FAILED) |

5/5 caught, no misses and no equivalent mutants. ML-1..ML-4 are the four behaviours the task
names, each pinned by the one three-state test; ML-5 is the pre-existing offset bug the copy
requirement depends on, pinned by the copy test (the same mutation leaves
`a_copy_of_a_block_has_no_marker_meta_or_status` green, because its single-line result starts at
offset 0).

## input-box: drop the `bar` input, `box` becomes the thin half frame (ADR 0035's 2026-10-16 note)

Base: the `oc/input-box` worktree; the source backed up under `.radian/scratch/mut/`
(`render.rs.orig`, `app.rs.orig`), each mutation patched in place with a Python `assert`-guarded
text replace (a stale anchor is a hard PATCH FAILED), restored with a plain `cp` and `cmp`ed
byte-for-byte (`git status --short` back to the same set after each). The one test named per row
ran with `timeout 300 cargo test -p radian-tui --no-fail-fast`.

| # | Requirement | Mutation | Caught by | Result |
|---|---|---|---|---|
| IB-M1 | the running status is **not bold** | `render.rs::input_status_spans`: the pulse keeps `spinner_style`'s BOLD (drop the `remove_modifier(Modifier::BOLD)`) | `render::the_box_frame_puts_the_status_in_the_label_position_and_never_bold` (the spinner cell is `.bold()`) | ✅ (1 FAILED) |
| IB-M2 | the idle top border carries **no label** | `render.rs` idle top rule draws `╭─ message ─…` again | `render::the_box_input_style_draws_the_thin_half_frame` (`!text.contains("message")` and the exact full-width rule) | ✅ (1 FAILED) |
| IB-M3 | the removed `bar` warns and falls back to `lines` | `app.rs::apply_config`: `Some("bar") => InputStyle::default()` (no warning pushed) | `config::the_input_style_is_applied_and_an_unknown_value_falls_back_to_lines` (warnings empty) | ✅ (1 FAILED) |

3/3 caught. IB-M2's test also pins the full shape (the `│ ❯ `/`│   ` rows and the `╰──` tail),
so a mutation of the marker column or the tail fails it too; the CJK wrap and the cursor column
are pinned by `render::the_box_frame_wraps_a_cjk_line` and `cursor::the_box_cursor_counts_the_prompts_display_width`.
The `input_style = "bar"` reader test (`config::the_tui_input_style_is_carried_and_a_non_string_is_skipped`)
covers the host side (the spelling is carried, not skipped).

**Piece 2 dropped.** The task's fourth mutation ("the configured separator ignored with a preset")
belonged to the `[tui.footer]` separator bug, which the owner dropped ("the owner is redesigning the
footer") before any code was written; no commit or mutation for it exists.

## ADR 0042: 渲染器换成 grok-build 的 `mermaid.rs`（2026-10-16）

Base `aedc23f`。变异在 worktree 内直接改，改前把每个文件复制到 `.radian/scratch/mut/`，改后
`cp` 还原并 `cmp` 一致（`git status --short` 变异后只剩被测文件，跑完为空）。命令：
`timeout 600 cargo test -p radian-tui --test mermaid --no-fail-fast`（M4 换成
`cargo test -p grok-mermaid --test pristine`）。

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| M1 | 决定 4：边映射到 `mermaid_edge` | `blocks/markdown.rs`: `Some(grok_mermaid::Role::Edge) => DiagramRole::Edge` → `DiagramRole::Text` | `the_diagram_roles_come_from_the_theme` | ✅ 1 failed |
| M2 | 决定 3：太宽返回 `TooWide`，**不**走上游 `fallback()`（不出现「open the image」） | `third_party/grok-mermaid/src/lib.rs`: `Oversize::Width` 分支换成 `mermaid::render(...)` 的 fallback 框并 `Ok` | `the_two_wide_samples_report_the_columns_they_need`、`a_too_wide_sample_names_the_columns_in_the_transcript`、`a_diagram_that_does_not_fit_keeps_the_source_and_names_the_reason`、`a_width_change_recounts_and_redraws` | ✅ 4 failed |
| M3 | 决定 3：`needed` = `max_width = None` 再渲一次的最宽 plain 行 | `lib.rs`: `needed: Art::from_mermaid(art).width` → `needed: 0` | `the_two_wide_samples_report_the_columns_they_need`、`a_too_wide_sample_names_the_columns_in_the_transcript` | ✅ 2 failed |
| M4 | 决定 2：`src/mermaid.rs` 只有 `use` 行与可见性两类有记录的改动 | `src/mermaid.rs`: 上游一行 `const MAX_LABEL: usize = 28;` → `27` | `third_party/grok-mermaid` `tests/pristine.rs::the_two_edits_are_the_only_difference_from_upstream` | ✅ 1 failed |

4 条变异，4 条抓住。M4 在 vendored crate 里跑（本 crate 的 `mermaid.rs` 快照测试看不见上游 diff）。

## esc-noquit: `Esc` 不再退出空闲会话＼�

Base: 本 worktree（`oc/esc-noquit`，分支自 `e2a4436`）。变异在 worktree 内直接改 `src/app.rs`，
改前 `cp src/app.rs .radian/scratch/esc-noquit/app.rs.orig`，改后 `cp` 还原并 `cmp` 一致
（`git status --short` 变异后只剩被测文件，还原后为那三个改动文件）。命令：
`timeout 600 cargo test -p radian-tui --locked --no-fail-fast --test quit <test>`。

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| EN-M1 | 空闲且输入为空时 `Esc` **什么都不做**（绝不退出） | `app.rs` `Action::Escape` 的空闲分支 `UpdateResult::none()` → `self.quit = true; UpdateResult::quiet(Effect::Quit)` | `quit::esc_never_quits_when_idle_and_empty`（`assert!(!app.quit, …)`：`Esc must not quit an idle session`） | ✅ 1 failed |

1/1 caught。EN-M1 只动空闲分支：运行中的 turn 取消（`update::esc_with_an_empty_input_and_a_running_turn_cancels`）与非空输入清空都不受影响，两个键都仍然绿；`--pick` 选择器的 `Esc` = 退出走的是
`Input::PickerKey` 另一条路径（`pty::pty_pick_starts_new_and_escape_leaves_no_session`），本改动不碰它，变异也不影响它。

## ADR 0027 slice ①: the footer component registry and the model switch (2026-10-16)

Worktree `footer-components-review-fix` (base `4a9c492`, the failed first task's head). In-place
binary-only mutations, driven by `.radian/scratch/mut/mutations.py` (not committed): each file
backed up under `.radian/scratch/mut/`, the anchor asserted present **exactly once** (a `cargo fmt`
shift or a no-op patch is a hard `PATCH FAILED` — the U3/U4 trap), the named test run with
`cargo test -p radian-tui --locked --no-fail-fast <target> <name>`, then the file restored with
`shutil.copyfile` + `os.utime` (so cargo rebuilds, the thin-① cache trap) and `cmp`ed
byte-for-byte; `git status --short` shows only the intended edits after the run.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| REG-M1 | decision 1: every built-in resolves through the registry | `components::lookup` drops its built-in branch (only the store fallback is left) | `footer::the_default_footer_is_two_rows` FAILED (the identity row loses the model, the branch and the reasoning glyph) |
| REG-M2 | decision 1: a compiled-in component claims its id first | `lookup` consults the store **before** the registry | `footer::a_builtin_id_wins_over_the_store_fallback` FAILED (`left: "HIJACK"`, right `"Mock 5"`) |
| REG-M3 | decision 1: the dynamic fallback serves contributor ids | `lookup` drops the store branch | `footer::an_arbitrary_store_id_renders_through_the_dynamic_component` FAILED (after the publish the row is still `custom.thing`) |
| REG-M4 | decision 1: one registry entry per built-in | `&tokens::Tokens` removed from `COMPONENTS` | `footer::tests::the_registry_lists_every_builtin_once` FAILED ("the registry is missing tokens") |
| SW-M1 | the applied `/model` switch follows through to the footer's `model` | the `ConfigChanged` handler no longer writes `model_name`/`provider`/`model_display` | `footer::the_footer_model_follows_an_applied_switch` FAILED ("the model id follows": `old-model` ≠ `claude-new`) |
| SW-M2 | a switch resolves no catalog display name (startup passes `None`) | the handler sets `model_display = Some("Claude New")` instead of `None` | `footer::the_footer_model_follows_an_applied_switch` FAILED ("no catalog display name is resolved: Some(\"Claude New\")") |

6/6 caught, no equivalent mutants; every mutated file `cmp`ed byte-identical after its mutation.

**Old-code check.** The first task's test
`an_arbitrary_store_id_renders_through_the_dynamic_component` was re-run against the pre-refactor
`footer.rs` (`631c8dd`, keeping the new test file): it fails at the second render
(`left: "custom.thing", right: "hello"`), so it is not vacuous. Its **comment** was wrong in a
different way, and the comment now says this: the old `contribution_item` *did* have a general
store fallback for any id `item_is_known` accepted — a preset id such as `opencode-go.usage` went
through it and drew the store text — while a **user-written** id was marked `known = false` by the
old `resolve_item`, so the old `render_item` returned the raw id *before* consulting the store and
a published value never appeared. The new `a_builtin_id_wins_over_the_store_fallback` adds the
behaviour the old dispatch got wrong (a store value under a built-in id was drawn instead of the
built-in); it fails the same `631c8dd` footer the same way.

**Coverage (decision 1).** The old `item_placeholders` table listed 20 built-in ids — `brand`,
`host`, `model`, `reasoning`, `tier`, `cwd`, `git_branch`, `worktree`, `status`, `session`,
`context`, `context_bar`, `tokens`, `speed`, `elapsed`, `cost`, `pending`, `tasks`, `mcp`,
`scroll` — and the registry has exactly those 20, one file each (`tasks` is the ADR 0030 addition;
the ADR C2 list omits it). `every_registry_id_has_a_component_file` pins the `components/<id>.rs`
file per registry id, so a new built-in cannot be registered without its file.
`git_branch`/`worktree`/`status` are ordinary registered components that read the contribution
store in their `data` — no `CONTRIBUTION_ITEMS` special case — and an id no built-in claims
resolves to the dynamic `StoreComponent`.

### ADR 0027 slice ① task 2: the `command` component's rendering (2026-10-16)

Same in-place driver as the `radian-host` entry above (`.radian/scratch/mut/mutate.py`, no
committed script): exact-match anchor, `cargo test -p radian-tui --test footer --locked
--no-fail-fast <name>`, restore from a byte-for-byte backup. Base `d647750`.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| T1 | a configured `command` id is a **known** item (no "unknown item" warning; hides until its value) | `is_command_item` always returns `false` (the pre-change dispatch, which marks a user-written id unknown and shows it raw) | `a_command_item_appears_where_its_id_is_placed` FAILED (the unknown-id warning appeared and the raw id was drawn) |
| T2 | a JSON `symbol` is rendered | `symbol_for` drops the `data.symbol` fallback | `a_command_json_style_and_symbol_render_and_the_symbol_style_applies` FAILED (no `!` on the row) |
| T3 | a configured `symbol` wins over the JSON `symbol` | `symbol_for` tries `data.symbol` before `item.icon` | same test FAILED (the configured `>` was replaced by the JSON `!`) |
| T4 | a command item's format may use `{value}` without a warning | `validate_format` no longer treats a command id specially | `a_command_item_accepts_the_value_placeholder` FAILED (`unknown placeholder {value}` warning) |
| T5 | a configured `symbol_style` styles the symbol a JSON `symbol` produced | the symbol's style ignores `symbol_style` | `a_command_json_style_and_symbol_render_and_the_symbol_style_applies` FAILED (the `!` was not red) |

5/5 caught; `crates/radian-tui/src/footer.rs` compared byte-identical after every mutation.

**Old-code check (T1).** The base tree has no `is_command_item`: `resolve_item`'s unknown rule is
`components::builtin(&id).is_none()`, so a user-written `weather` id is `unknown = true`, its row
draws the raw id and warns — exactly what T1 reproduces, and why
`a_command_item_appears_where_its_id_is_placed` is not vacuous. The end-to-end
`a_printf_fixture_appears_in_the_configured_footer_location` (a real `printf hi` fixture through
the runner and into the footer) is caught by T1 on the render side and by the `radian-host`
mutants H1/H5 on the run side.

### ADR 0027 slice ① fix round `footer-s1-fix1`: the registry tables vs `631c8dd` (2026-10-16)

Same in-place driver as above: exact-match anchor, `cargo test -p radian-tui --lib --locked
--no-fail-fast <name>`, restore from a byte-for-byte backup (`.radian/progress/*.bak`).

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| P1 | every built-in's default priority matches the `631c8dd` `item_priority` table | `Model`'s `priority` override is dropped (back to the refactor's 50 fallback) | `footer::tests::the_registry_matches_the_pre_refactor_priority_table` FAILED (`priority for model`: left 50, right 90) |
| P2 | `context_bar`'s default glyph is empty in every set (the old `glyph` table had no `context_bar` row) | `ContextBar::glyphs` is given `context`'s `["", "\u{f035b} ", "[", ""]` | `footer::tests::the_registry_matches_the_pre_refactor_glyph_table` FAILED (`default glyphs for context_bar`: left `["", "\u{f035b} ", "["]`, right `["", "", ""]`) |

2/2 caught; `crates/radian-tui/src/footer/components/model.rs` and `.../context_bar.rs` compared
byte-identical (same sha) after each mutation.

The five pin tests this fix round adds — `the_registry_matches_the_pre_refactor_priority_table`,
`..._placeholder_table`, `..._role_table`, `..._threshold_table`, `..._glyph_table` — compare every
built-in id against the `631c8dd` tables. Priority is the **required** fix: `brand` (92), `model`
(90) and `host` (78) had lost their overrides and fell back to 50. The glyph comparison found a
second regression (`context_bar`), restored here. The placeholder, role and threshold tables
already matched the old values.

## ADR 0016：`workspace` 档位继承外层沙箱（2026-10-16）

worktree `.radian-worktrees/inherited-sandbox`，分支 `dogfood/inherited-sandbox`。每个变异只改一个
文件（`src/profile.rs` 或 `tests/pty.rs`），跑完把保存的原文写回并 `cmp` 确认逐字节一致（副本在
`.radian/mut/`）。只跑应当抓住它的测试二进制（`--test pty`；IS-7 在 `sandbox-exec` 下跑）。
`radian-sandbox` 与 `radian-host` 的同类变异见各自 crate 的 MUTATIONS.md。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| IS-4 | `build_sandbox` 的 `workspace` 档位无条件选 `inherited`（不看内核；即「约束为假时也选用」） | `pty_the_startup_summary_names_the_real_sandbox_backend`（摘要写成 `workspace · inherited`，不是 `macOS sandbox`） | ✅ FAILED |
| IS-5 | 探测失败（`RADIAN_SANDBOX_PROBE_FAIL=1`）回退到 `inherited` 而不是拒绝 | `pty_a_failed_sandbox_probe_refuses_instead_of_running_unsandboxed`（应用没有退出） | ✅ FAILED |
| IS-6 | 检测读环境变量（`RADIAN_SANDBOX_INHERIT` 存在即为真） | `pty_the_startup_summary_names_the_real_sandbox_backend`（该测试给子进程设了 `RADIAN_SANDBOXED=1`/`RADIAN_SANDBOX_INHERIT=1`，摘要变成 `inherited`） | ✅ FAILED |
| IS-7 | 去掉 `pty_the_workspace_write_notice_names_the_mcp_sandbox_gap` 的 `skip_inside_sandbox!`（标记少一个） | 该测试在 `sandbox-exec` 下**不再跳过**，真的启动 TUI：`inherited` 的启动行没有 MCP 那一句，断言失败（同时标记计数变成 4） | ✅ FAILED |
| IS-8 | `/access` 查询不再追加 `sandbox_note`（`app.rs` 去掉那两行） | `app::tests::access_repeats_an_inherited_sandbox_line`（第二条通知缺失） | ✅ FAILED |

5/5 抓住。IS-4 与 IS-6 由同一个测试抓住：前者让真实后端名从摘要里消失，后者让环境变量能选中
`inherited`，两者都让摘要行写成 `inherited`。IS-7 是 acceptance 5 的两个方向之一：清单里的测试在沙箱
内必须打标记后返回，去掉一个既少一行标记、又让测试在它跑不了的环境里真跑（于是失败）。
## ADR 0027 slice ② (`footer-usage`): the `usage` component (2026-10-16)

Driver: exact-match `patch`, then `timeout 300 cargo test -p radian-tui <binary> --locked
--no-fail-fast`, one mutation at a time, each running only the binary that should catch it.
Restore: copy the byte-for-byte backup from `/tmp/footer-mut/` back over the file and compare
(`shasum` identical and `git diff --quiet`). Nothing was left changed.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| U1 | a provider whose descriptor declares no `usage` source draws no usage row (ADR 0027's own mutation) | `footer/components/usage.rs`'s `data` drops `radian_host::usage_endpoint(provider)?` | `--test footer`: `a_provider_without_a_usage_source_draws_no_usage_row` FAILED (59 passed, 1 failed) |
| U2 | `/usage` prints **every** window of the snapshot | `command.rs`'s `usage_notices` takes `snapshot.windows.iter().take(1)` | `--test footer`: `usage_prints_every_window` FAILED (59 passed, 1 failed); `--lib`: `command::tests::usage_notices_lists_every_window` FAILED |
| U3 | an old usage id warns once, naming the new id | `footer.rs`'s `canonical_id` returns the new id without pushing the warning | `--test footer`: `the_old_usage_ids_still_render_and_warn_once` FAILED (59 passed, 1 failed) |

3/3 caught; `crates/radian-tui/src/footer/components/usage.rs`, `src/command.rs` and
`src/footer.rs` each compared byte-identical (`shasum` unchanged) after their mutation.

**Old-code check.** The pre-change tree (3b13d76, extracted with `git archive` into a scratch
tree outside the repository) was given three tests that state the old behaviour directly:
`old_code_draws_the_usage_row_for_any_provider` (the deck row was the `opencode-go.usage`
contribution and was drawn for a routed `anthropic` — the descriptor gate the ADR's mutation
targets did not exist), `old_code_has_no_usage_command` (`/usage` parsed as an unknown command)
and `old_code_does_not_know_the_usage_id` (`usage` was an unknown item, warned about and drawn
raw). All three pass there, so U1's and U2's requirements are new, not vacuous.

The remaining new tests cannot run on that tree at all: `Contribution::usage`,
`radian_host::USAGE_ITEM` and the `usage` component do not exist there, so the test file does
not compile. What they pin instead is verified two ways: the two pre-move-line literals are the
pre-change `opencode_go::display`/`chatgpt_usage::display` output captured **on** that tree
(`Usage (remaining): 5h: 98% · 7d: 81% · 5h ↺1h0m` and `ChatGPT usage (remaining): 5h: 95% ·
7d: 60% · 5h ↺1h0m · 7d ↺1d1h · pro`), and the snapshot those literals are drawn from is pinned
field by field in `radian-host`'s `opencode_go::tests::the_snapshot_is_the_remaining_share` /
`chatgpt_usage::tests::the_snapshot_is_the_remaining_share_with_a_derived_label`.
## oauth-redact：会话中刷新出来的 token 也必须脱敏（2026-10-16）

Base `5cfb0b1` · worktree `.radian-worktrees/oauth-redact`。每个脱敏名单只在启动时读一次
`resolved_keys()`；`chatgpt` 的 access token 在会话里被 `ensure_fresh` 刷新（约一小时后第一次）
并写回 `auth.json`，而写入的两个新值不进任何已建好的名单，于是 panic log、模型报错和导出都不再
脱敏**正在使用**的那个 token（ADR 0026 禁止）。变异直接改 `src/terminal.rs`，改前
`cp` 到 `.radian/scratch/mut/`，改后 `cp` 还原并 `cmp` 一致（`git status --short` 跑完只剩被测的
两个文件）。命令：`timeout 300 cargo test -p radian-tui --locked --no-fail-fast --test redaction`。

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| OR-T1 | 决定 3：启动后才 `register` 的值同样要脱敏 | `terminal.rs` 的 `shared_secrets` 删掉合并 `radian_host::registered_secrets()` 的循环 | `redaction::a_value_registered_after_the_list_was_built_is_still_redacted`（输出里留着 `late-access-token-…`） | ✅ 1 failed |

1/1 caught。这条测试放在独立的集成二进制（`tests/redaction.rs`）而不是 `terminal.rs` 的
`mod tests`：注册表是进程级的，同一个二进制里加值会让 `terminal.rs` 两条对
`shared_secrets` 做 `==` 全等断言的测试变得不确定。


## attach（ADR 0043 切片 ①）：只读视图的跟随、无 live、无输入（2026-10-17）

基线 `ebb9f1a` · worktree `.radian-worktrees/attach`。四条变异分别改 `src/cli.rs`（A1、A3、
A5）与 `src/attach.rs`（A2），改前 `cp` 到 `.radian/scratch/mut/`，改后 `cp` 还原并 `cmp`
一致（跑完 `git status --short` 只剩被测的三个文件）。命令：`timeout 300 cargo test -p
radian-tui --locked --no-fail-fast --test pty pty_attach…` 与 `--test attach <name>`。

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| AT-T1 | A1：`--follow` 必须 tail 新追加的字节 | `run_attach` 的 `Input::Idle` 分支改成不调 `view.poll()`（`if false`） | `pty::pty_attach_follow_shows_new_committed_events`（`ATTACH-FOLLOW-NEW` 与 `working…` 都不出现） | ✅ 1 failed |
| AT-T2 | A2：渲染路径不接受 live 事件（ADR 0010 K2 的 delta 不落盘） | `AttachView::poll` 里加一句 `projection.push_delta(StreamKind::Assistant, "MUTANT")`（即给视图接上 live 管路） | `attach::the_attach_view_has_no_live_event_sink`（源码里出现 `push_delta`/`StreamKind`） | ✅ 1 failed |
| AT-T3 | A3：除滚动/退出外的按键不产生任何副作用 | `run_attach` 的 `AttachKey::Ignored` 分支改成往 journal 追加一行 `MUTANT` | `pty::pty_attach_swallows_every_other_key`（`a key press changed the journal`） | ✅ 1 failed |
| AT-T4 | A5：不存在的 id 在打开文件**之前**就拒绝 | 删掉 `run_attach` 的 `if !path.exists()` 检查 | `attach::an_unknown_session_id_refuses_and_creates_nothing`（拒绝行变成读失败的那一行） | ✅ 1 failed |

4/4 caught。AT-T3 的第一版**没有**抓住：当时断言在发键之后立刻跑，而 app 还没处理那些键
（`wait_for_screen` 对一个已在屏上的 needle 立即返回，不构成同步）。修法是先发被吞的键、再发
PageUp（footer 从 `follow` 变 `paused`）作为 happens-before，之后的字节断言才真的覆盖已处理的
按键；AT-T1/A1 与 AT-T3/A3 各跑五次稳定（5/5）。

A4 不设变异：它本身就是「attach 首帧 ≤ 2 × `--resume` 首帧」的同机测量（同一次运行里两者各跑
三次取中位数）；debug 构建下 10k 条首帧是 attach 1185 ms、resume 1473 ms，比值约 0.8。
## pick-stderr: the `--pick` path's startup diagnostics (2026-10-17)

Base `f4c2c09` · branch `dogfood/pick-stderr` · worktree `.radian-worktrees/pick-stderr`.
`--pick` (and a valueless `--resume`) enter the alternate screen *before* the session exists, so
every startup `eprintln!` between the picker and the session landed on the alternate screen; the
frame diff skips the cells that did not change, so the text (`[control] <sock>`, the trust status
line) stayed drawn over the input box. `StartupLog` now collects those lines,
and the startup block takes them. A memory warning becomes a transcript notice instead of stderr
(the rule ADR 0021 already states for the memory notice).

Each mutation was applied in place in `src/cli.rs`, restored from a byte-for-byte copy and
compared (`cmp`) after every run. Command:
`cargo test -p radian-tui --test pty --locked --no-fail-fast <name>`.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| PS-T1 | on the `--pick` path the startup diagnostics are not written to the terminal | `startup_log.deferred = Some(Vec::new())` dropped, so nothing is deferred | `pty_pick_prints_no_startup_diagnostics_into_the_alternate_screen` FAILED (the raw stream after `?1049h` carries `[control] `) |
| PS-T2 | every `eprintln!` site on that path goes through the log | the `[control] <path>` site prints with `eprintln!` again | same test FAILED |
| PS-T3 | the block takes the collected lines | `startup_lines.extend(startup_log.take())` removed | same test FAILED (no `[control] ` line in the block) |
| PS-T4 | the trust line is not collected (the block already carries it) | `StartupLog::print_before_screen` → `line` | same test FAILED (the trust line appears twice) |
| PS-T5 | a memory warning is a transcript notice, not stderr | `memory_warning` reverted to `eprintln!` | `pty_a_memory_warning_is_a_transcript_notice_not_stderr` FAILED |

5/5 caught; `src/cli.rs` compared byte-identical after every restore.


## attach 复核修复：id 必须先过 `validate_session_id`（2026-10-17）

复核轮（p1 2026-10-17）：`run_attach` 直接用 `dir.join(format!("{id}.jsonl"))` 拼路径，而
`--resume` 先过 `radian_host::validate_session_id`（`crates/radian-host/src/session_id.rs:16`）。
`../decoy` 这类 id 因此能读到 sessions 目录**之外**的文件。修复在 `run_attach` 的入口加上同一个
校验（在任何目录解析与文件访问之前），`src/cli.rs`；基线是本分支的 `7675130`，变异用
`cp`/`cmp` 往返（`.radian/scratch/mut/cli.rs.bak2`，比对后删除）。命令：`timeout 300 cargo test
-p radian-tui --locked --no-fail-fast --test attach a_path_like` 与 `--test pty pty_attach_refuses`。

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| AT-T5 | 非法 id 在解析目录/访问文件之前就被拒（非零退出、一行 stderr、不读不写） | 校验调用改成 `if false && let Err(…)`（等于不校验，id 原样进 `join`） | `attach::a_path_like_session_id_is_refused_before_anything_is_read`（stderr 变成 `radian: terminal: Device not configured (os error 6)`，说明它已经打开并读到了 `../decoy`）与 `pty::pty_attach_refuses_a_path_like_id_without_reading_it`（屏上出现 decoy 的 `❯ ATTACH-OLD-USER`/`ATTACH-OLD-REPLY`） | ✅ 2 failed（两个二进制各一） |

1/1 caught（两个测试分别抓住）。两个测试在**修复前**的代码上同样失败：句柄测试看到的是
`terminal: …`（已经读到文件），pty 测试看到 decoy 的正文。
