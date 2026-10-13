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
