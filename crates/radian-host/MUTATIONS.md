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

## ADR 0026 第 0 片（b）交叉引用

变异 M23（静默忽略 `model.api_key`）、M24（漏掉 `x-opencode-session`）、M25（非通用描述符接受
`--base-url`）改的是本 crate 的代码，但抓住它们的测试在 `agent-cli`；明细见
`crates/agent-cli/MUTATIONS.md` 的 M22–M25。

## ui-r2：`/theme` 写回与启动偏好（2026-10-14）

Base `ba7568d` · 一次性检出 `/tmp/ui2-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-host`，变异后 `git checkout --` 还原，提交前 `git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| TW1 | 写回时丢掉注释（`doc.to_string()` 后过滤掉 `#` 行） | 除 `tui.theme` 外逐字节保留 | `theme_write::tests::write_theme_changes_only_the_theme_line` FAILED |
| TW2 | 只读判定改成 `if false`（高版本也写） | 高 `schema_version` 不写 | `theme_write::tests::write_theme_refuses_a_read_only_config` FAILED |

2/2 抓住。新增键 `tui.tips`/`tui.banner` 的解析测试在 `config`（`startup_pref_tests`）；
fixture `docs/schema/config/tui-tips-banner.toml` 由 `check-config-fixtures.sh` 校验。
## ADR 0026 D11：统一思考级别（2026-10-14，重做）

Base `442f55a` · 变异只在一次性 worktree 里做（`git worktree add --detach /tmp/radian-pi-mut-N <commit>`），
跑完 `git worktree remove --force`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T1 | 夹取改为先向下取（交换两段循环） | `provider::thinking_tests::the_clamp_prefers_the_nearest_supported_level_above` | ✅ |
| T2 | 补全列出不支持的级别（filter 恒真） | `provider::thinking_tests::the_completion_lists_only_supported_levels_in_rank_order` | ✅ |
| T3 | 没有映射表时仍发参数（`resolve_thinking` 的 `None` 分支返回 `Adaptive`） | `model::thinking_resolution_tests::a_mapless_model_resolves_to_nothing` | ✅ |

## ADR 0026 第 1 片(`/login`、具名 provider)交叉引用

变异 L3（查 key 仍用通用描述符的槽位）、L4（同名时 auth.json 压过配置）、L5（验证请求的
报错带出 key）、L6（`/logout` 清空整个 auth.json）改的是本 crate 的 `provider.rs`/`login.rs`/
`auth.rs`，抓住它们的测试分别在 `agent-cli`（具名 provider 验收）、`radian-host`（`login::tests`）
与 `radian-tui`（logout pty）；明细见 `crates/agent-cli/MUTATIONS.md` 的 L1–L7。
## ADR 0026 D9：启动解析与恢复（2026-10-13）

Base `43ddbff` · 一次性检出 `/tmp/auto-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
单元变异跑 `cargo test -p radian-host --lib approval::tests::<test>`；facade 排队的变异跑
`radian-tui` 的 pty 测试。变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| RH-A1 | 配置 auto 的守卫改成 `tier == Safe`（full 下配置的 auto 生效） | full 忽略配置 auto 并警告 | `approval::tests::config_auto_is_ignored_under_full_with_a_warning` FAILED |
| RH-A2 | resume 守卫改成 `tier == Safe`（full 恢复了 auto） | full 不恢复 auto，提示 `/auto on` | `approval::tests::full_does_not_restore_auto_and_hints` FAILED；pty 层 `pty_resume_does_not_restore_auto_on_full_and_hints` 同样断言 |
| RH-A3 | facade `set_approval` 去掉 `config_queue.push` | 每次开关写一条 ConfigChanged | pty `pty_auto_key_approves_once_and_enables_auto`（journal 断言）FAILED |

3/3 抓住。CLI 优先与 workspace 恢复的正路径由
`approval::tests::an_explicit_cli_choice_wins_and_backfills` /
`resume_restores_the_journaled_state_over_the_config` 钉住。

## ADR 0026 D11 审查 A1：Anthropic 七档不同线上值（2026-10-14）

Base `0b9785b` · 一次性 worktree `/tmp/radian-pi-mut-a1`，跑完删除。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| A1a | 映射表全部改回 `adaptive`（六档同值） | `provider::thinking_tests::the_anthropic_levels_are_distinct` | ✅ |

## review-fixes 片：`auth.json` 临时文件名冲突（P2）

Base `c120b0e` · 就地变异（改 `write` 的临时名），跑完从备份还原、`git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| F2 | 临时名恢复成 `auth.json.tmp-<pid>`（无计数器/时间戳），并保留写前 `remove_file` | 同进程并发写不得互删临时文件 | `auth::tests::concurrent_writes_in_one_process_do_not_delete_each_other_s_temps` **5/5 次** FAILED，错误正是 `auth.json: No such file or directory (os error 2)`（另一个写者删掉了它正在写的临时文件） |

1/1 抓住，且稳定复现（不是偶发）：Barrier 对齐 + 50 次写，5 次全失败。

## ADR 0026 D11 审查 B1：档位预算夹到 max_tokens（2026-10-14）

Base `b46f506` · 一次性 worktree `/tmp/radian-pi-mut-b1`，跑完删除。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| B1a | 去掉档位预算夹取（`Enabled` 原样发出） | `agent-cli::a_level_budget_clamps_to_max_tokens` | ✅ |

## skills 片 1+2：发现、提示节、`skill` 工具（ADR 0025）

Base `4984e8b` · 在一次性 worktree（`/tmp/radian-pi-skills-mut`，detach）就地变异，每条跑完
`git checkout -- .`，最后 `git status --short` 为空。命令：`cargo test -p radian-host --lib skills`
（基线 20 passed）。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| SK-M2 | 去掉提示节的预算判断（不再 `break`、不数剩余） | 超预算按发现顺序截断并写 "N more skills not listed" | `skills::tests::over_budget_entries_are_dropped_in_order_with_a_remaining_count` FAILED |
| SK-M3 | 同名冲突时不 `continue`，改成 `retain` 掉先发现的再推后发现的（后来者胜） | 保留先发现的（路径序在前）并警告 | `skills::tests::a_name_collision_keeps_the_first_and_warns` FAILED |
| SK-M4 | `skill` 工具改成用名字拼路径（`root.join(name)/SKILL.md`），不走发现表 | 未知名字（含 `../a`）必须是 `unknown_skill` | `skills::tools::tests::an_unknown_name_is_a_tool_result_error` FAILED |

3/3 抓住。

**SK-M1（等价变异，不算洞）**：删掉 `discover` 里的 `candidates.sort()`。`walk` 每层都对子项排序，
而 `Path` 的 `Ord` 是**分量序**，所以 `walk` 的输出已经就是“每层按名排序的深度优先”＝`Path` 序；
删掉显式排序无可观察变化，现有顺序/确定性测试仍全绿。正因如此，代码里保留该行并写明它是契约
（防止将来 `walk` 改动悄悄破坏顺序），而不是靠某条测试钉住一个等价变异。

片 3（TUI `/skill:` / `/skills`）不在本片范围内，另行处理。

## skills 审查第 1 轮 S2/S4/S5 + 片 3（2026-10-14）

Base `c5a84f6` · 一次性 worktree `/tmp/radian-pi-skills-mut3`，每条跑完 `git checkout -- .`，最后
`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S2-M1 | 缩进行重新当顶层键处理（去掉 `if indented` 分支，`line.trim_start()`） | `skills::tests::an_indented_nested_name_does_not_override_the_top_level_name`（另 4 条也挂） | ✅ |
| S4-M1 | `skills::tools` 查表去掉 `!disable_model_invocation` | `skills::tools::tests::a_model_invocation_disabled_skill_is_unknown_to_the_tool` | ✅ |
| S5-M1 | `session_hooks` 里对调 `memory` 与 `skills` | `tests/skills_order.rs::the_production_hook_chain_puts_memory_before_skills` | ✅ |
| SL-M2 | `completion_candidates` 过滤掉 `disable_model_invocation` 的 skill | `skills::tests::completion_and_notices_include_model_invocation_disabled_skills` | ✅ |

4/4 抓住。片 1+2 的 SK-M1..M4 见上一节。

**关于"展开不进 journal 而只给模型"**：本实现里 `/skill:` 展开成的文本**就是**送进 inbox 的那条
消息，journal 与模型看到的是同一份，没有"只给模型"的旁路；pty 测试直接断言 `skill.jsonl` 里有展开后的
正文（`GREET-SKILL-BODY-MARKER`），把这层锁住。SL-M1（`/skill:` 不再展开走 `Outcome::Message`）会让
检查请求正文与 journal 的断言同时失败。

## skills 片 4：`[skills] extra_paths`（2026-10-14）

Base `beef4d5` · 一次性 worktree `/tmp/radian-pi-skills-mut4`，每条跑完 `git checkout -- .`，最后
`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S4-M1 | `discover_with_extras` 先扫 extra_paths 再扫用户级（extra 抢同名） | `skills::tests::the_user_level_wins_a_conflict_with_an_extra_path`（另 1 条也挂） | ✅ |
| S4-M2 | 配置里的 extra_path 不存在时静默跳过（不警告） | `skills::tests::a_missing_extra_path_is_one_warning` | ✅ |
| S4-M3 | `resolve_extra_path` 不把相对路径拼到配置文件所在目录 | `config::tests::extra_paths_resolve_tilde_relative_and_absolute` | ✅ |

3/3 抓住。（`~/.claude/skills` 只有列进 extra_paths 才读，由
`an_unlisted_root_is_not_read_until_it_is_an_extra_path` 钉住。）
## ui2-review1 U2/U3：共用原子写与符号链接（2026-10-14）

Base `eeadb11` · 一次性检出 `/tmp/ui2b-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-host --lib <test>`，变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| R2-1 | 临时名去掉计数器与纳秒（退化成 `tmp-<pid>`） | 同进程并发写不互删临时文件 | `config::theme_write::review_tests::concurrent_writes_do_not_error_and_leave_no_temp_files` FAILED（连跑 3 次全部抓住） |
| R3-1 | 写回前不解析符号链接（`target = path`） | 链接保持为链接、写到目标 | `config::theme_write::review_tests::write_theme_keeps_a_symlink_and_writes_its_target` FAILED |

2/2 抓住（R3-1 的补充测试 `write_theme_through_a_symlinked_directory` 在变异下仍通过：
当文件还不存在、只是父目录是链接时，直接 `open` 会经父链接落到真实目录，所以那条用例
本身对"是否显式解析链接"不敏感；抓住变异的是文件链接那条。两条都保留作回归）。

## models-refresh：opencode-go 内建列表（2026-10-14）

Base `71a9d1c` · 一次性检出 `/tmp/models-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
变异后 `git checkout --` 还原，提交前 `git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| MR-1 | `THINKING_LOW_HIGH_MAX` 的 `low` 线值改成 `"medium"` | 内建映射与上游逐字段一致 | `tests/opencode_go_models.rs::the_opencode_go_builtins_match_upstream` FAILED |

1/1 抓住。注意 `every_thinking_map_matches_supported_thinking_levels` 在变异下仍通过：
它只断言"哪些级别非 null"，不断言线值；抓住值改动的是结构测试（两者互补，都保留）。

## ADR 0031 片 A：信任记录与决策解析（2026-10-14）

Base `9fef91d` · 一次性 worktree `/tmp/radian-trust-mut`，每条跑完 `git checkout -- .`，`git status --short`
为空。命令 `cargo test -p radian-host --lib <测试名>`（基线 `trust` 13 passed）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T-A1 | `resolve` 保留**最远**祖先（继续向上遍历、最后一条胜） | `the_nearest_ancestor_wins_and_a_child_false_overrides_a_parent_true` | ✅ |
| T-A2 | `read_at` 对非 bool 值**跳过**（部分采纳）而不是整体拒绝 | `a_bad_file_is_rejected_whole_not_partly_adopted` | ✅ |
| T-A3 | `canonical` 直接返回原路径（不解析符号链接） | `a_symlink_and_its_target_share_one_decision` | ✅ |
| T-A4 | `write_at` 用 `new_mode = None`（新文件按 umask，不是 0600） | `the_record_round_trips_sorted_and_private`（mode 断言） | ✅ |

4/4 抓住。

## ADR 0025 / ADR 0028 R8：内建 skill + diagrams + mermaid_check（2026-10-15）

Base `cf37140` · 一次性 worktree `/tmp/radian-diag-mut`，每条跑完 `git checkout -- .`，`git status --short`
## ADR 0031 片 B：启动期提问、覆盖、审计（2026-10-14）

Base `b2c9c1c` · 一次性 worktree `/tmp/radian-trustb-mut`，每条跑完 `git checkout -- .`，`git status --short`
为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| DG-M1 | 内建 skill 排在用户级**之前**（内建先发现，用户级被它 shadow） | `skills::tests::a_user_skill_of_the_same_name_overrides_the_builtin` | ✅ |
| DG-M2 | `mermaid_check` 失败也返回 `ok: 0x0` | `mermaid::tests::each_failure_has_its_own_reason_and_a_hint`（另 2 条也挂） | ✅ |

2/2 抓住。
| T-B1 | 启动 modal 的默认高亮改成 `TrustHere`（回车即信任） | pty `pty_trust_modal_asks_and_enter_untrusts` | ✅ |
| T-B2 | `trust::load_for` 在命令行覆盖时也 `set_at` 写盘 | cli `trust_flags_override_without_writing` | ✅ |
| T-B3 | 非交互且无决策时把 decision 改成 `Trust` | cli `a_workspace_with_project_resources_is_untrusted_without_a_terminal`（含 status line 断言） | ✅ |
| T-B4 | `needs_ask` 去掉"来源是 Default"条件（有保存决定也问） | pty `pty_trust_override_and_saved_decision_skip_the_modal` | ✅ |

4/4 抓住。**过程记录**：T-B3 第一轮没抓住——当时 CLI 只在 `source == Default` 时打那一行，所以"默认但
decision=Trust"的变异仍打印解法；把条件改成 `!decision && source == Default` 并补 status line 断言后抓住。

## ADR 0031 片 B2：信任提问要求终端（2026-10-14）

Base `e9397be` · 一次性 worktree `/tmp/radian-trustb2-mut`，跑完 `git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T-B5 | `trust_interactive` 退回 `cli.prompt.is_none()`（无 `-p` 即提问，管道 stdin 也问） | cli `piped_stdin_is_never_read_as_the_trust_answer`（提问行出现、管道内容被当成回答、消息没到模型） | ✅ |

1/1 抓住。

## git-commit：宿主侧 `git_commit`（ADR 0030 R10，2026-10-16）

Base `a584b4b` · 一次性 worktree `/tmp/radian-mut-gitcommit`（`git worktree add --detach`，
共享 target 安全：radian-host 无 build.rs）· 每条跑完 `git checkout -- .`，`git status --short`
为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| GC-A | 去掉 `--no-verify` | `the_commit_command_never_runs_a_hook`（命令参数单测） | ✅ |
| GC-B | 去掉 `core.hooksPath=/dev/null` | `the_commit_command_never_runs_a_hook` | ✅ |
| GC-C | 去掉路径边界检查 | `outside_paths_are_refused` | ✅ |
| GC-D | 去掉保护分支检查 | `branch_rules_are_enforced` | ✅ |
| GC-E | 不写审计事件 | `the_commit_is_audited` | ✅ |

5/5 抓住。**过程记录**：GC-A/GC-B 第一轮**没抓住**——两者互为保险（只去一个，hook 仍然不会跑），
所以加了 `commit_args` 的参数单测分别断言两者，才各自抓住。GC-E 第一轮变异写错（在 GitCommitted
之前又插了一条 TurnStarted，没删掉 GitCommitted），改成真正删除后才由审计测试抓住。

## git-commit-fix：钉住仓库 + `core.fsmonitor=false`（ADR 0030 R10 安全修订，2026-10-16）

Base `60b1976` · 一次性 worktree `/tmp/radian-mut-gcfix` · 每条跑完 `git checkout -- .`，
`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| GCF-A | 去掉钉住检查（`pin.still`） | `a_git_dir_created_in_a_subdirectory_workspace_is_refused` | ✅ |
| GCF-B | 去掉 `-c core.fsmonitor=false` | `the_fsmonitor_defense_blocks_a_repo_config_script` | ✅ |

2/2 抓住。威胁测试 `the_fsmonitor_threat_is_real_without_the_defense` 是 canary：裸 `git add`
带 `core.fsmonitor` 会写标记（已在 git 2.53.0 上证明），证明这条防线不是多余的。

## git-commit-fix 审查：`core.hooksPath` 放公共参数（2026-10-16）

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| GCF-C | 从 `common_args` 去掉 `core.hooksPath=/dev/null`（`git add` 就不再带） | `every_git_call_disables_the_dangerous_repo_config` 与 `the_post_index_change_hook_does_not_run` | ✅ |

威胁 canary `the_post_index_change_threat_is_real_without_the_defense`：仓库配置
`core.hooksPath=.husky` + 工作区里会写标记的 `.husky/post-index-change`，裸 `git add` 会写标记
（git 2.53.0 实测）。`RepoPin::still` 的文件系统预检（新冒出的 `.git` 先于 `git rev-parse` 拒绝）
是纵深防御，去掉它行为上仍拒绝（rev-parse 会解析到新仓库而判定不同），所以没有对应变异。
## usage（用量行只跟着当前模型，ADR 0027）

Base `45723f6` · 一次性 worktree `/tmp/radian-glyphs-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| U-M1 | `spawn_gated` 关掉闸门时仍刷新（切走后不停刷新） | `tests/usage_gate.rs::the_usage_row_follows_the_current_model` | ✅ |
| U-M2 | `resetsAt` 改回 `Option<String>`（数字形式让整个响应解析失败、行消失） | `opencode_go::tests::resets_at_accepts_epoch_numbers` | ✅ |

2/2 抓住（U-M2 先试过只改字段类型，会编译失败 E0308，改成完整还原旧的 `Option<String>` + `countdown` 用法后才算数）。

## startup-line（摘要行写真实沙箱后端，2026-10-14）

Base `62ae29e` · 一次性 worktree `/tmp/radian-sl-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S-M2 | `sandbox_word` 的 workspace 分支写死 `seatbelt`（Linux 上是错的） | `describe::tests::the_summary_word_is_the_backend_it_is_given`（2026-10-15 改名/改写为 `the_summary_word_names_the_backend_in_plain_words`） | ✅ |
| S-M3 | 说明句改回 `**on**` | `describe::tests::the_workspace_facts_have_no_markdown` | ✅ |

2/2 抓住。

## herdr 上报（ADR 0034 切片 ①②）

Base `1b188c1` · 一次性 worktree `/tmp/radian-hd-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| HD1 | `state()` 去掉 `blocked` 优先（turn 先判） | `herdr::tests::blocked_wins_over_working` | ✅ |
| HD2 | `Seq::next` 直接返回 `now`（不保证递增） | `herdr::tests::seq_is_strictly_increasing` | ✅ |
| HD3 | `env_from` 去掉 `HERDR_ENV=1` 判定 | `pty_herdr_is_silent_without_the_env` | ✅ |
| HD4 | `Reporter::report` 在调用线程上同步发送 | `herdr::tests::a_silent_listener_does_not_block_the_caller` | ✅ |

4/4 抓住。切片 ② 的 pty 测试另用假 socket 断言 idle→working→idle→release、审批时 `blocked`
且 `message` 只有 `approval: shell`（不含命令原文）、无 `HERDR_ENV` 与 `RADIAN_HERDR=0` 时一条
都收不到。

## herdr 上报：shutdown 的丢唤醒（2026-10-15）

Base `f673ed7` · 一次性 detached worktree `/tmp/herdr-hang-mut`（跑完 `git worktree remove --force`）。
变异用 `--no-fail-fast`；HD6 是竞态，用负载循环判定（10 个 CPU burner + 25 s 超时，输出写文件）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| HD5 | `report` 只在 slot 为空时写入（保留最旧的一条） | `herdr::tests::the_slot_keeps_only_the_latest` | ✅ 抓住（断言 `the working report must have been replaced`：left `working`、right `blocked`） |
| HD6 | `stop_and_wake` 还原成不持 slot 锁的版本（原来的丢唤醒） | 负载下 herdr 测试循环 200 次 | ✅ 抓住：200 次里 **8 次挂起**（第 3/43/93/98/113/142/170/197 次，25 s 超时）；修复后 200 次 **0 挂起** |

HD6 不是单个测试能确定性抓住的（丢唤醒是竞态，窗口在「sender 检查 `stop`」与「进入
`Condvar::wait`」之间）；用 `sample` 抓到现场：测试线程停在 `Reporter::shutdown` 的 `join`
（herdr.rs:354），sender 线程停在 `Condvar::wait`（herdr.rs:374）。修复是 `stop_and_wake` 持
slot 锁设置 `stop` 并 `notify_all`，`shutdown` 与 `Drop` 都走它。
## brand-config（[tui] brand，ADR 0027，2026-10-15）

Base `4772113` · 一次性 worktree `/tmp/radian-bc-mut`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| B-M3 | 去掉 `tui.brand` 的 4 显示宽度检查 | `startup_pref_tests::tui_brand_is_carried_and_over_wide_is_refused` | ✅ |

1/1 抓住。
## context-files（ADR 0036 切片 ①，2026-10-14）

Base `1d4f6e9` · 一次性 worktree `/tmp/radian-cf-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| CF-M1 | 不在仓库根停，一直走到文件系统根（读到仓库外的文件） | `context::tests::discovery_stops_at_the_repository_root` / `a_workspace_outside_a_repository_reads_only_itself` | ✅ |
| CF-M2 | 段里去掉不可信说明 | `context::tests::the_global_file_is_first_and_a_path_is_read_once` | ✅ |
| CF-M3 | 单文件上限不生效（`limit = remaining`，65 KiB 不截断） | `context::tests::the_file_and_total_limits_truncate` | ✅ |
| CF-M4 | 全局文件完全不加载 | 同上 | ✅ |
| CF-M4b | 全局文件放到工作区之后（顺序错） | 同上 | ✅ |

5/5 抓住。

## 系统提示词分层与三份角色（ADR 0036/2，切片 ②）

Base `658b882` · 一次性 worktree `/tmp/radian-pl-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| PL1 | 排序去掉 `is_current`（当前 provider 不再在前） | `radian-tui` 的 `tests::the_flat_model_list_puts_the_current_provider_first` | ✅ |
| PL2 | `<session>`（0b）放到最前 | `skills_order::the_production_hook_chain_puts_memory_before_skills`（断言 session 在 skills 之后） | ✅ |
| PL3 | `assemble` 丢掉 `<runtime>`（0a） | `cli::system_prompt_is_sent`（`--system` 时 0a 仍在） | ✅ |
| PL4 | `default.md` 少一条规则（无每轮预算那条） | `prompt::tests::the_three_builtin_roles_are_distinct_and_carry_their_rules` | ✅ |
| PL5 | 超预算先丢**内层**（`raw.pop` 代替 `remove(0)`） | `context::tests::the_file_and_total_limits_truncate` | ✅ |
| PL6 | `git_commit` 放行 `.radian/` | `git_commit::tests::a_path_under_dot_radian_is_refused` | ✅ |

6/6 抓住。
## startup-speed（沙箱词改写为 "macOS sandbox" / "Linux sandbox"，2026-10-15）

工作区 `<home>/Code/radian-dogfood-ss`，分支 `dogfood/startup-speed`；每个变异改完从副本
还原并 `cmp` 确认。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SP-S1 | `sandbox_word` 的 workspace 分支改回直接返回后端名（`macos-seatbelt`） | `describe::tests::the_summary_word_names_the_backend_in_plain_words`；`radian-tui::tests::pty::pty_the_startup_summary_names_the_real_sandbox_backend` | ✅ |
| SP-S2 | `linux-bwrap` 的分支错写成 `"macOS sandbox"`（把平台的词写死成另一个平台） | `describe::tests::the_summary_word_names_the_backend_in_plain_words` | ✅ |

2/2 抓住。SP-S2 在 macOS 上运行时 pty 用例抓不住（这台机器只会走 `macos-seatbelt` 分支），
所以词组按平台分别断言在 `describe` 的单测里（同一测试也覆盖未知后端按真名显示）。

## context-escape（上下文文件闭合标签/属性转义，ADR 0036 C2 补充，2026-10-15）

Base `65ea1ed` · 一次性 worktree `/tmp/radian-ce-mut`，还原后 `git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| CE-M1 | 内容不转义（`close_tag` 直接返回原文） | `context::tests::a_content_closing_tag_is_escaped` | ✅ |
| CE-M2 | `path` 属性不转义 | `context::tests::a_path_with_quotes_cannot_end_the_attribute` | ✅ |
| CE-M3 | 转义大小写敏感（只查小写） | `context::tests::a_content_closing_tag_is_escaped`（大写变体未转义） | ✅ |

3/3 抓住。

## small-fixes-2：上下文文件去重

Base `e157bfb` · 一次性 worktree `.radian-worktrees/sf2-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SF1 | `load` 去掉 `seen` 去重 | `context::tests::a_symlinked_context_file_is_read_once` | ✅ |
## 分档定价（研究 F5）

Base `ba964a4` · 一次性 worktree `.radian-worktrees/tp-mut`（detached，跑完删除）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| TP1 | `rates_for` 恒用基础档（忽略 tiers） | `cost::tests::a_context_tier_prices_the_whole_call_above_its_threshold` | ✅ |
| TP3 | 边界用 `>=` 而不是 `>` | 同上（「正好在阈值仍是基础档」那条断言） | ✅ |

TP2（「阈值用会话累计」）在当前结构里不可表达：`cost_micros`/`rates_for` 是纯函数，只拿到
**本次调用**的 `ModelUsage`，没有会话累计可拿；`restore_usage` 也是逐条记录调用 `cost_micros`。
这正是研究 F5 要的语义（阈值比的是这次请求自己的输入长度），所以没有对应变异。
## review-fixes-1 (2026-10-15): git filters + corrupt trust

One-shot detached worktree `.radian-worktrees/rf1-mutations` (from the branch HEAD, deleted
## ADR 0014 reasoning replay (2026-10-15)

One-shot detached worktree `.radian-worktrees/rr-mutations` (from the branch HEAD, deleted
after); each mutation restored with `git checkout --`, `git status --short` empty at the end.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RF1-FA-A | skip the filter check entirely (`check_filters` returns `Ok`) | caught (rc=101) | `a_filter_driver_not_in_allowed_filters_is_refused` (the marker was created) |
| RF1-FA-B | allow every driver (drop the `allowed_filters` membership check) | caught (rc=101) | `a_filter_driver_not_in_allowed_filters_is_refused` |
| RF1-FA-C | treat `lfs` with a non-`git-lfs` command as allowed | caught (rc=101) | `lfs_is_allowed_only_with_a_git_lfs_command` |
| RF1-FC | `read_at(path).unwrap_or_default()` in `set_at` again | caught (rc=101) | `trust::tests::a_corrupt_record_is_not_overwritten` |
| RR-D | treat `reasoning_details` as a replayable text field (add it to `REPLAYABLE_INTERLEAVED`) | caught (rc=101) | `catalog::tests::the_catalog_interleaved_field_decides_reasoning_replay` |
| RR-F | ignore a named provider's `reasoning_replay` override (fall through to the catalog) | caught (rc=101) | `the_catalog_and_a_named_override_decide_reasoning_replay` |

## review-fixes-1b (2026-10-15): the filter check must list files, not directories

One-shot detached worktree `.radian-worktrees/rf1b-mutations` (from the branch HEAD, deleted
after); each mutation restored with `git checkout --`, `git status --short` empty at the end.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RF1-FA-D | back to `git status --porcelain -z` (a new directory shows as `newdir/`) | caught (rc=101) | `a_new_file_in_a_new_directory_is_checked`, `status_show_untracked_files_no_does_not_hide_the_check` |
| RF1-FA-E | check the explicit directory path instead of expanding it (`ls-files`) | caught (rc=101) | `a_directory_argument_is_expanded_for_the_filter_check` |

## review-fixes-1c (2026-10-15): check-attr reads paths from stdin

One-shot detached worktree `.radian-worktrees/rf1c-mutations` (from the branch HEAD, deleted
after); the mutation was restored with `git checkout --`, `git status --short` empty at the end.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RF1-FA-F | pass the path list to `git check-attr` as argv again | caught (rc=101, `Argument list too long`) | `a_large_repository_does_not_hit_the_argument_limit` |
## bg-jobs-2：job 通知的 lane、唤醒上限与孤儿扫描（ADR 0037 切片 3-4，2026-10-16）

Base `eb7ec96` · 一次性 detached worktree `/tmp/bg2-mut`（跑完 `git worktree remove --force`）。
变异用 `cargo test -p radian-host --test job_notify --no-fail-fast -- <test> --exact`，还原后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| N1 | busy/idle 两条 lane 对调（busy 唤醒、idle 注入） | `busy_injects_and_idle_wakes` | ✅ |
| N3 | 唤醒上限不检查（`< u32::MAX`） | `the_wake_cap_degrades_to_inject_and_a_user_message_resets_it` | ✅ |
| N4 | `orphan_jobs` 去掉 settled 过滤（已结束的 job 被当成孤儿上报） | `orphan_jobs_reports_a_start_without_a_settle` | ✅ |

3/3 抓住。N4 是「孤儿被重新上报/重跑」这条的最近可测变异：设计上恢复只**上报一次**、绝不重跑
（ADR 0037 决定 6），代码里没有重跑路径，所以没有「真的重跑」的单行变异。
## switch-replay: the `/model` switch path carries the reasoning replay (2026-10-15)

Worktree `.radian-worktrees/ab3-radian`, branch `dogfood/ab3-switch-replay` (base `a39738f`).
Each mutation was made in place, restored from a copy in `/tmp` and compared with `cmp`
(identical); `git status --short` then showed only the two new test files. Every run used
`--no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| SR-S1 | `RouteContext::args_for` (the `/model` switch path, `src/model.rs:480`) builds `reasoning_replay` as `None` instead of `resolve_reasoning_replay(&provider_ref, model)` | caught (rc=101, **exactly one** failure in `-p radian-host --no-fail-fast`) | `switch_reasoning_replay::the_switch_path_carries_the_reasoning_replay` — `left: None, right: Some("reasoning_content")` for `opencode-go/deepseek-v4.1-flash` |
| SR-S2 | `route_replays_reasoning` returns `false` (the switched route's budget no longer counts reasoning) | caught (rc=101) | `switch_reasoning_budget::a_switched_route_counts_reasoning_exactly_when_it_replays_it`; `switch_reasoning_replay::the_switch_path_carries_the_reasoning_replay` (the "budget's answer matches the request's field" assertion); and the pre-existing `reasoning_replay::the_catalog_and_a_named_override_decide_reasoning_replay` |

SR-S1 is the hole this task closed. Under it nothing else failed: the rest of `radian-host`
(276 lib tests and 15 other integration files, 87 tests), `radian-tui`'s
`tests/model_switch.rs` (16 tests) and the whole `radian-tui`/`agent-cli` suites stayed green.
(`-p radian-tui -p agent-cli` needs `cargo build -p agent-extensions --bin mcp-fixture` first;
without it 11 tests fail on "mcp-fixture is not built", which is unrelated to this decision —
they were re-run green after the fixture was built.) SR-S2 was already caught by
`reasoning_replay.rs`; the two new tests pin the same decision on the switch path, where
`args_for` and the budget resolver are two separate resolutions that must agree.

## access-cmd：`set_access` 的沙箱与探测失败（ADR 0038 切片 ②，2026-10-15）

Base `d8b7688`（worktree `.radian-worktrees/access-cmd`）· 就地变异后跑
`cargo test -p radian-host --test access_switch --no-fail-fast`，再从 `/tmp/access-mut/session.rs.bak`
还原并用 `diff -q` 确认逐字节相同。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| AS-M2 | `set_access` 只换 profile、不 `sandbox_slot.set(parts.sandbox)`（权限变了、沙箱没变） | `access_switch::set_access_swaps_the_profile_and_queues_the_change`（`sandbox_mode()` 断言：切换后仍是旧沙箱） | ✅ |
| AS-M5 | `set_access` 忽略 builder 的 `Err`（`let Ok(parts) = … else { return Ok(()) }`，探测失败静默不切而不是拒绝） | `access_switch::a_failed_probe_refuses_and_changes_nothing`（`expect_err` 失败） | ✅ |

2/2 抓住。AS-M2 是"切换只作用于权限、不作用于沙箱"的最近可测变异；沙箱本身的行为由
`radian-coding` 的 `shell::a_swapped_slot_changes_the_next_call` 与
`jobs::a_running_background_job_keeps_the_sandbox_it_started_with` 覆盖。
## memory-scope: id commands find the record's scope themselves (ADR 0021 D1)

Base `21bf74b` · in-place in this worktree, each mutation applied from a copy and restored from
it afterwards (`md5` compared); `cargo test -p radian-host --lib --no-fail-fast memory::commands`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | `resolve_write_scope` auto branch returns `WriteScope::Project` (the old behaviour) | ✅ caught | `memory::commands::tests::{forget_finds_a_global_record_in_an_unlinked_workspace, edit_finds_a_global_record_in_an_unlinked_workspace, archive_and_restore_find_a_global_record_in_an_unlinked_workspace, confirm_finds_a_global_record_in_an_unlinked_workspace, an_unknown_id_is_a_clear_error}` |
| M2 | `resolve_read_scope` auto branch returns `ScopeSelection::ProjectOnly` | ✅ caught | `memory::commands::tests::{show_finds_a_global_record_in_an_unlinked_workspace, an_unknown_id_is_a_clear_error}` |
| M3 | `MemoryService::locate` only searches Global (skips Project) | ✅ caught | `memory::commands::tests::{a_linked_workspace_acts_on_the_project_record, an_unconfirmed_forget_writes_nothing}` |
| M4 | the auto not-found path returns the store message (`no record <id>`) instead of `no memory record <id>` | ✅ caught | `memory::commands::tests::an_unknown_id_is_a_clear_error` |
| M5 | `resolve_write_scope` ignores an explicit `--global` (always locates) | ✅ caught | `memory::commands::tests::an_explicit_scope_never_crosses_to_the_other_scope` |
| M6 | `resolve_read_scope` ignores explicit `--global`/`--project` (always locates) | ✅ caught | `memory::commands::tests::an_explicit_scope_never_crosses_to_the_other_scope` |

6/6 caught. `show` already read Global through the default Project→Global view before this change,
so its test (`show_finds_a_global_record_in_an_unlinked_workspace`) is a regression guard, not a
red-on-old-code test; the new not-found wording is what M4 reds on the old code for `show`.
(Follow-up `memory-scope-wording` below moves that wording into `MemoryError::NotFound`, so the
store message M4 mutated is the `no memory record <id>` wording itself now.)

## memory-scope-wording: one not-found message for an unknown id (ADR 0021 D1 follow-up)

Base `083c6c8` · in-place in this worktree, the mutation applied from a copy and restored from it
afterwards (`md5` compared, `git status --short` empty), `cargo test -p radian-host --lib
--no-fail-fast memory::`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| W1 | `MemoryError::NotFound`'s wording goes back to `no record <id>` | ✅ caught | `memory::commands::tests::an_unknown_id_is_a_clear_error`, `memory::tools::tests::an_unknown_memory_get_uses_the_stores_not_found_wording` |

## memory-project-auto-create: ADR 0021 (decisions 1 and 2)

Base `6f2a08b` · in-place in this worktree, each mutation applied with `python3` from a copy and
restored from it afterwards (`md5` compared, `git status --short` empty), `cargo test -p
radian-host --lib --no-fail-fast memory::` (AC-1 additionally: `cargo test -p agent-cli --test
cli -- memory` after `cargo build -p radian-tui --bin radian`, and `cargo test -p radian-tui
--test pty -- pty_a_memory_add`).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| AC-1 | `project_for_write` never registers: a git workspace with an unknown key gets the `Unlinked` error (the pre-revision behaviour) | ✅ caught | `memory::service::tests::{a_write_registers_a_git_workspace_and_a_read_never_does, a_write_outside_a_repository_is_refused_and_creates_nothing, a_write_still_creates_when_a_candidate_previous_project_exists}`, `memory::tools::tests::{a_save_in_a_git_workspace_registers_the_project_and_notices_once, a_save_outside_a_project_workspace_is_refused_without_suggesting_global}`, and end to end `agent_cli::cli::{memory_add_registers_a_git_workspace_and_prints_one_notice, memory_save_in_a_git_workspace_creates_the_project_and_exec_shows_the_notice, memory_unlinked_workspace_refuses_project_writes_and_lists_global}`, `radian_tui::pty::pty_a_memory_add_shows_the_created_project_notice` |
| AC-2 | `MemoryService::state` registers a git workspace itself, so every **read** creates a project | ✅ caught | `memory::tools::tests::a_read_never_registers_a_project`, `memory::service::tests::{a_write_registers_a_git_workspace_and_a_read_never_does, a_write_still_creates_when_a_candidate_previous_project_exists}`, `memory::tools::tests::a_save_in_a_git_workspace_registers_the_project_and_notices_once` |
| AC-3 | `project_for_write` drops the git check (registers any unregistered directory, `/tmp` included) | ✅ caught | `memory::service::tests::a_write_outside_a_repository_is_refused_and_creates_nothing`, `memory::tools::tests::a_save_outside_a_project_workspace_is_refused_without_suggesting_global` |
| AC-4 | the `memory_unlinked` tool error says "Save to the global scope instead" again | ✅ caught | `memory::tools::tests::the_unlinked_tool_error_never_suggests_global` |
| AC-5 | the `memory_save` description drops the global rule | ✅ caught | `memory::tools::tests::the_save_description_states_when_global_is_right` |
| AC-6 | the not-a-project-workspace refusal suggests the global scope | ✅ caught | `memory::tools::tests::a_save_outside_a_project_workspace_is_refused_without_suggesting_global` |

6/6 caught. In each run only the tests listed above failed (the rest of the `memory::` filter
stayed green); AC-1 also reds the three CLI tests and the pty test above, which is what the two
end-to-end tests are for.

## resume-perf2: the resume history is the driver's recovered fold (2026-10-16)

Base `oc/resume-perf2` · in-place, each mutation copied to `.radian/mut/` first and restored from
the copy afterwards (`diff` compared, `git status --short` empty), `--no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RH-A | `recovery_parse` forced `None`, so the driver parses the journal again (the pre-resume-perf-B second read) | ✅ caught (open count 2, expected 1) | `a_resume_parses_the_journal_once` |
| RH-B | the host does not take the driver's recovered history (`recovered_history = Vec::new()`) | ✅ caught (host history `[]`, disk fold non-empty) | `a_resumed_history_matches_the_disk_fold_through_compaction_and_replacement` |

2/2 caught. The two mutations are the two halves of the same promise: RH-A pins that the resume
parses the journal **once** (B's counter test), RH-B pins that the host's initial view is exactly
that parse's fold, on a fixture with a committed compaction replacement and a pending one at end
of file (the test this slice adds).
## cost-marks: ADR 0027 / ADR 0007 (incomplete fee marked, each call's fee recorded)

Base `df6856d` · in-place in this worktree, each mutation applied with `python3` from a copy and
restored from it afterwards (`cmp` compared identical, `git status --short` back to the same
set), `cargo test -p radian-host --locked` and the named tests of the other crates, no
`--no-fail-fast` needed for these single-target runs.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | `cost_micros` drops the `incomplete` mark (a non-zero counter with no rate is silently skipped) | ✅ caught (rc=101) | `cost::tests::a_non_zero_counter_with_no_rate_marks_the_known_sum_incomplete`, `cost::tests::an_incomplete_call_makes_the_running_total_incomplete`, `footer::an_incomplete_fee_is_marked` |
| M2 | `restore_usage` ignores the recorded fee and re-prices every record (the pre-revision-12 behaviour) | ✅ caught (rc=101) | `a_recorded_fee_is_restored_as_it_stands_when_the_catalog_moves`, `a_recorded_incomplete_fee_keeps_the_total_incomplete` |
| M3 | `SessionProjection::add_usage_at` does not set `cost_estimated` (old records are not marked `~`) | ✅ caught (rc=101) | `an_old_record_without_a_fee_is_repriced_and_marks_the_total_an_estimate`, `footer::an_estimated_fee_shows_the_tilde_and_wins_over_incomplete` |
| M4 | `parse_tiers` sorts **descending** (`Reverse`), the tier hole research F5 left open | ✅ caught (rc=101) | `catalog::tests::the_tiers_are_parsed_ascending_whatever_the_catalog_order` |
| M5 | the writer never computes the fee for a usage record (`set_usage_fee` becomes inert) | ✅ caught (rc=101) | `write::a_usage_record_carries_the_fee_the_writer_computed` |

5/5 caught. In each run only the listed tests failed; the rest of the target stayed green.
## ADR 0021 + ADR 0036：固定段、追加消息、`<session>`（2026-10-15）

Base `b63ac20`（本 worktree，dogfood/memory-r2）· 变异直接做在本 worktree，每个变异前把文件
复制到 `/tmp/r2-mut*/`，跑完后从副本还原并 `diff -q` 确认一致；`git status --short` 在跑完后只剩
本片的正常改动。每个变异用 `--no-fail-fast`；动到宿主代码的那几条先
`cargo build -p radian-tui --bin radian`（agent-cli 的测试会拒绝过期的二进制）。

**第一次跑法有错**（把 `--no-fail-fast` 放在 `--` 之后当成了过滤器，M1 因此看起来"被抓住"），
下表是改正后重跑的结果：每条都给出真正失败的测试名与 panic 行。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 固定段的查询文本改回**最后一条**用户消息，改在 `MemoryHooks::before_model_call` 的 `.fixed(...)` 里 | `radian_host/tests/memory_injection.rs::the_hook_orders_the_block_by_the_first_user_message` | ✅ caught |
| M1b | 同一变异，改在追加接缝的查询回退（`addition_for_turn` 里的 `first_user_text(transcript)`） | `agent_cli::cli::{a_resumed_session_ranks_the_block_by_its_first_user_message, a_forked_child_ranks_the_block_by_the_parents_first_user_message}` | ✅ caught |
| M2 | 驱动器用 `InputSource::User` 注入追加消息 | `radian_runtime::tests/driver.rs::a_host_addition_lands_after_the_user_message_and_before_the_first_call` | ✅ caught |
| M3 | 追加消息改到 `drain_steering` **之后**注入（晚一步到） | 同上（transcript 顺序：追加消息不再紧跟用户消息） | ✅ caught |
| M4 | 追加候选忽略去重账本（`exclude` 为空） | `radian_host/tests/memory_injection.rs::a_later_turn_appends_its_new_hits_as_one_system_message` | ✅ caught |
| M5 | 固定段每次调用重算（去掉 `fixed` 的缓存短路） | 宿主：`memory_injection.rs::{the_block_reaches_the_system_prompt_and_the_event_is_written_once, a_later_turn_appends_its_new_hits_as_one_system_message, an_addition_budget_drops_whole_records_and_additions_can_be_off}`；CLI：`the_memory_block_is_fixed_per_process_and_a_new_record_is_appended_once` | ✅ caught |
| M6 | `<session>` 每次请求重算（去掉 key 比较） | `agent_cli::cli::the_memory_block_is_fixed_per_process_and_a_new_record_is_appended_once`（write 之后 `git:` 行从 `(clean)` 变成未提交） | ✅ caught |
| M7 | `/model` 后 `<session>` 不重算（重算分支改成 `let _ = key;`） | `radian_host::prompt::tests::the_session_layer_changes_only_when_the_model_route_does` | ✅ caught |

8/8 caught。M1 与 M1b 是同一个行为的两条代码路径，**必须分开记**：正常一轮里追加接缝先算固定段
（M1b 那条路径），`MemoryHooks` 自己的查询只在「接缝没算」（`additions = false`，或进程的第一轮不是
用户轮次）时才生效（M1 那条路径）；只做其中一个变异，另一个不会被任何测试发现。

两条**等价变异**（改掉也不会被任何测试区分）如实记录：

- **追加消息自身的 `meta.source = System`**：`Inbox::inject` 会用它自己的 `source` 参数覆盖
  `meta.source`（ADR 0011），所以把 `addition_for_turn` 里那行改成 `User` 没有任何可观察差异。
  可观察的那一半在驱动器的 `inject(message, InputSource::System)`，即 M2。
- **`fixed()` 查询回退的具体值**（用户轮次开始时用本轮文本）：在 CLI 端到端与单元测试里，第一条用户
  消息总是在 transcript 里（resume/fork）或就是本轮消息（首轮），两条路给出同一个查询文本，所以把
  回退值改成 `String::new()` 只有在一个「transcript 里根本没有用户消息、而且这是新会话」的构造下才
  可区分，本片没有这样的用例。

## mode-cmd：`set_mode` 的入队、journal 与角色层（ADR 0038 切片 ③，2026-10-15）

Base `e19e6cf`（worktree `.radian-worktrees/mode-cmd`）· 每个变异就地改完后跑
`cargo test -p radian-host --lib --test mode_switch --no-fail-fast`，再从 `/tmp/mut/` 的副本还原
并用 `diff -q` 确认逐字节相同。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| MODE-M1 | `RoleHooks` 的 mode 来源从 config slot 改成读 **projection**（`set_mode` 立刻写它）：切换于是同一次 turn 的下一个 model 调用就生效 | `mode_switch::a_switch_inside_a_running_turn_changes_nothing_until_the_next_turn`（turn 1 的第二次 model 调用变成 lead 角色） | ✅ |
| MODE-M2 | `set_mode` 不再 push `{"mode": …}`（只改 projection：切了、没记） | `mode_switch::{set_mode_queues_the_change_and_the_mode_in_force_waits_for_the_turn, the_role_layer_switches_at_the_next_turn_and_no_other_layer_moves, a_resumed_session_runs_the_mode_the_journal_recorded, a_switch_inside_a_running_turn_changes_nothing_until_the_next_turn}`（4 条 FAILED） | ✅ |
| MODE-M3 | 重建 base 时把第 2 层换成第 0a 层的字节（`assemble(..., &self.runtime)`，即切换时另一层的字节跟着动） | `prompt::tests::the_role_hook_moves_layer_one_only_when_the_mode_changes`、`mode_switch::{the_role_layer_switches_at_the_next_turn_and_no_other_layer_moves, a_resumed_session_runs_the_mode_the_journal_recorded}` | ✅ |

3/3 抓住。MODE-M2 的 4 条里最强的是 resume 那条：journal 里没有 `{"mode":"lead"}`，新进程第一次请求
就还是 general。

**一条等价变异（如实记录）**：`set_mode` 里删掉 `self.facts_hooks.refresh()`（`/mode` 的 `<session>`
那一半）没有任何测试能区分——`<session>` 段里不写 mode，重算与不重算的字节相同（只有 date/git 这类
活读在两次计算之间变了才有差别，本片的用例都在同一个工作区、同一天、不是 git 仓库）。保留这行是因为
ADR 0036 明说 `/mode` 与 `/model` 一样重算 0b；它是那个规则的实现，不是可测行为。

## orch-s4：task 会话生命周期与 Q8（ADR 0030 切片 4，2026-10-16）

工作树 `.radian-worktrees/orch-s4`。每个变异就地施加，用 `.radian/scratch/` 里的副本还原，
`cmp` 证明与副本一致、`git status --short` 只留下新测试文件；每个变异用
`cargo test -p radian-host --test task <测试> --no-fail-fast`（git_commit 的两条用 `--test git_commit`）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T-S4-A | `task_sandbox_policy` 把 `network` 留在 `true`（沙箱仍联网） | `the_task_sandbox_turns_the_network_off`（真实外连成功） | ✅ grabbed (rc=101) |
| T-S4-B | `open_session` 不再调 `with_allowed_branch`（task 会话可提交到任意分支） | `a_task_git_commit_is_limited_to_its_branch`（模型在 `other` 分支上调 `git_commit`，本应拒绝） | ✅ grabbed (rc=101) |
| T-S4-C | `end_of_task_commit` 恒返回 `Ok(None)`（收尾提交不再提交，工作树留脏） | `the_end_of_task_commit_cleans_the_worktree` | ✅ grabbed (rc=101) |
| T-S4-D | task 的会话根用 lead 工作区而不是 worktree | `a_task_opens_in_a_worktree_on_its_branch_with_a_task_parent`（`SessionCreated.workspace` 与 worktree 不符） | ✅ grabbed (rc=101) |
| T-S4-E | `commit()` 的 `main`/`master` 拒绝对所有会话重新生效（忽略 `refuse_main`） | `a_general_session_commits_on_main_through_the_session`（general 会话经模型工具在 `main` 提交） | ✅ grabbed (rc=101) |
| T-S4-F | task 会话的 `with_refuse_main(false)`（task agent 可在 `main` 提交） | `a_task_agent_session_refuses_a_commit_on_main`（task_agent=true、无 allowed_branch、在 `main` 上） | ✅ grabbed (rc=101) |
| T-S4-G | 收尾提交把 shared 任务也算进（finisher 拿到 lead 工作区与可解析的 pin） | `a_shared_task_writes_no_end_commit`（lead 工作区有未提交改动，错误提交会把用户改动提交掉） | ✅ grabbed (rc=101) |

7/7 全抓。

**T-S4-F 与 allowed_branch 的关系（如实记录）**：registry 起出来的 task 会话总有 `allowed_branch =
Some("radian/<id>")`，那种情况下 `refuse_main=false` 与 `true` **不可区分**（两者都拒绝 `main`，只是
错误码不同）。所以抓住这条变异必须用 registry 不产生的组合：`open_session(task_agent=true,
allowed_branch=None)` 在 `main` 上，此时只有 `refuse_main` 在起作用（对应 Q8 的「task agent 一律
拒绝 main/protected」）。`a_task_git_commit_is_limited_to_its_branch` 覆盖的是另一条：allowed_branch
这条线本身。

**T-S4-G 的时序**：正的收尾提交（`the_turn_end_commits_the_worktree`）轮询提交出现；反的
（`a_shared_task_writes_no_end_commit`）先等任务 journal 里出现 `TurnFinished`，再给错误的 finisher
一个有界的窗口（2 s）去提交，然后断言 lead 工作区未变——纯 `requests()` 计数会在 finisher 运行前
就通过（假绿）。

## orch-s5：task 三层审批与 task 侧待决队列（ADR 0030 切片 5，2026-10-16）

工作树 `.radian-worktrees/orch-s5`。每个变异就地施加，改前把源文件复制到
`.radian/scratch/mut-backup/`，改后用 `cp` 还原并以 `cmp` 证明一致（本任务禁 `git checkout --`）；
结束后 `git status --short` 只剩本片的改动。每个变异：
`timeout 240 cargo test -p radian-host --test task_pending <测试> --locked --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T-S5-A | `TaskApprover` 把 edge policy 的 `ask` 直接当 allow（策略说「问」= 自动批） | `a_policy_ask_lands_in_the_pending_queue_not_auto`（20 s 内有界等待等不到待决事件 → panic） | ✅ GRABBED (rc=101) |
| T-S5-B | `escalate_approval` 立即回 `AllowOnce`（lead 代人批准） | `escalate_reaches_the_human_queue_with_the_summary`（`yield` 后 task 已结束；且人类否决的答案没到达 task） | ✅ GRABBED (rc=101) |
| T-S5-C | R9 超时在事件**创建**时起算（把 `start_timeout` 从 `next` 移到 `approval` 的 push） | `the_timeout_starts_at_take_and_resolves_as_deny`（take 之前 advance 120 s，task 已被拒绝） | ✅ GRABBED (rc=101) |
| T-S5-D | task 的事件写进 lead 的 inbox（`spawn` 里给 lead journal 追加一条 `InboxEnqueued{source: Subagent}`） | `a_task_ask_blocks_on_the_pending_queue_and_never_touches_the_lead_inbox`（lead journal 出现 `InboxEnqueued`/`subagent`） | ✅ GRABBED (rc=101) |
| T-S5-E | `PermissionHooks.task_approver` 永不置位（task 的非人类 `ask` 在 hook 里先被拒，approver 收不到） | `a_task_ask_blocks_on_the_pending_queue_and_never_touches_the_lead_inbox`（等不到待决事件） | ✅ GRABBED (rc=101) |

5/5 抓住。硬禁止清单那条变异改的是 `radian-permissions`，记在
`crates/radian-permissions/MUTATIONS.md`（PM-O2）。

**T-S5-E 与修订 2 的冲突（如实记录）**：task 的 `on-ask` 与 edge policy 能到达 `TaskApprover`，
前提是 `PermissionHooks` 上新增的 `task_approver` 标志（`set_task_approver`，默认 `false`）。
hook 原本在咨询 approver **之前**就拒绝非人类 `ask`（`hooks.rs` 的
`"this turn is not from the user, so no approval can be obtained"`），而 task 的 turn 来源是
`Subagent`/未设，所以没有这个标志，三层里的第二、三层永远跑不到。ADR 0030 说
`radian-permissions` 不改、`PermissionHooks` 判定顺序照旧，与切片 5 要求的 `Approver` 冲突；
冲突写在 `.radian/progress/orch-s5.md`。该标志只对 task 路由生效，task 仍不能借用人类
session grant，其他非人类来源照旧在 hook 里被拒。

## orch-s5 review fix 1：edge policy 命令不得无沙箱运行（2026-10-16）

同一工作树。改前把 `src/task_pending.rs` 复制到 `.radian/scratch/mut-backup/`，改后 `cp` 还原
并以 `cmp` 证明一致。命令：
`timeout 240 cargo test -p radian-host --test task_pending a_policy_command_with_no_sandbox_is_skipped_and_never_runs --locked --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| T-S5-F | `run_policy` 恢复「无 sandbox 就直接 `std::process::Command` 在宿主上跑」的旧分支 | `a_policy_command_with_no_sandbox_is_skipped_and_never_runs`（标记文件出现且请求被 allow，不再是待决事件） | ✅ GRABBED (rc=101) |

1/1 抓住。该测试先于修复写就、在旧分支上失败（标记文件被创建）；修复后无 sandbox 直接回
`Ask`，命令不执行，请求进待决队列，reason 记「the edge policy was skipped: this task has no
sandbox」。`run_policy` 里已不存在无沙箱分支。
## sec-git：ADR 0039 决定 3（宿主不执行任何仓库配置的 filter 命令，2026-10-16）

工作树 `.radian-worktrees/sec-git`，基线 `11e4b01`。变异就地施加，用 `.radian/scratch/mut/` 的
副本还原（每个变异后 `cmp` 证明与副本一致）；每个变异用
`cargo test -p radian-host --test git_commit --no-fail-fast -- <过滤>` 与
`cargo test -p radian-host --test task --no-fail-fast -- creating_and_removing_a_worktree_runs_no_filter`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SG-M1 | 暂存退回 `git add -A` / `git add -- <paths>`（宿主的 `git add` 执行仓库 filter） | `lfs_runs_radians_own_command_and_never_the_configured_one`（配置的 `filter.lfs.clean` 脚本被 `git add` 跑了） | ✅ 1 FAILED |
| SG-M2 | `lfs` 从仓库配置读 `filter.lfs.clean` 命令串并用 `sh -c` 运行，而不是 Radian 自己的固定 `git-lfs clean -- <path>` | `lfs_runs_radians_own_command_and_never_the_configured_one`（配置的脚本跑了，提交的是脚本输出而不是固定命令的指针） | ✅ 1 FAILED |
| SG-M3 | `dirty()` 与 task 的 `create_worktree`/`remove_worktree` 去掉 no-filter 覆盖（`git status` / `git worktree add\|remove` 直接跑 filter） | `a_driver_in_info_attributes_never_runs`、`a_driver_in_core_attributes_file_never_runs`（`dirty` 跑了 clean），`creating_and_removing_a_worktree_runs_no_filter`（`worktree add` 跑了 smudge） | ✅ 3 FAILED |

3/3 全抓。SG-M1 由 lfs 那条确定性抓住：`git add` 对 `filter=lfs` 的文件会执行仓库配置里的
命令，而 lfs 是唯一「被允许、因而不会被 `plan_path` 提前拒绝」的驱动，所以「退回 git add」在这条
路径上必然跑配置命令。`a_gitattributes_flip_during_commit_never_runs_a_filter` 是并发断言
（断言「标记永不出现」），在 SG-M1 下 8 次提交里没有恰好撞上 check-attr 与 git add 之间的窗口，
因此不算 SG-M1 的抓取测试——确定性的抓取是 lfs 那条。

**如实记录（未做变异）**：`GIT_ATTR_SOURCE=<空树>` 不足以关掉 `$GIT_DIR/info/attributes` 与
`core.attributesFile`（实测见 `.radian/scratch/investigate3.out`），所以实现用「从生效配置枚举
filter 名字 + 对每个名字 `-c filter.<name>.{clean,smudge,process}=` 与 `required=false`」。这一层
的测试是 `a_driver_in_info_attributes_never_runs` 与 `a_driver_in_core_attributes_file_never_runs`；
SG-M3 证明它们确实区分「关掉命令」与「没关」。

## sec-git 复核修订 1：每个宿主 git 调用都走共享 helper（2026-10-16）

工作树 `.radian-worktrees/sec-git`，基线 `c33feca`。变异就地施加，用 `.radian/scratch/mut2/`
的副本还原（每个变异后 `cmp` 证明与副本一致）；每个变异跑对应的测试。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SG-F1-M1 | 页脚的 `background_git` 退回裸 `Command::new("git") -C dir`（丢掉 no-filter 覆盖） | `contrib::tests::the_git_contributor_never_runs_a_repository_filter`（页脚读跑了 clean filter）；守卫 `every_host_git_call_is_built_from_the_shared_helper`（`contrib.rs` 出现生产裸 git 调用） | ✅ 2 FAILED |
| SG-F1-M2 | `radian-coding` 的 `git_status`/`git_diff` 退回裸 git（丢掉覆盖） | `the_git_tools_never_run_a_repository_filter`（工具跑了 clean filter） | ✅ 1 FAILED |
| SG-F1-M3 | gitlink 用 100644 而不是 160000 暂存 | `a_moved_submodule_is_staged_as_a_gitlink_without_running_a_filter`（模式不对） | ✅ 1 FAILED |

3/3 全抓。守卫测试（`crates/radian-coding/tests/host_git.rs`）遍历 `crates/*/src` 里 `#[cfg(test)]`
之前的部分，`Command::new("git")` 只允许出现在 helper 自己（`host_git.rs`）与沙箱测试 harness
（`radian-sandbox/src/harness.rs`，其唯一的裸 git 只建测试仓库）；新增一个生产裸 git 调用会让它
失败，SG-F1-M1 的守卫那条就是证明。

**如实记录（测试的触发条件）**：`git status` 只在**大小相同**的 stat-dirty 文件上才哈希内容、
从而跑 clean filter；内容大小不同时 git 直接按 stat 判为改动，不跑 filter。所以三个行为测试都用
「与 index blob 同大小」的改动（`one\n` → `two\n`）。第一次写测试时用了不同大小（`one\ntwo\n`），
SG-F1-M1 的行为测试假绿（只有守卫抓住）；改成同大小后行为测试也抓住。
## sec-task：ADR 0039 决定 4/5/7（task worktree 出工作区、spawn 事务、可等待的收尾提交，2026-10-16）

工作树 `.radian-worktrees/build-script`，分支 `dogfood/sec-task`（基线 `origin/main` f72097d；
变异时 HEAD `f5d5ee38`）。先 `cp` 一份 `crates/radian-host/src/task.rs` 到 `.radian/scratch/`，
每个变异就地施加后跑一条测试，再 `cp` 还原，最后 `shasum -a 256 -c` 证明与副本逐字节一致。
命令：`cargo test -p radian-host --test task <测试> --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S4-1 | `task_worktree_root` 把根放回工作区内（`<ws>/.radian-worktrees`，`home` 也改成工作区） | `the_task_worktree_is_outside_the_workspace_on_a_per_repository_root` | ✅ 抓住 (rc=101) |
| S4-2 | 新根的每一级都不加 `O_NOFOLLOW`（`open_dir_at` 跟随 symlink） | `a_symlink_under_the_new_root_is_refused` | ✅ 抓住 (rc=101) |
| S4-3 | spawn 的错误路径不调 `guard.rollback()`（worktree 与 driver 都留下） | `a_failed_spawn_leaves_nothing_behind_and_pairs_a_task_finished` | ✅ 抓住 (rc=101) |
| S4-4 | 在 `open_session` 之后就 `guard.disarm()`（登记前解除，brief 失败不再回滚） | 同上（`Brief` 那个失败点） | ✅ 抓住 (rc=101) |
| S4-5 | `remove()` 不等收尾提交（删掉 `await_end_commit`） | `remove_waits_for_the_end_of_task_commit` | ✅ 抓住 (rc=101) |
| S4-6 | 收尾提交失败被丢掉（finisher 不写 `TaskFinished`） | `a_failing_end_commit_shows_in_task_finished` | ✅ 抓住 (rc=101) |

6/6 全抓。

**S4-4 的第一次尝试（如实记录）**：把 `guard.disarm()` 插在 `let record = Arc::new(TaskRecord …)`
之前**不**被抓住——那是 brief 之后的最后一句话，四个失败点都已过去，变异不可区分。改成
`open_session` 之后（brief 之前）立刻解除才被 `Brief` 那个失败点抓住。check 列表里保留了这次
失败记录，省得后人重复。

**S4-5 的同步方式**：收尾提交被一个 seam（`CommitGate`）卡住，测试用 oneshot 收到「提交已进入」，
不用 sleep 同步；「仍未被唤醒」用有界 `timeout` 观测。变异（remove 不等）会在提交还卡住时读到
未提交的工作树并拒绝，因此 `timeout` 返回 Ok，断言失败。
## ADR 0039 决定 6：通知方在 spawn 之前同步订阅（2026-10-16）

工作树 `oc/sec-jobs`（base `0e9baef`）· 变异就地改 `job_notify.rs`、跑完从
`.radian/scratch/mut/job_notify.rs.orig` 还原并 `diff -q` 确认，`git status --short` 干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SJ6 | `JobNotifier::spawn` 把 `self.jobs.subscribe()` 放回 `runtime.spawn(async move { … })` 内部 | `job_notify::the_first_job_events_are_not_lost`（首个 job 的 `Started`/`Settled` 都不在 journal 里） | ✅ failed |

## ADR 0030 切片 6 part a：lead 的工具（2026-10-16）

工作树 `.radian-worktrees/orch-s6a`（分支 `oc/orch-s6a`）；每条变异就地改，跑完从
`.radian/scratch/` 下的副本还原并 `cmp` 确认，结束时 `git status --short` 只剩测试文件。
命令 `cargo test -p radian-host --test lead_tools --no-fail-fast`（gate 的来源/顺序则由
`cargo test -p radian-permissions` 的 `the_tool_gate_denies_before_the_rules_and_the_approver` 钉住）。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| S6A-1 | `wait_task` 超时时取消 task（timeout 分支里加 `record.session.stop()`） | ✅ failed | `a_timeout_returns_still_running_and_keeps_the_task` |
| S6A-2 | 摘要永不截断（`truncate_chars` 短路成「不截」） | ✅ failed | `a_long_summary_is_cut_and_says_so` |
| S6A-3 | `lead_mode_gate` 恒返回 `None`（general 也放行每个 lead 工具） | ✅ failed | `the_mode_gate_denies_in_general_and_allows_in_lead_with_one_tool_list` |
| S6A-4 | `lead_mounted` 去掉 `!task_agent`（任务会话也挂上 lead 工具） | ✅ failed | `a_task_session_gets_no_lead_tools` |
| S6A-5 | `SpawnArgs` 加一个 `policy` 字段（模型可传边缘策略命令） | ✅ failed | `spawn_task_refuses_bad_arguments_and_has_no_policy_field` |
| S6A-6 | 任务会话的 hook turn source 不设成 `Subagent`（留空） | ✅ failed | `a_lead_turn_stays_user_and_a_task_turn_is_subagent` |
| S6A-7 | profile 里给 lead 工具名的 `allow` 规则被去掉（`with_allowed_tools` 删掉） | ✅ failed（30 s 超时：lead 模式的 `list_tasks` 卡在审批上） | `the_mode_gate_denies_in_general_and_allows_in_lead_with_one_tool_list` |

7/7 抓住。

**S6A-1 的第一次尝试（如实记录）**：最初超时测试只断言 `!record.running()` 与
`last_outcome().is_none()`——`session.stop()` 之后这两条都还成立，变异**没被抓住**。改成超时后
再给任务一条 `Subagent` 输入并等它跑出一次模型请求（`wait_requests(&server, 1)`），被 stop 的
会话不再驱动任何回合，测试才抓住。

**S6A-8 / S6A-9（part a 里没有对应代码，如实记录）**：brief 里的两条变异「任务事件写进 lead 的
inbox」与「lead 的回合来源被任务事件降低」在 part a 里**不可施加**——lead 的工具根本不持有 lead
的 inbox 句柄，任务的事件只经过 `TaskPendingQueue`（切片 5）。这两条性质由
`a_lead_turn_stays_user_and_a_task_turn_is_subagent` 的两条断言钉住（lead 日志里没有任何
`"subagent"` 输入；`hooks().turn_source() == Some(User)`）。能把任务事件塞进 lead 的 inbox 的近似
变异在运行时/切片 4-5 的代码里，由那些切片的 inbox-来源测试抓住。
