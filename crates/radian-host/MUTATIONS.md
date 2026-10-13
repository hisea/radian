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
