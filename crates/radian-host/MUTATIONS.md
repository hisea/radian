# `radian-host` mutation record (provider selection + shared describe)

Base: `47ede1a` · one-shot worktree `/tmp/anthost-mut` (created from HEAD, deleted after) ·
each mutation run with `--no-fail-fast`; the file was restored from a copy after each and
`git status --short` was empty at the end.

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1 | `--provider anthropic` must use the Messages API | `Provider::from_name("anthropic")` returns `OpenAiCompatible` | ✅ caught (`anthropic_provider_sends_a_messages_request_and_shows_the_reply`: path is not `/v1/messages`) |
| M2 | the default key env switches with the provider | `Provider::default_api_key_env` for Anthropic returns `OPENAI_API_KEY` | ✅ caught (`anthropic_reads_anthropic_api_key_env_by_default_and_never_leaks_it`: the key env is not read, setup refuses) |
| M3 | `--thinking` must reach the request body | drop the `with_thinking` call in `build_model` | ✅ caught (`anthropic_thinking_modes_are_sent_in_the_body`) |
| M4 | the snapshot names the real provider | hardcode `"provider": "openai-compatible"` in `open_journal` | ✅ caught (`the_session_snapshot_names_the_real_provider`) |

The shared `describe` (item 2) is a pure text move; the two startup-text tests that pin it
are `the_startup_text_does_not_claim_credential_isolation` (agent-cli) and
`pty_the_workspace_write_notice_names_the_mcp_sandbox_gap` (radian-tui); both were run and
pass, and there is no separate behavior to mutate beyond the wording those tests already fix.

## 追加（anthost-2，thinking 预算检查）

Base `5188989` · one-shot worktree `/tmp/anthost-mut2` · `--no-fail-fast`；每个变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M5 | 检查上界写成 `<=`（允许 `budget == max_tokens`） | ✅ 抓住 (rc=101) | `anthropic_rejects_a_thinking_budget_out_of_range_before_creating_a_session`（`enabled:16384` 那轮） |
| M6 | 去掉下界（`0..max_tokens`，允许 `enabled:1000`） | ✅ 抓住 (rc=101) | 同上（`enabled:1000` 那轮） |

## 追加（smallfixes，Anthropic-only flags）

Base `f552250` · one-shot worktree `/tmp/small-mut` · `--no-fail-fast`；变异后 `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M7 | `check_provider_flags` 直接 `return Ok(())`（openai 下忽略 `--thinking`/`--max-tokens`） | ✅ 抓住 (rc=101) | `model::tests::anthropic_only_flags_are_refused_for_openai_compatible`，以及宿主的 `anthropic_only_flags_are_refused_under_openai_compatible`（CLI）与 `pty_anthropic_only_flags_are_refused_under_openai_compatible`（TUI） |

M7 同时被单元测试和两个宿主的用法错误测试抓住；三者都断言退出码/错误，而不是只看 stdout。

## ADR 0013 决定 4：诊断目录（2026-10-13）

Base: `f6fda6f` · 一次性检出 `/tmp/diag-mut`（从 HEAD 建，做完删掉）· 独立 `CARGO_TARGET_DIR` ·
每个变异用 `--no-fail-fast` 跑，然后 `git checkout -- .`；`git status --short` 为空。
清理规则与 TUI 的 panic 写盘测试在 `crates/radian-host/src/diagnostics.rs` 的单元测试与
`crates/radian-tui/tests/pty.rs`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | `clean_with` 的 `expired` 恒为 `false`（7 天前的不删） | `diagnostics::tests::{a_file_older_than_the_retention_is_removed_and_a_new_one_kept, append_also_cleans_the_directory, cleanup_leaves_files_that_are_not_ours}` | ✅ 抓住 |
| M2 | 超限时按 mtime 倒序（先删最新） | `diagnostics::tests::an_oversized_directory_drops_the_oldest_first_until_under_the_limit` | ✅ 抓住 |
| M3 | `is_diagnostic_file` 恒为 `true`（清理碰非本程序文件） | `diagnostics::tests::{cleanup_leaves_files_that_are_not_ours, only_our_own_file_names_are_diagnostics}` | ✅ 抓住 |
| M4 | 诊断写入失败时 `std::process::exit(1)` | `pty_an_unwritable_diagnostics_directory_is_only_a_notice` | ✅ 抓住 |

4/4 全抓。M1–M4 是 brief 要求的四项；M4 由 pty 测试抓住，断言 panic 仍以 101 退出、失败只是
一行 `[diagnostics]` 提示。

## ADR 0018：模型目录（2026-10-13）

一次性检出 `/tmp/catalog-mut`（base `e3bfd3c`），每个变异用 `git checkout --` 恢复、
`git status --short` 为空；测试一律 `--no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M5 | 24 小时内的缓存仍然出网（去掉 fresh-cache 短路） | ✅ failed | `a_missing_cache_fetches_once_and_then_uses_the_cache`（第二次 resolve 又发了一次请求） |
| M6 | `--context-window` 不优先（显式分支被去掉，改查目录） | ✅ failed | `an_explicit_window_wins_and_does_not_fetch`、`an_explicit_window_never_fetches` |
| M7 | 忽略 `limit.input`（窗口直接用 `context`） | ✅ failed | `window_tokens_is_min_of_input_and_context_and_zero_is_unknown`（也由 `a_missing_cache_fetches_once_and_then_uses_the_cache` 抓住） |
| M8 | 目录请求带上 `authorization` 头 | ✅ failed | `the_catalog_request_carries_no_key_or_header` |
| M9 | 显式 `--context-window` 时仍出网拉取（显式分支改成 `load(...)`） | ✅ failed | `an_explicit_window_never_fetches`（无缓存那轮变成 1 次请求） |

5/5 全抓，无漏网、无等价变异。M6/M7 的单元测试与集成测试都失败（同一行为两条路径）。

## ADR 0018：`B = min(I, C − R)`（`pi/guard`，2026-10-13）

一次性 worktree `/tmp/radian-guard-mut`（HEAD = `8825691`，独立 target），`--no-fail-fast`；
变异后 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | `R` 用目录的 `limit.output`（`build_window` 把 `output` 当成 `request_max_output`） | ✅ failed | `catalog_provider_overrides_the_hostname`（`B` 变成 616000，而非 922000） |
| M11 | `--context-window` 当成 `B`（显式分支算出 `B` 后又被 `explicit` 覆盖） | ✅ failed | `an_explicit_window_wins_and_does_not_fetch`（期望 `B = 83616`，得到 100000） |
| M12 | `B` 不取 `I` 的 min（`input.min(after_reservation)` → `after_reservation`） | ✅ failed | `the_input_budget_is_min_of_input_and_context_minus_the_reservation` |

M7 里的单元测试 `window_tokens_is_min_of_input_and_context_and_zero_is_unknown` 已由本轮改名为
`the_input_budget_is_min_of_input_and_context_minus_the_reservation`；目录 `output` 只做警告、
不参与 `R`。

## ADR 0021 M1 第二片：项目身份与 `MemoryService`（2026-10-13）

Base `5d80946` · 一次性 worktree `/tmp/memsvc-mut`（从 HEAD 建，做完删掉）· **独立 `CARGO_TARGET_DIR`**
· 每个变异跑 `cargo test -p radian-host memory` 与 `cargo test -p agent-cli --test cli memory`
（都 `--no-fail-fast`），然后 `git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | worktree 不解析 `commondir`（返回自己的 gitdir） | `memory::identity::tests::{a_worktree_and_its_main_repository_share_one_key, the_identity_matches_git_common_dir_when_git_is_available, a_relative_gitdir_and_commondir_are_resolved}`、`memory::service::tests::a_worktree_resolves_to_the_main_repositories_project`、`memory_project_is_shared_with_a_git_worktree` | ✅ 抓住 |
| M2 | `Unlinked` 时 Project 写自动注册 | `memory::service::tests::{an_unlinked_workspace_refuses_a_project_write_and_still_reads_global, relinking_after_a_move_finds_the_original_records}`、`memory_unlinked_workspace_refuses_project_writes_and_lists_global`、`memory_relink_after_a_move_finds_the_original_records` | ✅ 抓住 |
| M4 | 用户命令写 `Source::Agent` | `memory::service::tests::{a_project_write_reads_back_after_creating_the_project, user_commands_set_the_user_source_and_check_the_revision}`、`memory_add_list_edit_confirm_export_import_show_round_trip`、`memory_json_output_is_parseable_and_has_stable_fields` | ✅ 抓住 |
| M5 | `add` 默认写 Global | `memory::service::tests::{a_project_record_overrides_a_global_one_with_the_same_key, a_project_write_reads_back_after_creating_the_project, …}` 与 6 个 CLI `memory_*` 测试 | ✅ 抓住 |
| M6 | 识别键不向上查找祖先目录 | `memory::identity::tests::a_subdirectory_finds_the_repository_above_it` | ✅ 抓住 |
| M7 | `candidate_name` 不把 `.git` 当仓库名 | `memory::identity::tests::candidate_names_use_the_repository_directory_for_a_common_dir` | ✅ 抓住 |

6/6 全抓（M3 在 `agent-cli/MUTATIONS.md`）。M4/M5 同时被单元测试与 CLI 测试抓住，说明
「服务层规则」与「命令接线」两条路径都有覆盖。

**评审补充（合并时，2026-10-13）**：原实现在没有 `commondir` 时取 gitdir 的祖父目录，真实 submodule（`<super>/.git/modules/<name>`，git 不为它写 `commondir`）因此被解析成上层仓库的 `.git`，两者静默共享一个项目。改为 git 自己的规则：没有 `commondir` 时 gitdir 即 common dir。变异"恢复祖父回退"由 `a_submodule_is_its_own_project_not_the_superprojects` 与 `a_missing_commondir_means_the_gitdir_is_the_common_dir` 抓住。

## ADR 0021 M1 第三片：agent 工具与注入（2026-10-13）

Base `2d01d9b` · 一次性 worktree `/tmp/memsvcb-mut`（从 HEAD 建，做完删掉）· **独立
`CARGO_TARGET_DIR`** · 每个变异跑 `cargo test -p radian-host --no-fail-fast`（M9 跑
`-p radian-permissions`），然后 `git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 注入按字符截断记录（`take(20)`）而不是整条 | `memory::injection::tests::{a_selected_record_is_injected_whole_never_truncated, a_record_that_does_not_fit_is_dropped_whole_not_truncated}` | ✅ |
| M2 | 忽略软配额借用（超出本作用域配额即丢弃） | `memory::injection::tests::a_global_record_borrows_the_projects_unused_share` | ✅ |
| M3 | agent 未确认段不打标题 | `memory::injection::tests::the_two_segments_are_labelled_apart` | ✅ |
| M4 | `MemoryInjected` 每次请求都写（去掉集合比较） | `tests/memory_injection.rs::the_block_reaches_the_system_prompt_and_the_event_is_written_once` | ✅ |
| M5 | `MemoryInjected` 的 scope 填成内容 | 同上（断言会话文件不含内容） | ✅ |
| M6 | 读取失败返回 `HookError`（让这一轮失败） | `tests/memory_injection.rs::a_broken_memory_log_warns_but_does_not_fail_the_turn` | ✅ |
| M7 | `memory_save` 默认 Global | `memory::tools::tests::{an_unlinked_workspace_refuses_a_project_save_and_allows_global, save_search_get_revise_round_trip_with_the_low_trust_note, saving_over_a_user_key_is_a_key_conflict_naming_the_holder}` | ✅ |
| M8 | `Unlinked` 时 Project 写自动注册 | `memory::service::tests::{an_unlinked_workspace_refuses_a_project_write_and_still_reads_global, relinking_after_a_move_finds_the_original_records}`、`memory::tools::tests::an_unlinked_workspace_refuses_a_project_save_and_allows_global` | ✅ |
| M10 | `memory_save` 走用户命令（`Source::User`） | `memory::tools::tests::{save_search_get_revise_round_trip_with_the_low_trust_note, a_global_record_can_be_revised_while_unlinked}` | ✅ |
| M11 | 工具结果去掉低可信说明 | `memory::tools::tests::save_search_get_revise_round_trip_with_the_low_trust_note` | ✅ |
| M12 | `ranked` 丢掉未匹配记录（只注入匹配项） | `memory::injection::tests::a_non_matching_record_is_still_injected_after_the_matching_ones`、`tests/memory_injection.rs::a_new_hook_instance_resumes_with_the_current_memory` 等 | ✅ |

11/11 全抓（无编译错误型「抓住」）。M9 见 `radian-permissions/MUTATIONS.md`，M13 见
`radian-session/MUTATIONS.md`。

**评审补充（合并时，2026-10-13）**：注入块原样拼接记录内容，agent 写的记录可用换行伪造"user or confirmed"段标题，把自己抬进可信段。改为把内容写成一行（换行转义为 `\n`，反斜杠为 `\\`）。变异"恢复原样拼接"由 `memory::injection::tests::agent_content_cannot_forge_the_trusted_heading` 抓住。

## ADR 0021 M1 第四片：共享命令的确认（2026-10-13）

Base `4bf767d` · 一次性 worktree `/tmp/memtui-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M4 | `confirm` 恒为 `None`（`confirmed=false` 仍执行写操作） | `memory::commands::tests::an_unconfirmed_forget_writes_nothing`；`agent_cli::cli::memory_forget_needs_confirmation_off_a_terminal`、`memory_split_and_merge_move_records_between_projects`；`radian_tui::tests::command::{a_confirmed_forget_waits_for_y_and_n_keeps_the_record, a_pending_confirmation_keeps_keys_out_of_the_input_box}`、`command::tests::memory_writes_become_a_confirmation` | ✅ |

## sessions：导出脱敏

Base: `9daed70` · 一次性检出 `/tmp/sessions-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M4 | `sessions::export` 不传 secrets 给 `ExportOptions` | `sessions::export_redacts_known_secrets` | ✅ failed |

## dogfooding D22：`split` 的确认提示

Base `a0e8b4c` · 一次性检出 `/tmp/df7-mut` · 独立 target · `--no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M5 | `split` 提示仍数参数（`move {ids.len()} record(s)`） | ✅ failed | `memory::commands::tests::the_split_prompt_counts_the_records_that_exist` |
| M6 | 全部 id 都不存在时仍进入确认（去掉 early error） | ✅ failed | 同上 |

`split` 的读取是只读的（`store().records(&Scope::Project(from))`），确认前不写任何东西；
M6 证明「全部不存在」是退出 1 的错误，而不是一次空确认。

## ADR 0018：自适应 `R`

Base `0a3c197` · 一次性检出 `/tmp/adaptr-mut`（detached，独立 target，跑完删除）· `--no-fail-fast` ·
每个变异 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M7 | 仍用固定 16384（`R = max(16384, request)`） | ✅ failed | `catalog::tests::the_output_reservation_scales_with_the_window_and_the_request`；也由 `cli::the_startup_line_names_an_explicit_window` 抓住（B 回到 3616） |
| M8 | `⌊C/4⌋` 写成 `⌊C/2⌋` | ✅ failed | 同上（C=8192 → 4096 而非 2048） |
| M9 | 忽略请求的 max_output（去掉 `.max(request)`） | ✅ failed | 同上（C=8192 + max_tokens=16384 应得 16384） |
| M10 | `C` 未知时返回上限 16384（而不是 0/请求值） | ✅ failed | 同上（`output_reservation(None, None)` 应为 0） |

4/4 全抓。M7 走单元与 CLI 两条路径：固定预留会让 `--context-window 20000` 的 `B` 回到旧的 3616。

## Phase 4：只读配置读取器（ADR 0008，条目重编号为 M11–M14，避免与 main 的 M7–M10 撞号）

Base `8028d9e` · 一次性 worktree `/tmp/config-mut`（从 HEAD 建，做完删掉）· **独立
`CARGO_TARGET_DIR`** · 每个变异跑 `cargo test -p radian-host`、
`cargo test -p radian-tui --lib --test config`、以及 `cargo test -p agent-cli --test cli` 的
`config` / `a_plaintext_secret` / `a_lower_schema` 过滤（都 `--no-fail-fast`），然后
`git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M11 | 敏感键裸字符串被接受（明文密钥） | `config::tests::a_bare_secret_is_refused_without_echoing_the_value`、`config_fixtures::the_reader_matches_the_reference_checker` | ✅ |
| M12 | 拒绝信息回显密钥值（把值拼进 `key` 路径） | `config::tests::a_bare_secret_is_refused_without_echoing_the_value`、`config_fixtures::the_refusal_messages_never_echo_a_value`、`cli::a_plaintext_secret_in_the_config_is_refused_without_echoing_it` | ✅ |
| M13 | 更低的 `schema_version` 被静默接受 | `config::tests::a_missing_version_and_a_lower_version_are_refused`、`config_fixtures::the_reader_matches_the_reference_checker`、`cli::a_lower_schema_version_refuses_to_start_without_creating_a_session` | ✅ |
| M14 | 未知 `permissions.profile` 被当作已知档位 | `config::tests::an_unknown_profile_is_refused_and_an_unknown_theme_is_ignored` | ✅ |

4/4 抓住。（这些变异跑里 `bad_mcp_config_exits_two_with_the_path` 也出现在失败列表——它是并发 cargo
运行的 watchdog 抖动，干净树上单跑 3 次都过，不是变异引起的。）

## Phase 5：Pi 迁移（`config::migrate_pi`）

Base `5aa84b6` · 一次性 worktree `/tmp/pimig-mut` · 独立 `CARGO_TARGET_DIR` ·
每个变异 `git checkout -- .` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M15 | 存在 `auth.json` 时读取它（只把 NotFound 当跳过） | `pi_migrate::auth_json_is_never_read`、`cli::config_import_pi_never_reads_auth_json` | ✅ |
| M16 | 字面 `apiKey` 被复制进草稿（建议名直接用字面值） | `pi_migrate::a_literal_key_becomes_an_environment_reference_and_is_never_shown` | ✅ |
| M17 | 差异说明回显字面值（note 里带上 `{text}`） | `pi_migrate::a_literal_key_becomes_an_environment_reference_and_is_never_shown`、`cli::config_import_pi_drafts_a_valid_configuration_and_never_shows_a_literal_key` | ✅ |
| M18 | `--write` 覆盖已存在文件（去掉存在性检查） | `pi_migrate::the_write_is_owner_only_atomic_and_never_overwrites` | ✅ |
| M19 | 写盘前不经读取器校验 | `pi_migrate::an_invalid_draft_is_never_written` | ✅ |
| M20 | 执行 `!command`（`sh -c` 跑掉 `!` 后面的部分） | `pi_migrate::a_command_key_is_listed_and_never_executed`、`cli::config_import_pi_lists_a_command_key_and_does_not_run_it` | ✅ |
| M21 | 去掉按键与 Radian 默认键的冲突检查 | `pi_migrate::a_keybinding_that_collides_with_a_radian_default_is_skipped`、`pi_migrate::the_typical_fixture_maps_the_endpoint_model_key_and_preferences` | ✅ |

7/7 抓住。**诚实补充（M15）**：第一版变异写成 `let _ = std::fs::read_to_string(dir.join("auth.json"));`
——**没被抓住**，因为它把权限错误吞掉了，行为上与「不读」不可区分。改成「文件存在就读、只有
NotFound 才跳过」（真实实现会这么写）之后，`auth.json` 权限 0000 的 fixture 让 `import_pi` 直接
失败，两个测试都抓住。也就是说「绝不读 auth.json」这条规则的可测形式就是 brief 建议的那种：
不可读的凭据文件不得影响迁移。

## s1q：待决请求队列（ADR 0023 U7）

Base: `433cdce` · 一次性检出 `/tmp/s1q-mut`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| P1 | 等待方放弃后元素仍留在队列（`retain_open` 不清） | `pending::a_dropped_waiter_disappears_and_a_late_answer_is_late`、`pending::a_cancelled_approval_leaves_no_element` | ✅ failed |
| P2 | `abort_session` 波及其他 session（去掉 origin 判断） | `pending::abort_session_completes_only_that_session` | ✅ failed |
| P3 | 迟到答复被当作成功（忽略 `send` 的 `Err`） | `pending::a_dropped_waiter_disappears_and_a_late_answer_is_late` | ✅ failed |
| P4 | `push` 不通知 | `pending::answer_completes_the_waiter_and_notifies` | ✅ failed |
| P5 | 顺序改为 LIFO（`push_front`） | `pending::two_sessions_keep_arrival_order_and_do_not_overwrite_each_other` | ✅ failed |

5/5 抓住。P1/P3 起初漏过：原测试在 `answer` 前先调了 `len()`，把放弃的元素清掉，于是走了"id 不存在"的 Late 分支、也没覆盖 `len` 清理；改成先 `answer`（走 `send` 失败路径）、再加一个"drop 后不 answer 只 `len`"的元素后抓住。

## ADR 0024：问题的待决队列（2026-10-13）

Base `4866944` · 一次性检出 `/tmp/ask-mut`（detached，跑完删除）· 独立 target ·
`cargo test -p radian-host --test pending_questions` · 变异后从副本还原。
审批侧的 `tests/pending.rs` 一行未改、照常通过（`QueuedApprover` 加了默认类型参数）。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| A7 | `abort_session_all` 把问题以 `Ok(答案)` 而不是 `Err(PromptError::Gone)` 结算 | ✅ 抓住 | `a_closed_session_aborts_its_questions_with_prompter_gone`、`a_shared_queue_session_close_aborts_both_kinds`、`a_late_answer_is_ignored` |
| A8 | `abort_session_all` 去掉 origin 判断（中止所有 session） | ✅ 抓住 | `a_closed_session_aborts_its_questions_with_prompter_gone`（另一个 session 的问题被误中止） |

2/2 抓住。A7 说明"session 关闭 → 问题得到 `prompter_gone`"这条映射有测试钉住；
A8 说明共享队列的按 session 隔离没有因为两种请求共用一条队列而丢失。

## ADR 0023 Step 1 · PR ④：Session Facade 与投影（2026-10-13）

Base `7b811b7`（合并 PR ③ 后的 `5dcac58` 上原样重跑，同样 4/4 抓住）· 一次性检出（detached，跑完删除）· 独立 target · `--no-fail-fast`；
每个变异后从副本还原，结束时 `git status --short` 只剩变异脚本。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| S1 | `LocalSession::command` 去掉能力校验（直接执行） | ✅ 抓住 | `session::a_local_session_refuses_a_command_it_does_not_declare` |
| S2 | `SessionProjection::write` 先置脏标志、后写值 | ✅ 抓住 | `projection::tests::the_value_is_written_before_the_flag_is_set` |
| S3 | `open_session` 忽略 `SessionSetup.capabilities`，一律声明全部能力 | ✅ 抓住 | `session::a_local_session_refuses_a_command_it_does_not_declare` |
| S4 | `open_session` 不写 MCP 事实（`projection.set_mcp`） | ✅ 抓住 | `radian-tui` 的 `pty_mcp_tools_run_through_approval_and_the_status_bar_shows_them` |

4/4 抓住。S1 与 S3 是同一个断言的两次独立破坏：一次在**执行前**的校验，一次在**声明**的来源；
宿主"再次校验、不信任 UI"的两半都被钉住。S2 同时说明"先写值、再置标志"的顺序不是注释。

## s1e-2：`ask_user` 的注册与关闭时的队列中止（2026-10-13）

Base `17fb385` · 一次性检出 `/tmp/s1e2-mut`（detached，跑完删除）· `--no-fail-fast`；
变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| T1 | 只有给了 `--workspace`（有编码工具）时才注册 `ask_user` | ✅ 抓住 | `radian-tui` 的 `pty_ask_user_is_registered_without_a_workspace` |
| T2 | `LocalSession::stop` 不中止队列（恢复成只靠进程退出 drop） | ✅ 抓住 | `session::stopping_a_session_aborts_its_pending_requests` |

2/2 抓住。T1 就是审查发现的那条：没有 `--workspace` 时模型看不到 `ask_user`，而启动行仍说 `on`——
变异后启动行与实际注册都由 `SessionFacts` 反映，测试的两条断言同时失败。T2 恢复成"进程退出时靠 drop
得到 abandoned"的碰巧 fail-closed；带 watchdog 的测试在 2 s 内拿不到中止结果即失败。

## ADR 0026 第 0 片（a）：auth.json、provider 描述符、目录字段、凭据禁读

Base `deb583f` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| A26-1 | `upsert_api_key` 写回时重建空 map，丢掉其他条目 | `auth::tests::a_write_preserves_unknown_and_oauth_entries_and_is_atomic` | ✅ failed |
| A26-2 | 放宽权限检查（`mode & 0o077 != 0` 短路成 false） | `auth::tests::a_wide_permission_file_is_refused_with_a_fix_command` | ✅ failed |
| A26-3 | `!command` 被执行（去掉拒绝，改成 `/bin/sh -c` 跑它） | `auth::tests::a_command_key_is_refused_and_never_run`（标记文件被创建 / 返回值不是 None） | ✅ failed |
| A26-4 | `ResolvedKey` 的 `Debug` 打印真实 key | `auth::tests::debug_never_prints_the_key` | ✅ failed |
| A26-5 | 逃逸矩阵不把 `credentials` 加进禁读 | `escape_macos`（`radian_home.credentials_unreadable` 在沙箱里可读） | ✅ failed |

5/5 抓住。A26-5 由 `crates/radian-sandbox` 的 macOS 矩阵跑（Linux VM 暂停中，未跑）。

### auth-2：权限检查的参数化测试

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| A26-6 | 权限检查只看其他用户位（`0o077` → `0o007`），放行同组可读的 `0640` | `auth::tests::a_wide_permission_file_is_refused_with_a_fix_command`（`mode 0640 must be refused`） | ✅ failed |
| A26-7 | 权限检查只看组位（`0o077` → `0o070`），放行其他人可读的 `0604` | 同上（`mode 0604 must be refused`） | ✅ failed |

2/2 抓住。原测试只用 `0644`，所以 `0o007` 变异不被发现；改成参数化（0640/0660/0604/0644/0666/0607/0770
拒；0600/0400 接受）后两条变异各自被对应的模式抓住。
