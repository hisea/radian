# `radian-host` mutation record (provider selection + shared describe)

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

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
are `the_startup_text_does_not_claim_credential_isolation` (radian-exec) and
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

## ADR 0013 T4：诊断目录（2026-10-13）

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

## ADR 0021 第二片：项目身份与 `MemoryService`（2026-10-13）

Base `5d80946` · 一次性 worktree `/tmp/memsvc-mut`（从 HEAD 建，做完删掉）· **独立 `CARGO_TARGET_DIR`**
· 每个变异跑 `cargo test -p radian-host memory` 与 `cargo test -p radian-exec --test cli memory`
（都 `--no-fail-fast`），然后 `git checkout -- .`；`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | worktree 不解析 `commondir`（返回自己的 gitdir） | `memory::identity::tests::{a_worktree_and_its_main_repository_share_one_key, the_identity_matches_git_common_dir_when_git_is_available, a_relative_gitdir_and_commondir_are_resolved}`、`memory::service::tests::a_worktree_resolves_to_the_main_repositories_project`、`memory_project_is_shared_with_a_git_worktree` | ✅ 抓住 |
| M2 | `Unlinked` 时 Project 写自动注册 | `memory::service::tests::{an_unlinked_workspace_refuses_a_project_write_and_still_reads_global, relinking_after_a_move_finds_the_original_records}`、`memory_unlinked_workspace_refuses_project_writes_and_lists_global`、`memory_relink_after_a_move_finds_the_original_records` | ✅ 抓住 |
| M4 | 用户命令写 `Source::Agent` | `memory::service::tests::{a_project_write_reads_back_after_creating_the_project, user_commands_set_the_user_source_and_check_the_revision}`、`memory_add_list_edit_confirm_export_import_show_round_trip`、`memory_json_output_is_parseable_and_has_stable_fields` | ✅ 抓住 |
| M5 | `add` 默认写 Global | `memory::service::tests::{a_project_record_overrides_a_global_one_with_the_same_key, a_project_write_reads_back_after_creating_the_project, …}` 与 6 个 CLI `memory_*` 测试 | ✅ 抓住 |
| M6 | 识别键不向上查找祖先目录 | `memory::identity::tests::a_subdirectory_finds_the_repository_above_it` | ✅ 抓住 |
| M7 | `candidate_name` 不把 `.git` 当仓库名 | `memory::identity::tests::candidate_names_use_the_repository_directory_for_a_common_dir` | ✅ 抓住 |

6/6 全抓（M3 在 `radian-exec/MUTATIONS.md`）。M4/M5 同时被单元测试与 CLI 测试抓住，说明
「服务层规则」与「命令接线」两条路径都有覆盖。

**评审补充（合并时，2026-10-13）**：原实现在没有 `commondir` 时取 gitdir 的祖父目录，真实 submodule（`<super>/.git/modules/<name>`，git 不为它写 `commondir`）因此被解析成上层仓库的 `.git`，两者静默共享一个项目。改为 git 自己的规则：没有 `commondir` 时 gitdir 即 common dir。变异"恢复祖父回退"由 `a_submodule_is_its_own_project_not_the_superprojects` 与 `a_missing_commondir_means_the_gitdir_is_the_common_dir` 抓住。

## ADR 0021 第三片：agent 工具与注入（2026-10-13）

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

## ADR 0021 第四片：共享命令的确认（2026-10-13）

Base `4bf767d` · 一次性 worktree `/tmp/memtui-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M4 | `confirm` 恒为 `None`（`confirmed=false` 仍执行写操作） | `memory::commands::tests::an_unconfirmed_forget_writes_nothing`；`radian_exec::cli::memory_forget_needs_confirmation_off_a_terminal`、`memory_split_and_merge_move_records_between_projects`；`radian_tui::tests::command::{a_confirmed_forget_waits_for_y_and_n_keeps_the_record, a_pending_confirmation_keeps_keys_out_of_the_input_box}`、`command::tests::memory_writes_become_a_confirmation` | ✅ |

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
`cargo test -p radian-tui --lib --test config`、以及 `cargo test -p radian-exec --test cli` 的
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
`--base-url`）改的是本 crate 的代码，但抓住它们的测试在 `radian-exec`；明细见
`crates/radian-exec/MUTATIONS.md` 的 M22–M25。

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
`auth.rs`，抓住它们的测试分别在 `radian-exec`（具名 provider 验收）、`radian-host`（`login::tests`）
与 `radian-tui`（logout pty）；明细见 `crates/radian-exec/MUTATIONS.md` 的 L1–L7。
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
| B1a | 去掉档位预算夹取（`Enabled` 原样发出） | `radian-exec::a_level_budget_clamps_to_max_tokens` | ✅ |

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

## 系统提示词分层与三份角色（ADR 0036，切片 ②）

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

工作区 `<worktree>`，分支 `dogfood/startup-speed`；每个变异改完从副本
还原并 `cmp` 确认。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SP-S1 | `sandbox_word` 的 workspace 分支改回直接返回后端名（`macos-seatbelt`） | `describe::tests::the_summary_word_names_the_backend_in_plain_words`；`radian-tui::tests::pty::pty_the_startup_summary_names_the_real_sandbox_backend` | ✅ |
| SP-S2 | `linux-bwrap` 的分支错写成 `"macOS sandbox"`（把平台的词写死成另一个平台） | `describe::tests::the_summary_word_names_the_backend_in_plain_words` | ✅ |

2/2 抓住。SP-S2 在 macOS 上运行时 pty 用例抓不住（这台机器只会走 `macos-seatbelt` 分支），
所以词组按平台分别断言在 `describe` 的单测里（同一测试也覆盖未知后端按真名显示）。

## context-escape（上下文文件闭合标签/属性转义，ADR 0036 补充，2026-10-15）

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
`tests/model_switch.rs` (16 tests) and the whole `radian-tui`/`radian-exec` suites stayed green.
(`-p radian-tui -p radian-exec` needs `cargo build -p agent-extensions --bin mcp-fixture` first;
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
radian-host --lib --no-fail-fast memory::` (AC-1 additionally: `cargo test -p radian-exec --test
cli -- memory` after `cargo build -p radian-tui --bin radian`, and `cargo test -p radian-tui
--test pty -- pty_a_memory_add`).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| AC-1 | `project_for_write` never registers: a git workspace with an unknown key gets the `Unlinked` error (the pre-revision behaviour) | ✅ caught | `memory::service::tests::{a_write_registers_a_git_workspace_and_a_read_never_does, a_write_outside_a_repository_is_refused_and_creates_nothing, a_write_still_creates_when_a_candidate_previous_project_exists}`, `memory::tools::tests::{a_save_in_a_git_workspace_registers_the_project_and_notices_once, a_save_outside_a_project_workspace_is_refused_without_suggesting_global}`, and end to end `radian_exec::cli::{memory_add_registers_a_git_workspace_and_prints_one_notice, memory_save_in_a_git_workspace_creates_the_project_and_exec_shows_the_notice, memory_unlinked_workspace_refuses_project_writes_and_lists_global}`, `radian_tui::pty::pty_a_memory_add_shows_the_created_project_notice` |
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
`cargo build -p radian-tui --bin radian`（radian-exec 的测试会拒绝过期的二进制）。

**第一次跑法有错**（把 `--no-fail-fast` 放在 `--` 之后当成了过滤器，M1 因此看起来"被抓住"），
下表是改正后重跑的结果：每条都给出真正失败的测试名与 panic 行。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | 固定段的查询文本改回**最后一条**用户消息，改在 `MemoryHooks::before_model_call` 的 `.fixed(...)` 里 | `radian_host/tests/memory_injection.rs::the_hook_orders_the_block_by_the_first_user_message` | ✅ caught |
| M1b | 同一变异，改在追加接缝的查询回退（`addition_for_turn` 里的 `first_user_text(transcript)`） | `radian_exec::cli::{a_resumed_session_ranks_the_block_by_its_first_user_message, a_forked_child_ranks_the_block_by_the_parents_first_user_message}` | ✅ caught |
| M2 | 驱动器用 `InputSource::User` 注入追加消息 | `radian_runtime::tests/driver.rs::a_host_addition_lands_after_the_user_message_and_before_the_first_call` | ✅ caught |
| M3 | 追加消息改到 `drain_steering` **之后**注入（晚一步到） | 同上（transcript 顺序：追加消息不再紧跟用户消息） | ✅ caught |
| M4 | 追加候选忽略去重账本（`exclude` 为空） | `radian_host/tests/memory_injection.rs::a_later_turn_appends_its_new_hits_as_one_system_message` | ✅ caught |
| M5 | 固定段每次调用重算（去掉 `fixed` 的缓存短路） | 宿主：`memory_injection.rs::{the_block_reaches_the_system_prompt_and_the_event_is_written_once, a_later_turn_appends_its_new_hits_as_one_system_message, an_addition_budget_drops_whole_records_and_additions_can_be_off}`；CLI：`the_memory_block_is_fixed_per_process_and_a_new_record_is_appended_once` | ✅ caught |
| M6 | `<session>` 每次请求重算（去掉 key 比较） | `radian_exec::cli::the_memory_block_is_fixed_per_process_and_a_new_record_is_appended_once`（write 之后 `git:` 行从 `(clean)` 变成未提交） | ✅ caught |
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

1/1 全抓（M2、M3 的行为测试随 `git_diff` 工具一并删除，见 `remove-git-diff`；M1 的页脚测试仍在）。SG-M1 由 lfs 那条确定性抓住：`git add` 对 `filter=lfs` 的文件会执行仓库配置里的
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

1/1 全抓（M2、M3 的行为测试随 `git_diff` 工具一并删除，见 `remove-git-diff`；M1 的页脚测试仍在）。守卫测试（`crates/radian-coding/tests/host_git.rs`）遍历 `crates/*/src` 里 `#[cfg(test)]`
之前的部分，`Command::new("git")` 只允许出现在 helper 自己（`host_git.rs`）与沙箱测试 harness
（`radian-sandbox/src/harness.rs`，其唯一的裸 git 只建测试仓库）；新增一个生产裸 git 调用会让它
失败，SG-F1-M1 的守卫那条就是证明。

**如实记录（测试的触发条件）**：`git status` 只在**大小相同**的 stat-dirty 文件上才哈希内容、从而跑 clean filter；内容大小不同时 git 直接按 stat 判为改动，不跑 filter。所以 M1 的页脚行为测试用「与 index blob 同大小」的改动（`one\n` → `two\n`）。第一次写测试时用了不同大小（`one\ntwo\n`），SG-F1-M1 的行为测试假绿（只有守卫抓住）；改成同大小后行为测试也抓住。（M2、M3 删除前也是同大小改动；两行随 `git_diff` 工具一并删除。）
## ADR 0043 复核修订 1：`tools` 卸载与 `skills` 允许名单（2026-10-16）

工作树 `.radian-worktrees/orch-s7a`（分支 `oc/orch-s7a`，基 `5625f6f`）。每条变异就地改，跑单个测试，
再从 `.radian/scratch/mut/` 的副本 `cp` 还原并 `cmp` 确认；跑完 `git status --short` 只剩源码改动。跑法
（`timeout` 包裹，`--no-fail-fast`）：

```sh
cargo test -p radian-host --test task --no-fail-fast -- <下面的测试名>
```

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| RF1-1 | `open_session` 不把 `tools_allow` 交给 `HostTools`（`with_tool_allow(tools_allow)` 去掉，只剩权限层 deny） | ✅ failed | `a_tools_allow_list_unmounts_the_unlisted_built_ins`（工具规格变成全部内置工具：`ask_user, git_commit, … write`，`<runtime>` 也列出 `write`；模型脚本里对 `write` 的调用变成 `no tool named`） |
| RF1-2 | `open_session` 忽略 `skills_allow`（`Some(_) => discovery.skills`，不过滤目录） | ✅ failed | `a_skills_allow_list_keeps_only_the_named_skills`（目录里出现 `s7a-two, diagrams, theme-import`，不止名单里的 `s7a-one`） |
| RF1-3 | 注册表不计算缺失技能名（`let warnings: Vec<String> = Vec::new();`，丢掉了「未找到的技能名」警告） | ✅ failed | `an_unknown_skill_name_is_a_spawn_warning_not_a_failure`（`record.warnings` 为空） |

3/3 抓住。

**`git_commit` 的处理（如实记录）**：`git_commit` 是允许名单宇宙里的一个内置工具（`builtin_tool_names`），
本片早先已按名单 deny 它。卸载因此也把它卸掉：`tools: [read, list]` 的任务工具规格恰好是 `list, read`，
`git_commit` 不在其中。O3 的收尾提交由宿主自己完成，不需要任务挂 `git_commit`。
## ADR 0028: radian's own layered flowchart layout (2026-10-16)

Worktree `.radian-worktrees/build-script`, branch `dogfood/mermaid-layout`. Each mutation is
applied in place by `.radian/scratch/mutate.py` (it asserts the anchor occurs the expected number
of times, so a `cargo fmt` shift or a no-op patch fails loudly instead of passing as "caught"),
the suite runs with `--no-fail-fast`, and the file is restored with a plain `cp` and `cmp`ed byte
for byte (drivers: `.radian/scratch/mutate-layout{,2,3,4,5}.sh`). `git status --short` held only the
source and test files at the end.

| # | Requirement mutated | Mutation | Caught by | Result |
|---|---|---|---|---|
| L1 | decision 3: an adjacent edge leaves the **middle** of a box (`mermaid_layout.rs::adjacent_edge`) | `let source = from.centre();` → `from.across_end()` | `the_owner_chain_is_a_centred_column_of_three_row_boxes` (the fork's `┴` is off the box's middle, so the connector is no longer one column), `a_diamond_splits_and_joins_on_the_bus_rows`, `an_lr_chain_is_the_same_drawing_turned`, `a_back_edge_wraps_outside_the_canvas`, `the_width_decides_between_the_vendored_reason_and_the_drawing` | ✅ 5 failed |
| L2 | decision 3: a box is **one row high**, not port-counted | `plan`: the vertical `along_size` back to the vendor's rule (`3 + ports`) | `the_owner_chain_...` (`bottom - top == 2`), `a_diamond_...`, `a_back_edge_...` | ✅ 3 failed |
| L3 | decision 3: every layer is **centred** in the canvas | `place`: `let mut across = (canvas - layer_across[index]) / 2;` → `0` | `the_owner_chain_...` (the layer's left is not `(canvas - extent) / 2`), `a_diamond_...`, `a_back_edge_...` | ✅ 3 failed |
| L4 | decision 3: a back edge leaves a box's **side** and wraps **outside** the canvas | `channel_edge`: `let out = (from.across_end(), from.mid_along());` → `(from.centre(), ...)`, so the run starts inside the box | `a_back_edge_wraps_outside_the_canvas` (the route is refused, the diagram becomes the vendor's) and `accepts_the_same_samples_as_the_vendored_renderer` | ✅ 2 failed |
| L5 | decision 4: a refusal goes to the **vendored renderer**, not an error | `mermaid.rs::render_spans`: the fallback call → `Err(RenderError::Unsupported)` | `a_diagram_the_layout_refuses_is_the_vendored_render_byte_for_byte`, `accepts_the_same_samples_as_the_vendored_renderer`, `the_width_decides_...` | ✅ 3 failed |
| L6a | decision 5 / ADR 0029: an edge's label keeps the **edge** role (vertical layout) | `adjacent_edge`: the TD label's `Role::Edge` → `Role::Text` | `the_layout_emits_only_the_three_roles` (`role_of("yes")`) | ✅ 1 failed |
| L6b | the same, horizontal layout | the LR label's `Role::Edge` → `Role::Text` | `the_diagram_roles_come_from_the_theme` (radian-tui) | ✅ 1 failed |
| L7 | two edges of one gap that would **cross** are refused | `draw`: `bus_rows_are_clear(graph, plan)?;` removed | `a_diagram_the_layout_refuses_is_the_vendored_render_byte_for_byte` (the crossing sample is drawn instead of falling back) | ✅ 1 failed |
| L8 | a wide character's second cell carries nothing (ADR 0028 R9) | `spans`: `Screen::Covered => continue` → an `Edge` space | `a_cjk_label_is_sized_by_display_width` (the label row is wider than its box) | ✅ 1 failed |
| L9 | a stadium's corners are rounded (decision 3) *and* turned with the layout | `round_corners`: `round_glyph(screen_bits(cell.bits, flow))` → `round_glyph(cell.bits)` | `a_stadium_keeps_its_corners_rounded` (the `LR` golden gets `╭`/`╰` in the wrong corners) | ✅ 1 failed |

9 mutations, 9 caught.

**An equivalent mutant, honestly recorded (the ADR's "回边穿过框")**: "a back edge crosses a box"
is defended **twice** — the route's freedom pre-check (`channel_edge`'s `grid.free` loop) and the
grid's own refusal to draw over text or an arrowhead (`Grid::wire`/`Grid::arrow`). Removing
either one alone changes no output on any sample: the first round of this check removed the
`grid.free` pre-checks (`mutate-layout.sh` M4) and the whole suite stayed green, because the
route is stopped by the label text of the box in its way. L4 is the observable form of the same
requirement: the route then starts inside the box, the layout refuses it, and the back-edge
snapshot sees the vendor's drawing instead of the layout's.
## ADR 0043 decision 1：进程控制口（Unix socket、JSON lines，2026-10-16）

Base `a3eb856`（分支 `oc/orch-s8`）· 就地改 `src/control.rs`，每次从 `/tmp/control.rs.bak`
`cp` 还原并用 `cmp` 确认逐字节相同；每个变异跑
`cargo test -p radian-host --test control --no-fail-fast <测试名>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| CTL-1 | `parse_source` 接受 `"user"`（`"user" => Ok(InputSource::User)`） | ✅ failed | `sources_are_fixed_by_the_port_and_an_approval_verb_does_not_exist`（`source: user` 得到 `ok: true` 而不是拒绝） |
| CTL-2 | 扇出改成 `client.tx.blocking_send(...)`（在运行时线程上等慢客户端） | ✅ failed（进程 SIGABRT：`blocking_send` 在运行时线程上 panic） | `a_slow_subscriber_is_disconnected_and_turns_still_finish` |
| CTL-3 | `bind_unix` 去掉 `clean_stale_sockets(&dir)` | ✅ failed | `the_socket_is_private_cleans_stale_files_and_is_removed_on_drop`（死 pid 的 `2147483647.sock` 仍在） |
| CTL-4 | socket 模式写成 `0o644` | ✅ failed | 同上（mode 断言 `0o600`） |

4/4 抓住。CTL-2 的「抓住」表现为 panic/abort（tokio 禁止在运行时线程上 `blocking_send`），
这正是变异要证明的：扇出绝不能在生产事件的线程上等待客户端。

## ADR 0026 slice ①: ChatGPT OAuth login and refresh (2026-10-16)

Base `7705bb5` (branch `oc/oauth-login`) · mutated in place in `crates/radian-host/src/{auth,oauth_chatgpt}.rs`,
each restored from a `cp` of the pre-mutation file in `.radian/scratch/mutations/` and confirmed
byte-identical with `cmp` (no `git checkout --`/`restore`, per this worktree's rules); every
mutation run with `--no-fail-fast`.

| # | Requirement mutated | Mutation | Result | Caught by |
|---|---|---|---|---|
| M1 | an oauth token must never reach a log/journal/export | `AuthStore::resolved_keys()`'s oauth loop deleted (only `api_key` entries redacted) | ✅ FAILED | `auth::tests::oauth_tokens_are_redacted_from_journal_and_export_shaped_text`, `auth::tests::resolved_keys_also_carries_oauth_access_and_refresh_tokens` |
| M2 | refresh before expiry | `ensure_fresh_in` returns the stored (stale) access token unconditionally, before the margin check | ✅ FAILED | `refresh_runs_only_within_the_margin_and_writes_back` (`left: "stale"` vs. the refreshed token) |
| M3 | the callback's `state` must be checked | `parse_callback` drops the `state != expected_state` branch | ✅ FAILED (both) | `oauth_chatgpt::tests::a_state_mismatch_is_refused_without_reading_the_code` (unit); `a_state_mismatch_is_refused_and_never_reaches_the_token_endpoint` (integration, against the fake server) |
| M4 | the written `auth.json` must be `0600` | `AuthStore::write`'s mode hardcoded to `0o644` | ✅ FAILED | `exchange_stores_an_oauth_entry_at_0600` (caught even earlier than the mode assertion: the reopen's own `WidePermissions` check refuses the file) |
| M5 | `originator` must be `radian`, never Codex's own value (ADR 0026) | `ORIGINATOR` constant changed to `"codex_cli_rs"` | ✅ FAILED | `oauth_chatgpt::tests::authorize_url_carries_the_fixed_constants_and_originator_radian` |

5/5 caught, including the 4 the task brief names explicitly (M1–M4) plus the ADR's own listed
mutation for the Codex-impersonation risk (M5). M4's shared write path (`AuthStore::write`) is
also covered by the pre-existing `A26-1`/`F2` entries above for `api_key` writes; M4 here
confirms the **same** path protects the `oauth` entry specifically, through the oauth-only test.

## ADR 0026 slice 3: wiring the chatgpt provider (2026-10-16)

Base `92b11fb` (branch `oc/oauth-wire`) · mutated in place in `crates/radian-host/src/
{oauth_chatgpt,model}.rs`, `crates/radian-tui/src/cli.rs` and
`crates/radian-host/tests/chatgpt_lead_task.rs`, each restored from a `cp` of the
pre-mutation file in `.radian/scratch/mutations/` and confirmed byte-identical with `cmp`
(no `git checkout --`/`restore`, per this worktree's rules); every mutation run with
`--no-fail-fast`.

| # | Requirement mutated | Mutation | Result | Caught by |
|---|---|---|---|---|
| M1 | `originator` must be `radian`, never Codex's own value | `oauth_chatgpt::ORIGINATOR` changed to `"codex_cli_rs"` | ✅ FAILED | `provider::tests::the_chatgpt_descriptor_is_registered_with_responses_models_and_static_headers_only` (the descriptor's static headers) |
| M2 | every request carries `chatgpt-account-id` | `ChatGptModel::complete` drops the `.with_header("chatgpt-account-id", …)` call | ✅ FAILED | `chatgpt_headers::headers_carry_originator_radian_and_the_account_id_from_the_token` (mock-recorded header is `None`) |
| M3 | the usage contributor only polls while routed to `chatgpt` | `chatgpt_usage_active` changed to always return `true` | ✅ FAILED | `cli::chatgpt_usage_gate_tests::the_gate_is_active_only_for_chatgpt` |
| M4 | a task's own route (ADR 0043), not the lead's, decides its model | the end-to-end test's model factory changed to ignore `agent_model` and always build the lead's default (chatgpt) model | ✅ FAILED | `chatgpt_lead_task::a_chatgpt_lead_default_and_a_deepseek_task_route_requests_to_the_right_mock` (the routed task's outcome becomes `Failed`, since the chatgpt mock's one scripted reply is already consumed by the default-route task) |

4/4 caught — exactly the task brief's list. M4 mutates the *test's own* model-factory
closure rather than `radian-tui/src/cli.rs`'s real `task_model`: that closure is private to
a different crate's startup function and not independently callable from a `radian-host`
test, so the closure here (built to mirror it field-for-field, per the test file's own doc
comment) stands in for it. The mutation and its catch are faithful to the real routing
logic's shape, even though they do not touch the production closure's bytes.
## ADR 0042: 渲染侧移出 host（`mermaid_check`、`mermaid_layout`、`diagrams` skill 删除，2026-10-16）

本 ADR 的 code 改动没有新的 host 变异可跑：`radian-host/src/mermaid.rs`、`mermaid_layout.rs`
与它们的测试，以及内建 `diagrams` skill、`mermaid_check` 工具（挂载、dispatch、radian-exec 表、
permission allow 名单）全部删除；`radian-tui` 改为直接依赖 vendored 的
`third_party/grok-mermaid`。因此本文件里这些旧的节不再有对应代码或测试，保留为历史：

- 「ADR 0025 / ADR 0028 R8：内建 skill + diagrams + mermaid_check」的 DG-M1/DG-M2；
- 「ADR 0028: radian's own layered flowchart layout」的 L1-L9。

新渲染器的四个变异记在 `crates/radian-tui/MUTATIONS.md` 的「ADR 0042」一节；上游文件的保真度
（决定 2）由 `third_party/grok-mermaid/tests/pristine.rs` 自己查。
## ADR 0043：项目编排配置、信任门、快照、沙箱 verify（2026-10-16）

Base `11a25d9` · worktree 内就地改 `src/orchestration.rs` / `src/task.rs`，每个变异从内存里的
原始字节还原并用 `==` 断言逐字节相同（脚本 `.radian/scratch/mutate-s9.py`，跑完 `git status`
为空）；每个变异跑单条测试 `cargo test -p radian-host --no-fail-fast <测试>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| MUT9-1 | `load_project` 无视 `trusted`（`if !trusted` → `if false`）：未信任的工作区也读项目文件 | ✅ failed | `an_untrusted_workspace_reads_no_project_config_and_no_policy`（未信任时 `policy`/`verify` 非空）；集成 `the_edge_policy_comes_from_a_trusted_project_config_only` 的未信任一半同理 |
| MUT9-2 | verify 不走沙箱（把 `sandbox.command_str("/bin/sh", …)` 换成裸 `std::process::Command`） | ✅ failed | `verify_runs_in_the_task_sandbox`（禁读目录里的 `cat` 成功了，`exit_code == Some(0)`） |
| MUT9-3 | verify 从 **task 自己的 worktree** 读（`self.orchestration.verify` → `load_project(worktree, true)`） | ✅ failed | `a_tasks_own_dot_radian_and_a_later_edit_do_not_change_the_running_task`（命令是 `echo task` 而不是 `echo lead`） |
| MUT9-4 | 快照在 spawn 时重读 lead 工作区（`self.orchestration.verify` → `load_project(&self.workspace, true)`） | ✅ failed | 同上（spawn 后写进 lead 的 `echo changed` 生效了） |
| MUT9-5 | 策略命令不从快照填（`policy.or_else(|| …)` → `let policy = policy;`） | ✅ failed | `the_edge_policy_comes_from_a_trusted_project_config_only`（受信任时 `record.policy` 仍是 `None`） |
| MUT9-6 | named 路径不再读 lead 快照的项目 agent 目录（`src/named.rs` 的 `self.orchestration.project_agents.as_deref()` → `None`，合并 origin/main 后补） | ✅ failed | `a_named_agent_reads_the_leads_project_definition_snapshot`（named agent 落到用户级 `writer` 的 worktree 隔离，`outcome.worktree` 变成 `Some`，断言 `None` 失败） |

5/5 抓住。MUT9-1 同时钉住「未信任时 verify 与 policy 都不读」这一条门的两个面（同一个
`if !trusted` 早退）。MUT9-3 与 MUT9-4 都用同一条快照测试，但变异点不同：一个从 task 的
worktree 读，一个在 spawn 时重读 lead 的文件。合并 origin/main（切片 7b 的 named agent）后
补了 MUT9-6：named agent 的定义也读 lead 工作区的快照（ADR 0043），同一条
测试就地变异 `src/named.rs` 验证，改完从 `.radian/scratch/named.rs.bak` 的副本 `cp` 还原并用
`cmp` 断言逐字节相同。

## ADR 0027 slice ① task 2: the out-of-process footer `command` component (2026-10-16)

Worktree `footer-command-component` (base `radian/integration` `d647750`). In-place mutations
driven by `.radian/scratch/mut/mutate.py` (not committed): each file was backed up under
`.radian/scratch/mut/backup/`, the anchor asserted present **exactly once** (a `cargo fmt` shift
or a no-op patch is a hard `PATCH FAILED`), the named test run with
`cargo test -p radian-host --locked --no-fail-fast <target> <name>`, then the file restored with
`shutil.copyfile` + `os.utime` (so cargo rebuilds) and compared byte-for-byte; `git diff` on the
source files is empty after the run.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| H1 | only the first stdout line is kept | `first_output_line` no longer cuts at `\n` | `command_footer::the_first_line_is_taken_and_truncated_to_256_bytes` FAILED (`first\nsecond` kept whole) |
| H2 | the value is hard-capped at 256 bytes | the size check in `first_output_line` becomes `line.len()` | same test FAILED (400 bytes kept) |
| H3 | `period = 0` is once-only and other values clamp to 15–3600 | `period_from_secs` returns `Some(Duration::from_secs(secs))` | lib `command_footer::tests::the_period_is_clamped_and_zero_means_once` FAILED |
| H4 | the whole run is bounded by a 5 s timeout | the `tokio::time::timeout(COMMAND_TIMEOUT, run)` wrapper is removed | `command_footer::a_timeout_keeps_the_last_value_and_marks_it_stale` FAILED (the 30 s command runs to completion; the elapsed bound fires) |
| H5 | a non-zero exit is a failure (last value kept, stale) | the `!status.success()` check becomes `if false` | `command_footer::a_nonzero_exit_keeps_the_last_value_and_marks_it_stale` FAILED (the failing output is published as fresh) |
| H6 | the child's environment is cleared | `.env_clear()` removed | `command_footer::the_environment_is_cleared_to_the_deliberate_allow_list` FAILED (host variables reached the child) |
| H7 | the deliberate allow-list exports `RADIAN_SESSION_ID` | that `command.env(...)` line removed | same test FAILED |
| H8 | a JSON `style` selects the text's theme role | `parse_output` ignores the name (`SegmentRole::Normal`) | `command_footer::json_output_sets_the_role_and_symbol` (and the lib unit test) FAILED |
| H9 | invalid JSON is a failure, not plain text | `if !line.starts_with('{')` becomes `if true` | `command_footer::invalid_json_and_unknown_styles_keep_the_last_value` FAILED (`{not json}` published as text) |
| H10 | a project-level `command` is refused | `command_specs`'s `origin == Project` check becomes `if false` | `config_fixtures::the_command_fixture_carries_command_and_period` FAILED (the project config yielded specs) |
| H11 | the stdin JSON carries every documented fact | `context_percent` is written as `null` | `command_footer::the_stdin_is_the_documented_json_context` FAILED |
| H12 | the output never reaches a journal | a `std::fs::write("/tmp/radian-footer-journal", &buffer)` added in `run` | `command_footer::the_command_runner_has_no_journal_or_herdr_path` FAILED (`command_footer.rs` reached `std::fs`/journal) |
| H13 | the command line is split with no shell | whitespace no longer separates words (the join branch is a no-op) | lib `command_footer::tests::command_lines_split_without_a_shell` FAILED (`printf hi` became one argv word) |
| H14 | a `disabled = true` command item is never run | the disabled check in `command_specs` is removed | lib `command_footer::tests::a_disabled_command_is_not_a_spec_and_a_malformed_one_warns` FAILED |

14/14 caught; every mutated file compared byte-identical after its mutation.

**Old-code check.** The runner module and its tests are new, so on the pre-change tree the
command tests do not compile (no `command_footer` module). The behaviours they pin are the
mutants above; H5/H12 are the two the brief names (a failure keeps the last value; the output
must never reach a journal).

## ADR 0027 slice ① fix round `footer-s1-fix1`: H10 removed (2026-10-16)

**v1 has no project config layer** (ADR 0031;
`config::resolve_path` chooses only `--config` > `RADIAN_CONFIG` > `~/.radian/config/config.toml`,
all user-chosen). The provenance seam (`ConfigOrigin`, `classify_origin`, `Config.origin`,
`load_in_workspace`) and `command_specs`'s `origin` parameter were removed, so the project-level
`command` refusal no longer exists. The **H10** row above is therefore **removed**: its requirement
is gone, not failed. Every `command` from any current source is accepted
(`config_fixtures::the_command_fixture_carries_command_and_period`, minus its former project half).
When a project-level config layer is added, a `command` must be refused there, and that slice owns
the test. No other row in this file is touched.

## Gate review of the footer `command` component (2026-10-17, reviewer)

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| GR-1 | an unknown JSON style's error repeats the style name (`format!("... {name:?} ...")`, the branch's code) | caught (rc=101) | `an_unknown_style_notice_never_carries_the_style_name` |
| GR-2 | drop `.env_clear()` from the command spawn | caught (rc=101) | `the_environment_is_cleared_to_the_deliberate_allow_list` (`HOME` leaks; the old `set_var` probe was removed because it raced the parallel tests) |

## ADR 0016: the `inherited` startup lines (2026-10-16)

Worktree `.radian-worktrees/inherited-sandbox`, branch `dogfood/inherited-sandbox`. Each mutation
changes `src/describe.rs` only, is run with `cargo test -p radian-host --lib --locked
--no-fail-fast -- describe::tests`, and the file is restored from a saved copy and compared
byte-identical afterwards.

| # | Mutation | Caught by |
|---|---|---|
| IS-9 | the `inherited` facts sentence reuses `workspace_sandbox_facts` (the whole `workspace` tier sentence: deny-read list, read-only `.git`, network state, Unix sockets, unsandboxed MCP) | `the_inherited_sandbox_line_claims_no_local_rules` FAILED (the forbidden strings appeared) |
| IS-10 | the `inherited` arm is dropped, so the generic `workspace · sandbox {backend} (probe: …)` line is used (the line names the real backend and carries the tier's facts) | `the_inherited_sandbox_line_claims_no_local_rules` FAILED (`an outer sandbox confines this process` missing) |

2/2 caught. IS-9 is acceptance 6's mutation; IS-10 is acceptance 2's "the startup line names the
real backend" mutation, which the ADR's own `sandbox inherited` substring would not catch on its
own (the generic line contains it too, as the backend's name) — so the unit test pins the whole
inherited wording.
## oauth-redact: a `chatgpt` token refreshed mid-session must be redactable too (2026-10-16)

Every redaction list is read from `resolved_keys()` at startup, so about an hour
into a session (the first `ensure_fresh` refresh, `model.rs:204`) the list no longer carries the
access token the session is using. Run: `cargo test -p radian-host --locked --no-fail-fast
--test oauth_chatgpt a_refresh_registers`. The mutation edits `src/oauth_chatgpt.rs` in place,
saving a copy to `.radian/scratch/mut/` first and comparing the restored file with `cmp`; after
the run `git status --short` shows only the intended files.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| OR-H1 | `oauth_chatgpt::store_token` drops its two `secrets::register(...)` lines (the refresh still writes the pair, nothing registers it) | ✅ 1 failed | `a_refresh_registers_the_new_tokens_for_redaction` (`registered_secrets()` is `[]`, so `contains(&fresh_access)` fails) |

## JobNotifier: an ended session releases its journal (2026-10-17)

The notifier held `Arc<Jobs>` while its task waited for the registry's channel to close; the sender lives in that registry, so it never closed and every ended session leaked its task, journal (and lock) and inbox. Run: `cargo test -p radian-host --test job_notify`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| JN-1 | `spawn` keeps a strong registry reference (`std::mem::forget(jobs)` instead of `drop(jobs)`; the old code's strong field has the same effect) | caught (rc=101, "still holds the journal: 2 references") | `an_ended_session_releases_its_journal` |

## chatgpt-limits Part 2: a `chatgpt` route's window comes from its descriptor table (2026-10-17)

models.dev has no `chatgpt` provider, so a `chatgpt` session resolved no window at all. The
descriptor table (`provider::CODEX_MODELS`, cited from `@oh-my-pi/pi-catalog/src/models.json`'s
`openai-codex` block) is now the resolver's fallback (`catalog::descriptor_window`). Run:
`cargo test -p radian-host --locked --no-fail-fast --test chatgpt_window`; the mutation edits
`src/provider.rs` in place, saving a copy to `/tmp/chatgpt-limits-save/` first and comparing the
restored file with `diff -q`; afterwards `git diff` shows only the intended files.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| CW-1 | drop the table entry: `gpt-5.6-sol`'s `codex_limit(1_000_000, 128_000)` → `ModelLimit::UNKNOWN` (the model falls through to the catalog, which has no `chatgpt`) | caught (rc=101) | `every_chatgpt_model_resolves_the_window_its_table_cites` (`source` is `Unknown`, not `Descriptor`) |

## ADR 0043 slice ⓪A：`git_commit` 与编排工具都不再挂载（2026-10-17）

删掉 `git_commit` 与 13 个 task/lead/named 工具后，新会话的模型工具表里不能再出现它们。
变异把 `git_commit` 的 `ToolSpec` 重新挂回 `HostTools::open_session` 的 `specs`
（`cp` 到 `/tmp/s0a-tools.rs.bak`，改后 `cp` 还原并 `cmp` 一致；变异版先
`cargo build -p radian-tui --bin radian` 再跑测试）。命令：
`cargo test -p radian-exec --locked --test cli a_headless_exec_mounts_no_orchestration_tools`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| OA-2 | `tools.rs`：`open_session` 把 `git_commit` 的 spec 重新 push 进 `specs`（重新挂载） | ✅ 1 failed（`git_commit` must not be mounted: [..., "git_commit"]） | `a_headless_exec_mounts_no_orchestration_tools` |

## ADR 0043 slice ⓪a：`subagent`（2026-10-17）

`src/subagent.rs` 与 `src/session.rs` 的六条变异；每条 `cp` 到 `.radian/scratch/mut/`，改后
`cp` 还原并 `cmp` 一致。改到会影响运行中子进程的变异版前，先
`cargo build -p radian-tui --bin radian`（`common::radian_bin()` 会拒绝过期二进制），再跑
`timeout 300 cargo test -p radian-exec --locked --test subagent --no-fail-fast <test>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| SA-1 | `session.rs`：去掉深度检查（`if !is_subagent_child && let Some(parts) = subagent_parts` → 只看 `parts`，子会话也挂 `subagent`） | ✅ 1 failed（子会话的工具表里有 `subagent`） | `a_general_child_has_no_subagent_tool` |
| SA-2 | `subagent.rs`：`--profile` 恒为 `full`（不读父的 sandbox slot） | ✅ 1 failed（`left: "full"`，`right: "workspace"`） | `a_child_records_the_parents_tier` |
| SA-3 | `subagent.rs`：`subagent` 的 `ToolConcurrency` 从 `Shared` 改 `Exclusive` | ✅ 1 failed（barrier 记录的并发峰值 `left: 1`，`right: 3`；耗时 21.6 s，两个 10 s 超时） | `three_subagent_calls_in_one_reply_run_at_the_same_time` |
| SA-4 | `subagent.rs`：删掉 `tokio::select!` 的取消分支（只 `child.wait()`，不传播取消） | ✅ 1 failed（3 s 内子进程还在；restore 后手动 kill 了日志里的 pid） | `a_cancelled_parent_turn_kills_the_child` |
| SA-5 | `subagent.rs`：父非 auto 也给子 `--yes`（`if true`） | ✅ 1 failed（结果里没有 `denied:` 行） | `a_non_auto_parent_denies_the_childs_ask_and_says_so` |
| SA-6 | `subagent.rs`：`explore` 不再传隐藏 `--tools`（挂子会话的普通工具集） | ✅ 1 failed（工具表里出现 `write` 等读写工具） | `an_explore_child_has_only_the_three_read_tools_and_write_is_refused`（`explore` 工具表恰为 `["glob", "grep", "read"]`） |

6/6 抓住。SA-4 说明取消必须**主动**杀死子会话：只靠 future 被 drop 时的
`kill_on_drop`（5 s grace 之后）不足以让「父取消 → 子进程立刻消失」成立，测试的 3 s
期限正是照 `DEFAULT_TOOL_CANCEL_GRACE` 定的有界轮询。SA-1 的检查点在
`open_session`，所以它同时说明「子不能再起子」是建立会话时的物理不挂载，而不是运行时的判断。
## ADR 0043 slice ①：项目声明只能收窄，信任选项写 access.json（2026-10-17）

每个变异都在 `src/` 原地改，先 `cp` 到 `/tmp/`，跑完 `cp` 还原并 `cmp` 逐字节一致；变异版先
`cargo build -p radian-tui --bin radian`（测试二进制的 `radian` 会拒绝过期），再跑单个测试。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| PJ-1 | `project::apply_profile` 接受 `full`（`out.tier = Some(Preset::FullAccess)`） | ✅ 1 failed（`cannot widen` 警告缺失） | `radian-exec::cli::a_project_config_cannot_widen_the_tier`；单元 `project::tests::full_in_a_project_config_is_refused_with_a_warning` |
| PJ-2 | `project::narrow` 取更宽者（`rank(a) >= rank(b)`） | ✅ 1 failed（记住的 `full` 没有被项目的 `workspace` 收窄） | `radian-exec::cli::a_remembered_tier_is_narrowed_by_the_project`；单元 `project::tests::the_narrower_tier_wins` |
| TS-1 | `trust::apply` 的 `TrustHereSandbox` 不写 `access::remember_for` | ✅ 1 failed（`access/access.json` 不存在） | `radian-tui::pty::pty_trust_sandbox_choice_writes_the_access_record` |

3/3 抓住。

## ADR 0043 slice ③: `git_status`/`memory_get` vanish, `git_diff { stat }`/`memory_search { ids }` replace them (2026-10-17)

The renamed tools' old names stay registered (Core's preflight refuses an unregistered name before
any host hook or the provider runs) and are stripped from every model request
(`HostModel::complete` -> `strip_renamed_tool_aliases`). `memory_search` takes `ids` (a full read)
or `query` (a search), exactly one. Mutations were applied in place, each restored from a `cp` in
`.radian/scratch/mut/` and confirmed byte-identical with `cmp`; the one that changes the spawned
binary was run after `cargo build -p radian-tui --bin radian` (`common::radian_bin()` refuses a
stale one).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| GV-2 | the `ids` arm of `memory_search` is ignored (it returns an empty result) | ✅ 1 failed (`No matching memory records.` instead of the two records) | `memory::tools::tests::memory_search_with_ids_returns_the_full_records` |

Command: `timeout 300 cargo test -p radian-host --lib --locked --no-fail-fast memory_search_with_ids_returns_the_full_records`
(GV-2). (GV-1's `strip_renamed_tool_aliases` is deleted with `remove-git-diff`; the row is dropped.)

## ADR 0043 decision 2 gaps: `--header` forwarding, the live route, the drop guard (2026-10-17)

Worktree `.radian-worktrees/subagent-gaps`, branch `dogfood/subagent-gaps`. Each mutation was
applied in place in `crates/radian-host/src/subagent.rs`, restored from a `cp` and confirmed
byte-identical with `cmp`. SG-1 changes the child the tool spawns, so the `radian-exec` run was
made after `cargo build -p radian-tui --bin radian` (the harness refuses a stale binary).

Commands: `timeout 300 cargo test -p radian-host --locked --no-fail-fast --test subagent_headers`
(SG-1, radian-host); `timeout 300 cargo test -p radian-exec --locked --no-fail-fast --test subagent
the_parents_header` (SG-1, end to end).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| SG-1 | the child `env` carrying the parent's `--header` values is never set (`if false`) | ✅ 2 failed | `radian-exec::subagent::the_parents_header_reaches_the_child_and_nothing_else_writes_it` (`the child must send the parent's header`: `left: None`); `radian_host::subagent_headers::the_header_value_reaches_the_child_through_the_environment_not_its_argv` (`EOF while parsing` an empty variable) |
| SG-2 | `current_route` returns the startup route (the config slot is ignored) | ✅ 1 failed | `radian_host::subagent_route::the_child_runs_the_parents_live_model_route` (the recorded argv names `--model old`; the parent's own request still named `new`, so the switch itself is proven) |
| SG-3 | `SessionKillGuard::drop` does nothing (plain `kill_on_drop`) | ✅ 1 failed | `radian_host::subagent_drop::dropping_the_call_leaves_no_grandchild_alive` (the logged grandchild pid was still alive after the 10 s bound) |

## Fix round 1 A: the forwarded header must not outlive the child's startup (2026-10-17)

`RADIAN_CHILD_HEADERS` is read and **removed from the child's own environment** at the top of
`run_blocking`, before the Tokio runtime exists (so the process still has one thread), and the MCP
spawn in `agent-extensions` removes the name again (belt and braces: that spawn already
`env_clear`s, so a name in `inherit_env` is the only path left). Mutations were applied in place,
restored from a `cp` under `/tmp/radian-fix1/mut/` and confirmed byte-identical with `cmp`; every
run was made after `cargo build -p radian-tui --bin radian` and `cargo build -p agent-extensions
--bin mcp-fixture` (the harness refuses a stale binary).

Commands: `timeout 300 cargo test -p radian-exec --locked --no-fail-fast --test subagent <name>`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| FH-1 | `take_child_headers_from_env` reads the variable but does not `remove_var` it | ✅ 1 failed (the child's `shell` command wrote `["X-Test=child-env-secret-9"]` into the dump file) | `radian-exec::subagent::the_childs_own_environment_no_longer_carries_the_parents_header` |
| FH-2 | the MCP spawn's `env_remove("RADIAN_CHILD_HEADERS")` is dropped | ⚠️ not caught alone (with FH-1 fixed the variable is already out of the child's environment, so `inherit_env` copies nothing) | none — the belt and braces is observable only together with FH-1 |
| FH-1+FH-2 | both removals gone | ✅ 1 failed (the MCP server's env dump held `["X-Test=mcp-env-secret-5"]`) | `radian-exec::subagent::an_mcp_server_the_child_starts_does_not_get_the_parents_header` |

## Fix round 1 B: the two-step stop — SIGTERM, the grace, then the session SIGKILL (2026-10-17)

Both the cancel path (`stop_child`) and the drop guard (`SessionKillGuard::drop`) now send SIGTERM
to the child first, wait `CHILD_STOP_GRACE` (3 s) for it to exit, and only then `kill_session` — a
SIGKILL of the child's session cannot reach a `shell` command, because that tool `setsid`s every
command, and the child's own cleanup (cancel the turn, then close the session, which kills every
job's session) is what reaches them. `radian-coding` gained `stop_leader` (SIGTERM to one pid) and
`wait_for_exit` (bounded, `waitid` with `WNOWAIT`, so the leader stays unreaped for the fallback).
`crates/radian-host/tests/subagent_drop.rs` now pins the **fallback** (its stub ignores SIGTERM),
and `crates/radian-host/tests/subagent_stop.rs` is new. Mutations were applied in place, restored
from a `cp` under `/tmp/radian-fix1/mut/` and confirmed byte-identical with `cmp`; every run was
made after `cargo build -p radian-tui --bin radian` (the harness refuses a stale binary).

Commands: `timeout 400 cargo test -p radian-host --locked --no-fail-fast --test subagent_stop`;
`timeout 400 cargo test -p radian-exec --locked --no-fail-fast --test subagent a_cancelled`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TS-1 | the cancel path sends SIGKILL straight away (the graceful step is skipped) | ✅ 3 failed | `radian_host::subagent_stop::cancelling_the_call_stops_what_the_child_started` (the escaped grandchild was still alive after 10 s); `radian-exec::subagent::a_cancelled_parent_turn_stops_the_command_the_childs_shell_started` and `..._stops_the_childs_background_job` (both timed out waiting for the pid to go) |
| TS-2 | the drop guard sends SIGKILL straight away | ✅ 1 failed | `radian_host::subagent_stop::dropping_the_call_stops_what_the_child_started` (the escaped grandchild was still alive). `radian_host::subagent_drop::dropping_the_call_leaves_no_grandchild_alive` still passes: it pins the fallback. |
| TS-3 | `radian exec` installs no SIGTERM listener (the kernel's default action kills the child at once; this mutation lives in `crates/radian-exec/src/events.rs`, see that crate's MUTATIONS.md) | ✅ 2 failed | both `radian-exec` tests above (the command and the job outlived the cancelled turn). The two `subagent_stop` tests still pass: their stub handles SIGTERM itself. |
## ADR 0026: protocol from the models.dev `npm`, hard-coded per-protocol base URLs, the bundled snapshot, Responses with an API key (2026-10-17)

Each mutation was applied in place under `src/`, the catching test binary was run alone with
`--no-fail-fast`, and the source was restored from a `cp` in `/tmp/mut/` and confirmed with `cmp`.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| R2-1 | `ProviderRef::resolve_protocol` ignores the catalog (always `Ok(descriptor.protocol_for(model))`) | ✅ 4 failed | `provider::tests::the_catalog_npm_decides_the_protocol_and_an_unknown_one_is_refused`, `provider::tests::the_bundled_snapshot_decides_the_protocol_with_no_cache`, `provider::tests::the_catalogs_api_field_is_never_used_for_the_endpoint`, `responses_api_key_route::a_catalog_responses_model_on_a_built_in_descriptor_uses_the_api_key_route` |
| R2-2 | the catalog's `api` field is parsed again and used as the endpoint (`resolve_route_for`) | ✅ 1 failed | `provider::tests::the_catalogs_api_field_is_never_used_for_the_endpoint` |
| R2-3 | the bundled snapshot level is dropped from `load`/`resolve_offline` (no cache → nothing) | ✅ 4 failed | `catalog::no_cache_and_a_failed_fetch_uses_the_bundled_snapshot`, `catalog::offline_never_fetches`, `chatgpt_window::an_offline_switch_to_a_chatgpt_model_resolves_the_table`, `chatgpt_window::a_descriptor_without_numbers_is_answered_by_the_catalog_not_the_table` |
| R2-4 | `build_model` ignores `chatgpt_oauth` and always builds the ChatGPT OAuth model (reuses its header builder) | ✅ 3 failed | `responses_api_key::an_api_key_responses_route_sends_the_bearer_and_no_codex_headers`, `responses_api_key::a_text_turn_and_a_tool_turn_run_over_the_api_key_responses_route`, `responses_api_key_route::a_catalog_responses_model_on_a_built_in_descriptor_uses_the_api_key_route` |
| R2-5 | `Catalog::npm` drops the provider-level fallback (entry-level only) | ✅ 2 failed | `catalog::tests::the_npm_is_the_entrys_value_then_the_providers`, `catalog::tests::the_bundled_snapshot_parses_with_its_date` |
| R2-6 | `Protocol::from_npm` maps an unknown `npm` to chat instead of refusing | ✅ 3 failed | `provider::tests::the_catalog_npm_decides_the_protocol_and_an_unknown_one_is_refused`, `provider::tests::the_bundled_snapshot_decides_the_protocol_with_no_cache`, `responses_api_key_route::a_catalog_responses_model_on_a_built_in_descriptor_uses_the_api_key_route` |
| R2-7 | the `protocol from models.dev` notice never fires | ✅ 1 failed | `provider::tests::the_catalog_overruling_the_table_is_noticed` |
| R2-8 | a resynced table row drifts (`grok-4.7` back to `go_openai`) | ✅ 2 failed | `opencode_go_models::the_opencode_go_builtins_match_upstream`, `provider::tests::opencode_go_maps_models_to_the_three_protocols` |
| R2-9 | the `openai` table drifts from the snapshot (its models back to chat) | ✅ 2 failed | `provider::tests::the_built_in_table_agrees_with_the_bundled_snapshot`, `provider::tests::the_built_ins_are_registered` |

Commands: `timeout 300 cargo test -p radian-host --lib --locked --no-fail-fast <name>`;
`timeout 300 cargo test -p radian-host --locked --test <file>`. 9/9 caught.

## ADR 0034: `pane.report_metadata`, `state_labels`, `pane.report_agent_session` (2026-10-17)

Base `2e513ef` · branch `dogfood/s4-herdr`. Each mutation was applied in place, only the catching
binary was run (`--no-fail-fast`), and the source was restored from a `cp` in `/tmp/s4/` and
confirmed with `cmp`.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| H34-1 | `token_diff` drops the "changed" guard: every known key is sent every time | ✅ 2 + 1 failed | `radian-tui cli::herdr::tests::each_metadata_trigger_reports_only_the_changed_key`, `...::a_key_that_goes_unknown_is_cleared`; pty `pty_herdr_reports_metadata_changes` |
| H34-2 | a `cost` token is added to `metadata_tokens` | ✅ 1 + 1 failed | `radian-tui cli::herdr::tests::each_metadata_trigger_reports_only_the_changed_key`; pty `pty_herdr_reports_metadata_changes` |
| H34-3 | `blocked_label` is always `"blocked"` (the H2 category is dropped) | ✅ 1 + 2 failed | `herdr::tests::the_blocked_label_is_the_h2_category`; pty `pty_herdr_labels_a_question_as_question`, `pty_herdr_reports_metadata_changes` |
| H34-4 | `Reporter::session` is not queued at startup (no `report_agent_session`) | ✅ 1 failed | pty `pty_herdr_reports_the_session_identity` |

Commands: `timeout 900 cargo test -p radian-tui --locked --lib --no-fail-fast cli::herdr`;
`timeout 900 cargo test -p radian-host --locked --lib --no-fail-fast herdr`;
`timeout 900 cargo test -p radian-tui --locked --test pty --no-fail-fast herdr`. 4/4 caught.
## the `-m provider/model` hint (`startup::provider_in_model_hint`)

The form is `--provider <p> --model <m>`; a `-m`/`--model` value that looks like `<known
provider>/<rest>` with no `--provider` gets one startup hint and nothing else. Mutations were
applied in place, each restored from a `cp` in `.radian/scratch/mut/` and confirmed byte-identical
with `cmp`; MP-2 also rebuilds the spawned binary (`cargo build -p radian-tui --bin radian`)
because `common::radian_bin()` refuses a stale one.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| MP-1 | the known-provider check is gone (every `a/b` warns) | ✅ 1 failed (`left: Some("…--provider some-org --model foo?")`, `right: None`) | `startup::tests::the_model_hint_fires_only_for_a_known_provider_with_no_provider_flag` |
| MP-2 | the `--provider` guard is gone (an explicit `--provider` still warns) | ✅ 1 failed (`left: Some("…--provider anthropic --model claude-x?")`, `right: None`) | the same unit test; and `radian-exec::exec::the_model_provider_hint_fires_once_without_a_provider_flag` (`err.contains("did you mean")` where none is allowed) |

Commands: `timeout 570 cargo test -p radian-host --lib --locked --no-fail-fast the_model_hint`
(both); `timeout 570 cargo test -p radian-exec --test exec --locked --no-fail-fast
the_model_provider_hint` (MP-2). MP-1 is caught only by the unit test: the CLI test's cases use
`opencode-go/x` and `anthropic/claude-x`, both real descriptors, so warning on every slash would
not change its answer.

## ADR 0027 slice ④: `Headers` usage from rate-limit response headers (2026-10-17)

Base `c09b614` · branch `dogfood/footer-s4`. Each mutation was applied in place, only the catching
binary was run (`--no-fail-fast`), and the source was restored from a `cp` in `/tmp` and confirmed
with `cmp`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| UH-1 | misread one header name (`anthropic-ratelimit-requests-remaining` → `...-request-remaining`) | ✅ 1 failed (rc=101) | `headers_usage::tests::the_anthropic_headers_form_two_windows` (the requests window loses its remaining; the exact-name match finds nothing) |
| UH-2 | the `anthropic` descriptor drops its `Headers` source (`usage: None`) | ✅ 1 failed (rc=101) | `provider::tests::only_the_endpoint_providers_declare_a_usage_source` (the slice's "descriptor declares it" acceptance) |

Commands: `timeout 300 cargo test -p radian-host --locked --no-fail-fast --lib headers_usage`
(UH-1); `timeout 570 cargo test -p radian-host --locked --no-fail-fast --lib provider::` (UH-2,
run green on the fixed code: 30 passed). UH-1 is the slice's required "misread one header name"
mutation.
## Footer command runner: `ETXTBSY` retry + `EPIPE` tolerance (2026-10-17, linux-flakes)

The out-of-process footer `command` runner (`src/command_footer.rs`) had two Linux races, both
measured in this worktree before the fix (30 sequential
`cargo test -p radian-host --test command_footer` runs: 6/30 suites failed in the quiet loop,
21/30 in the loaded loop; every failure a first-tick `store.get("thing").unwrap()` on `None`,
plus one wrong-notice-text failure at `command_footer.rs:460`): a spawn failing with
`ExecutableFileBusy` (os error 26, `ETXTBSY` — another thread's `fork` inherited the script's
write fd for the instant between its create and close), and an `EPIPE` ("stdin failed") on the
stdin write when a fast `printf` fixture exits before the parent writes. Both hit a real user
too (a fast command that ignores stdin, or a save of the script at the tick moment), so the
fix is in the product code: `spawn_retrying_txtbsy` retries an `ExecutableFileBusy` spawn up
to 20 times 10 ms apart, and `write_stdin` tolerates `BrokenPipe`. Mutations applied in place
in this worktree, each restored from `/tmp/command_footer.fixed` and confirmed with `cmp`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| CF-R1 | `spawn_retrying_txtbsy` spawns once (the retry loop is gone) | ✅ 1 failed (`the retry outlives the writer's hold: "the footer command could not be started"`) | `command_footer::tests::a_txtbsy_spawn_is_retried_until_the_writer_closes` (Linux-only: holds the script open for writing, drops the hold after 30 ms against the 200 ms retry budget) |
| CF-R2 | `write_stdin` reports every write error (the `BrokenPipe` arm is gone) | ✅ 1 failed (`EPIPE on stdin is tolerated: "the footer command's stdin failed"`) | `command_footer::tests::a_closed_stdin_pipe_is_not_a_run_failure` (drops the read end of a `UnixStream` pair, saturates the send buffer so the kernel must report `EPIPE`; no timing involved) |

Commands: `timeout 300 cargo test -p radian-host --lib --no-fail-fast a_txtbsy` (CF-R1);
`timeout 300 cargo test -p radian-host --lib --no-fail-fast a_closed_stdin` (CF-R2). 2/2 caught.
The pty root-refusal drain fix in the same task is test-helper-only
(`crates/radian-tui/tests/pty.rs`: drain the master after `try_wait` succeeds), so it has rate
proof but no mutation entry.
## linux-unix-sockets item 1: the `<runtime>` Unix-socket sentence follows the backend (2026-10-17)

Base `80fdc58d` · in place in this worktree, the source restored from a `cp` in `/tmp/unixsock-mut/`
and confirmed byte-identical with `cmp`; only the catching binary was run (`--no-fail-fast`). The
spawned `radian` binary was rebuilt with the mutation in place (the CLI harness refuses a stale
binary) and rebuilt again after the restore (the restore-build log matches the pre-mutation one).

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| U1 | the `<runtime>` flag back to the tier (`unix_sockets_restricted: workspace_mode` in `session/open.rs`) | ✅ 1 failed (`bwrap does not restrict Unix sockets, so the sentence must be gone`, the false sentence is back on Linux) | `radian-exec cli::the_workspace_system_prompt_unix_socket_sentence_follows_the_backend` |

Commands: `timeout 570 cargo test -p radian-exec --locked --test cli --no-fail-fast
the_workspace_system_prompt_unix_socket_sentence_follows_the_backend` (after
`cargo build -p radian-tui --bin radian --locked`, both times). The prompt unit test
(`radian-host --lib prompt::the_runtime_unix_socket_sentence_follows_the_backend`) does not touch
`open.rs`, so the end-to-end workspace-session test is the one that reds this wiring mutation;
on macOS the same test asserts the sentence stays (seatbelt still restricts).

## ADR 0021 D5/D7: the model loses its write tools; `memory_search` mounts past the budget (2026-10-20)

Worktree `.radian-worktrees/memory-tools`, branch `pi/memory-tools`. Mutations applied in
place, each restored from a `cp` in `/tmp/memtools-mut/` and confirmed byte-identical with
`cmp`; only the catching binary was run (`--no-fail-fast`). M1/M2/M5 change the spawned
binary, so each ran after `cargo build -p radian-tui --bin radian --locked` (the CLI harness
refuses a stale binary) and the binary was rebuilt again after the restore (M5's restore-build
log matches the pre-mutation one; the over-budget and headless set tests pass on it).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| MT-1 | `injection::exceeds_budget`恒为 `true`（总是挂载） | ✅ 1 failed (`a memory within the budget mounts no search`) | `radian-exec::cli::memory_search_mounts_only_past_the_injection_budget`（预算内那半） |
| MT-2 | `injection::exceeds_budget`恒为 `false`（永不挂载） | ✅ 1 failed (`a memory past the budget mounts the search`) | `radian-exec::cli::memory_search_mounts_only_past_the_injection_budget`（超预算那半） |
| MT-3 | `memory_rules()` 去掉 `memory_search` 的 `allow` | ✅ 1 failed（`memory_search` 不再是 `Allow`） | `radian-permissions::profile::memory_search_is_allowed_in_every_tier_and_no_memory_tool_asks` |
| MT-4 | `default.md` 删掉 Remembering 一节 | ✅ 1 failed（`contains("/memory add")`） | `radian-host::prompt::the_builtin_default_role_carries_its_rules` |
| MT-5 | `memory_save` 的 spec 加回 `specs()` | ✅ 1 failed (`only \`memory_search\` mounts, never \`memory_save\``) | `radian-exec::cli::memory_search_mounts_only_past_the_injection_budget`（超预算那半的缺席断言） |

5/5 抓住。注：MT-5 的空内存钉住集合（headless `skill`+`subagent`、TUI 15 个）看不到它——
预算内整个 memory provider 不挂载，`specs()` 根本不被咨询；缺席断言放在超预算的那半里，
这也是唯一能看见 spec 回归的地方。

## mcp-cwd: an MCP server entry's `cwd` (2026-10-22)

Worktree `.radian-worktrees/mcp-cwd`, branch `radian/mcp-cwd` (base `ba3d7f10`). Every
mutation edits `src/mcp.rs` in place, restores from `.radian/mcp.rs.bak` with `cp` (never
`git checkout`), and runs the single catching binary with `--no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| MC-1 | a relative `cwd` resolves against the process working directory (`resolve_cwd` drops `base_dir.join`) | ✅ failed | `mcp::tests::cwd_resolves_relative_to_the_declaring_file_in_every_form` |
| MC-2 | the missing-directory check is gone (`push_server` always pushes) | ✅ failed | `mcp::tests::a_missing_cwd_skips_one_server_with_one_warning` |
| MC-3 | `cwd` skips `${VAR}` expansion (`let expanded = raw;`) | ✅ failed | `mcp::tests::cwd_expands_env_tilde_and_blanks_project_credentials` |
| MC-4 | `cwd` expands in `LoadScope::User` even for a project entry (credential blanking lost) | ✅ failed | same test (the `ANTHROPIC_API_KEY` cwd halves) |
| MC-5 | the resolved `cwd` never reaches `McpServerConfig` (`config.cwd = cwd` removed) | ✅ failed | `mcp_cwd::the_server_process_starts_in_the_configured_directory` |
| MC-6 | a bare `~` is no longer expanded (falls through to the declaring directory) | ✅ failed | `cwd_expands_env_tilde_and_blanks_project_credentials` |

6/6 caught; the file was byte-identical after the restore. Commands: `cargo test -p
radian-host --lib mcp::tests::<name>` for MC-1..MC-4 and MC-6, `cargo test -p radian-host
--test mcp_cwd` for MC-5.

## TK0055: several keys for one provider (2026-10-24)

Worktree `.radian/arc/worktrees/TK0055-multi-key`, branch `work/TK0055-multi-key`. Each
mutation edits the named file in place, restores from a copy under `.radian/bak/` with `cp`
(never `git checkout`), and runs the catching test with `--profile sandbox --no-fail-fast`.
Every restore was confirmed byte-identical with `diff -q`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0055-M1 | the pool picks the **last** key that is not resting (`.find` → `.rfind`) | ✅ 1 failed | `radian-host::key_pool::tests::the_pool_picks_the_first_key_and_keeps_it` |
| TK0055-M2 | a `503` rests the key (`status: Some(429)` → `Some(429 \| 503)`) | ✅ 1 failed | `radian-host::key_pool::tests::only_a_quota_429_rests_its_key` |
| TK0055-M3 | the cache key drops the key fingerprint (`cache_key` ignores it) | ✅ 1 failed | `radian-host::key_pool_failover::a_quota_429_switches_to_the_next_key_for_the_retry` (the retry still sends `sk-first`) |
| TK0055-M4 | `resolved_keys` returns only the first key (`.take(1)`) | ✅ 1 failed | `radian-host::auth::tests::a_keys_list_resolves_every_key_in_order_with_names` |
| TK0055-M5 | `/logout` of the first key removes the entry instead of promoting the next | ✅ 1 failed | `radian-host::auth::tests::adding_and_removing_keys_keeps_key_as_the_first` |

5/5 caught.
## TK0090: `env_docs` stops reporting ordinary constants (2026-10-24)

Worktree `.radian/arc/worktrees/TK0090-env-docs-const-uses`, branch
`work/TK0090-env-docs-const-uses` (base `982f0801`, `origin/main`). Each mutation edits
`crates/radian-host/tests/env_docs.rs` (M3: `docs/user/environment.md`) in place, restores from
a `cp` under `.radian/` (never `git checkout`), and runs
`cargo nextest run -p radian-host --locked --profile sandbox --no-fail-fast -E 'binary(env_docs)'`
with `--no-fail-fast`.

Before the change the scanner found 27 names (`.radian/env-names-before.txt`); the new scanner
finds 25 (`.radian/after-r3.sorted`). The two that drop out, `RADIAN_SESSION_ID` and
`RADIAN_CWD`, exist only as `pub const INHERITED_ENV: &[&str] = &["RADIAN_SESSION_ID",
"RADIAN_CWD"]` (`crates/radian-host/src/command_footer.rs:83`), whose identifier no environment
call passes; the code only sets them with literal `.env("RADIAN_SESSION_ID", …)` /
`.env("RADIAN_CWD", …)` writes (lines 403/405), and a write literal is not a source. They are
set-only names (their manual row says "set by radian, never read back"), so the row stays; the
scanner no longer discovers them. Every other name is unchanged.

The constant rule is uniform: a `pub const` names a variable only when its identifier is an
environment-call argument; a call's literal argument never promotes a constant's value, scalar or
list. M1–M5 were run against that scanner.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0090-M1 | count every constant's literals again (the old rule: every all-caps literal on a `pub const` line is a variable, `if true`) | ✅ 7 failed | `a_heading_constant_is_not_environment_variables` (`TODO`, `DOING`, `VERIFYING`), `a_constant_used_outside_an_environment_call_yields_nothing` (`HELLO_WORLD`), the scalar and list literal-promotion tests (`TODO`), and the positive tests, which also report their unused decoy constants (`RADIAN_HOME_LABEL` in both `RADIAN_HOME` tests, `RADIAN_CHILD_HEADERS_LABEL`) |
| TK0090-M2 | drop `.env(` from the accepted environment calls (`ENV_CALLS`) | ✅ 1 failed | `a_constant_set_on_a_child_yields_its_literal` (no name found) |
| TK0090-M3 | delete the `RADIAN_HOME` row from `docs/user/environment.md` | ✅ 1 failed | `every_env_var_appears_in_environment_md`: `these variables are missing from docs/user/environment.md: ["RADIAN_HOME"]` |
| TK0090-M4 | let a constant's literal count whenever an environment call names it (`named_by_identifier \|\| call literals contains(literal)`) | ✅ 2 failed | `a_scalar_constant_named_only_by_a_write_literal_yields_nothing` and `a_list_constant_named_only_by_a_write_literal_yields_nothing` (both report `TODO`) |
| TK0090-M5 | restore the removed list exception (a list-typed constant's entry counts when an environment call names the same literal) | ✅ 1 failed | `a_list_constant_named_only_by_a_write_literal_yields_nothing`: left `["TODO"]`, right `[]` (the review's `BOARD_LABELS` case) |

5/5 caught. M1 stays a sample-only catch: `COLUMN_HEADINGS` is `pub(crate) const`, which the old
substring `pub const` never matched, so the real scan is green under M1; the sample tests pin the
rule. The decoy constants on the positive tests are what make them fail under the old
unconditional-constant rule.

## TK0170: opencode-go chat and Responses move to the `/inference/go/v1` base (2026-10-25)

Worktree `.radian/arc/worktrees/TK0170-opencode-go-inference-v1`, branch
`work/TK0170-opencode-go-inference-v1`. The mutation edits `OPENCODE_GO_OPENAI_BASE` in
`crates/radian-host/src/provider.rs` back to the old `…/inference/go/openai/v1`, restores from
`.radian/provider.rs.bak` with `cp` (never `git checkout`), and runs
`cargo nextest run -p radian-host --locked --profile sandbox --no-fail-fast`. The restore was
confirmed byte-identical with `diff -q`.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0170-M1 | `OPENCODE_GO_OPENAI_BASE` points at the old `https://opencode.ai/inference/go/openai/v1` | ✅ 6 failed | `provider::tests::a_route_picks_the_adapter_base_url_and_headers`, `provider::tests::an_explicit_base_url_survives_the_default_moving`, `provider::tests::opencode_go_maps_models_to_the_three_protocols`, `provider::tests::the_catalogs_api_field_is_never_used_for_the_endpoint`, `provider::tests::the_bundled_snapshot_decides_the_protocol_with_no_cache`, and `opencode_go_models::the_opencode_go_builtins_match_upstream` |

1/1 caught.

## TK0172: reasoning replay falls back to the bundled snapshot (2026-10-25)

Worktree `.radian/arc/worktrees/TK0172-reasoning-replay-snapshot-fallback`, branch
`work/TK0172-reasoning-replay-snapshot-fallback`. The mutation makes
`catalog::catalog_reasoning_replay` cache-only again (`read_cache(&cache_path())?`), restores from
`.radian/catalog.rs.bak2` with `cp` (never `git checkout`), and runs
`cargo nextest run -p radian-host --locked --profile sandbox --test reasoning_replay --test
reasoning_replay_no_cache --no-fail-fast`. The restore was checked with `diff -q` (empty).

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0172-M1 | `catalog_reasoning_replay` returns `None` without a cache (reads `read_cache(&cache_path())` only, no bundled-snapshot fallback) | ✅ 1 failed (rc=100) | `reasoning_replay_no_cache::with_no_cache_the_bundled_snapshot_decides_reasoning_replay` (`assertion left == right` at "with no cache, the bundled snapshot still decides replay"; the cache-backed `reasoning_replay::the_catalog_and_a_named_override_decide_reasoning_replay` still passes, as it should) |

1/1 caught.

## TK0191: chat `max_tokens` and `opencode-go` streamed usage (2026-10-26)

Branch `work/TK0191-chat-max-tokens-and-opencode-go-usage`. Each mutation was applied in place,
the named test binary was run, and the source was restored from a copy under `.radian/`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0191-M2 | `default_include_usage` returns `None` for every provider (the old host-only default) | ✅ 1 failed (rc=100) | `provider::tests::only_the_opencode_go_descriptor_asks_for_streamed_usage_by_default`; `responses_api_key_route::a_catalog_responses_model_on_a_built_in_descriptor_uses_the_api_key_route` fails on `chat.include_usage == Some(true)` too |
| TK0191-M3 | `build_window` passes `None` to `output_reservation`/`input_budget` (R ignores the value the request sends), while `Window.request_output` still carries it | ✅ 1 failed (rc=100) | `catalog::a_chat_route_reserves_its_catalog_max_tokens` (R 16 384 and B 922 000 instead of R 250 000 and B 750 000) |

## TK0192: `RADIAN_EXTRA_BODY` and the A/B body arm (2026-10-26)

Branch `work/TK0192-extra-request-body-env-and-ab-arm`. Each mutation was applied in place,
the named test binary was run, and the source was restored from a copy under `.radian/mut/`
(`cmp` empty, `git status --short` back to the same set).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0192-M1 | the chat adapter merges `settings` before `config.extra_body` (`for source in [&settings, &config.extra_body]`), so the env body wins over the request's own setting | ✅ 1 failed (rc=100) | `extra_body::a_request_setting_wins_over_the_extra_body_and_the_warning_names_the_key` (`assertion left == right` at "the request value wins": 0.1 instead of 0.9) |
| TK0192-M2 | `parse_extra_body` skips the reserved-key check | ✅ 1 failed (rc=100) | `extra_body::reserved_keys_and_non_objects_are_refused` (`expect_err` on an `Ok` value for `{"messages":[]}`) |

## TK0200: `RADIAN_EXTRA_BODY` reserved keys per route (2026-10-27)

Branch `work/TK0200-extra-body-reserved-per-route`. Each mutation was applied in place, the
named test binary was run, and the source was restored from a copy under `.radian/mut/`
(`cmp` empty).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0200-M1 | the startup check in `radian-exec/src/setup.rs` consults all three adapters again (the old union) instead of `route.adapter()` | ✅ 1 failed (rc=100) | `radian-exec::cli::the_extra_body_max_tokens_is_allowed_on_a_chat_route_and_refused_on_anthropic` (the chat half exited 2: ``RADIAN_EXTRA_BODY sets `max_tokens`…``) |
| TK0200-M2 | the `/model` switch check in `radian-tui/src/cli/loop_context.rs` is removed | ✅ 1 failed (rc=100) | `radian-tui cli::loop_context::tests::a_model_switch_to_a_route_that_reserves_an_extra_body_key_is_refused` (the effect fell through to the credential/budget path instead of sending the refusal) |

## TK0213: a named provider resolves protocol and endpoint per model (2026-10-27)

Branch `work/TK0213-per-model-protocol-rules`. Each mutation was applied in place, the named
test binary was run, and the source was restored from a copy under `.radian/mut/` (`cmp` empty).

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0213-M1 | `NamedProvider::resolve_model` never takes a rule (`.filter(|_| false)` on the first-match find), so the model list and `kind` answer instead | ✅ 1 failed (rc=100) | `provider::named_route_tests::a_rule_beats_the_model_list_and_the_model_list_beats_the_kind` (rule expected `anthropic`, got `openai-compatible`); `a_rules_only_table_overlays_an_auth_provider_and_needs_one` fails on the same rule |
| TK0213-M2 | `DEFAULT_PREFER` lists chat first (`openai-compatible`, `openai-responses`, `anthropic`) | ✅ 1 failed (rc=100) | `provider::named_route_tests::the_default_prefer_picks_anthropic_and_responses_over_chat` (the claude entry picks chat) |
| TK0213-M3 | `endpoint_from_path` returns the bare origin, dropping the advertised path | ✅ 1 failed (rc=100) | `provider::named_route_tests::the_endpoint_is_the_origin_plus_the_stripped_advertised_path` and `the_line_names_protocol_endpoint_and_source`; `named_model_routing::a_named_provider_sends_to_the_api_its_model_list_advertised` fails on the Anthropic request path |
| TK0213-M4 | `Protocol::from_kind` drops pi's aliases (`openai-completions`, `anthropic-messages`) | ✅ 1 failed (rc=100) | `provider::named_route_tests::aliases_are_accepted_and_lines_print_radian_names`; `a_rules_only_table_overlays_an_auth_provider_and_needs_one` fails on `env.RADIAN_KIND = "openai-completions"` |
| TK0213-M5 | `startup_model_list_at` swallows a failed fetch (returns `(None, None)` instead of the one line) | ✅ 1 failed (rc=100) | `catalog::provider_models_tests::a_failed_startup_fetch_falls_to_kind_with_one_line` (`expect("one startup line")` on `None`) |

## TK0214: a named provider probes and remembers its protocol (2026-10-27)

Branch `work/TK0214-probe-and-remember-protocol`. Each mutation was applied in place, the named
test binary was run, and the source was restored from a copy under `.radian/` (`cmp` empty).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0214-M1 | `probe_status` accepts `Unavailable` and `PROBE_STATUSES` adds 429, so a rate limit probes | ✅ 1 failed (rc=100) | `named_probe::a_non_probe_status_never_probes` (the 429 round sent a probe request) |
| TK0214-M2 | the rule-sourced failure no longer names the rule (`rule_pattern` forced to `None`) | ✅ 1 failed (rc=100) | `named_probe::a_rule_sourced_failure_names_the_rule_and_does_not_probe` (the error text has no `rule claude/*`) |
| TK0214-M3 | the writer's already-present checks are disabled (outer `already_present` and the array's `match` test), so a second save duplicates the rule | ✅ 2 failed (rc=100) | `config::providers_write::tests::an_existing_match_is_left_alone` and `a_missing_file_is_created_with_the_rules_only_table` (two `kind = "anthropic"` rules / `Written` instead of `AlreadyPresent`) |
| TK0214-M4 | `probe_candidates` stops putting the model-list endpoints first (`&& false` on that loop) | ✅ 1 failed (rc=100) | `named_probe::a_model_list_endpoint_is_probed_before_the_base_url` (responses tried before the advertised anthropic endpoint) |
| TK0214-M5 | a failed probe returns the first error without the tried list and the rule hint (`Err(first)`) | ✅ 1 failed (rc=100) | `named_probe::every_attempt_failing_keeps_the_first_error_and_writes_nothing` (no `tried …` line) |
| TK0214-M6 | `remember` always queues the probed route instead of respecting a queued switch | ✅ 1 failed (rc=100) | `named_probe::a_queued_model_switch_is_not_undone_by_a_probe` (the old route is the last queued change) |
| TK0214-M7 | `save_rule` ignores the read-only verdict (`false` instead of `config_target.read_only`) | ✅ 1 failed (rc=100) | `named_probe::a_read_only_config_is_not_saved` (the schema-99 file was rewritten) |

## TK0216: the host passes its tool set into the session's descriptions (2026-10-27, branch `work/TK0216-shell-description-follows-tool-set`)

ADR 0043 §4: a tool description names another tool only when that tool is mounted. `HostTools::open_session`
passes `ToolPresence::from_allow(self.tool_allow)` to `CodingTools::open_session_with`; the mutation
drops that link. It was applied by hand to `crates/radian-host/src/tools.rs`, the `radian` binary was
rebuilt, the named test ran, and the source was restored from `.radian/host-tools.rs.bak`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0216-M5 | `HostTools::open_session` opens coding tools with `ToolPresence::ALL`, ignoring `tool_allow` | ✅ 1 failed (rc=100) | `an_explore_child_has_only_the_three_read_tools_and_write_is_refused` (`crates/radian-exec/tests/subagent.rs`: the `read,glob,grep` child's `read` description named `patch`) |
| TK0216-M6 | `subagent::spec` always keeps the `explore` parenthetical (`if true` instead of the read/glob/grep presence) | ✅ 1 failed (rc=100) | `subagent::tests::the_explore_parenthetical_names_only_mounted_tools` (the `subagent,goal` set still named `read glob grep`) |


## TK0220: a probe uses the thinking level in force and stops on a non-refusal candidate (2026-10-28, branch `work/TK0220-probe-current-thinking-and-stream-start`)

ADR 0026 D10: `probe` builds each candidate from the `thinking` level it receives (shared with
`model_for` through `route_context`), and it moves to the next protocol only when the candidate
error is one of `PROBE_STATUSES`. Each mutation was applied in place, the `named_probe` binary
was run, and the source was restored from `.radian/model.rs.good` (`cmp` empty).

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0220-M1 | the candidate loop builds with `self.ctx.args_for_route` again, so it sends the startup level | ✅ 2 failed (rc=100) | `named_probe::a_probe_candidate_carries_the_thinking_level_in_force` (candidate body was `adaptive`, not `enabled:4095`) and `a_remembered_probe_follows_a_later_thinking_change` (`high` request was `adaptive`) |
| TK0220-M2 | the non-refusal check is removed, so every candidate error continues to the next protocol | ✅ 2 failed (rc=100) | `named_probe::a_candidate_that_streams_then_fails_ends_the_probe` (a third, Responses request was sent) and `a_candidate_that_is_not_a_refusal_ends_the_probe` (the 429/500 rounds each sent a third request) |

## TK0221: every rules-only provider table is refused at startup without its auth entry (2026-10-28, branch `work/TK0221-rules-only-table-refused-at-startup`)

ADR 0026 D10: `config::load` checks **every** `[providers.<name>]` table, not only the provider
the run resolves. `provider::rules_only_provider_without_auth` names the first table with no
`kind` and no `base_url` whose name has no `auth.json` entry. Each mutation was applied in place
to `crates/radian-host/src/config/mod.rs` or `provider.rs`, the `radian` binary was rebuilt (the
exec tests refuse a stale one), the named tests ran, and the source was restored from
`.radian/config-mod.rs.bak` / `.radian/provider.rs.bak` (`cmp` empty).

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| TK0221-M1 | the startup call is removed (`check_rules_only_providers(&config)?` commented out), so only the per-resolution backstop remains | ✅ 4 failed (rc=100) | `a_rules_only_provider_table_without_an_auth_entry_is_refused_at_startup` (exit 0, the turn ran), `a_reasoning_replay_only_provider_table_without_an_auth_entry_is_refused_at_startup` and `a_read_only_config_with_an_orphan_rules_only_table_is_refused_at_startup` (both saw exit 2 for "no model", not the table), and `pty_a_rules_only_provider_table_without_an_auth_entry_is_refused_at_startup` (the app kept running) |
| TK0221-M2 | the `!decl.is_full()` guard is dropped, so a full `kind` + `base_url` declaration without an auth entry is refused | ✅ 1 failed (rc=100) | `a_full_provider_table_without_an_auth_entry_starts` (exit 2, the table was refused) |
| TK0221-M3 | the auth predicate is inverted (`has_auth_entry(name)` instead of `!has_auth_entry(name)`), so an entry counts as missing | ✅ 2 failed (rc=100) | `a_rules_only_provider_table_with_an_auth_entry_starts_without_a_warning` (exit 2) and `provider::named_route_tests::a_rules_only_table_without_an_auth_entry_is_found_for_every_table` |
