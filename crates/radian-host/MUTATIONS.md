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
