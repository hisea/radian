# 变异记录：第一次真实端点实测暴露的缺口（Phase 3 修补）

日期：2026-10-12。全部变异在一次性检出 `/tmp/fixes-mut`（`git worktree add --detach HEAD`，HEAD = 本任务的 `865307d`）里做：变异 → 只跑对应测试（`cargo test ... -- --exact --no-fail-fast`）→ 恢复文件。跑完 `git status --short` 为空，worktree 已删除。

每件事至少一个变异（破坏对应行为，确认对应测试失败）。

| 变异 | 目标测试 | 结果 |
|---|---|---|
| M1 `--header` 无 `=` 的错误里回显整个参数（值泄漏） | `a_header_without_an_equals_sign_is_a_usage_error_that_does_not_echo_it` | 抓住（rc=101） |
| M2 `Authorization` 覆盖被静默接受 | `an_authorization_header_is_refused_so_the_key_has_one_source` | 抓住 |
| M3 `--include-usage` 参数被忽略（恒 `None`） | `include_usage_flag_controls_stream_options` | 抓住 |
| M4 `--header` 值里的 `{session_id}` 不被替换 | `a_header_value_can_reference_the_generated_session_id` | 抓住 |
| M5 `--header` 不写进 config | `a_header_flag_is_sent_and_can_be_repeated` | 抓住 |
| M6 `completion_tokens_details` 又被丢掉 | `the_dogfooded_usage_shape_keeps_completion_tokens_details` | 抓住 |
| M7 `cache_read` 不再从 `prompt_tokens_details.cached_tokens` 回退 | `usage_maps_openai_and_anthropic_field_names` | 抓住 |
| M8 user-agent 改回 `agent-runtime/`（OpenAI adapter） | `the_user_agent_names_radian` | 抓住 |
| M9 user-agent 改回 `agent-runtime/`（Anthropic adapter） | `the_user_agent_names_radian` | 抓住 |

结论：9/9 全抓。超时修复是纯测试代码（放宽 watchdog），没有可变异的产品行为，故不设变异；它的「不脆」由重新跑 `cargo test -p radian-exec --test cli` 全套（32 个测试，含 `every_exit_reaps_the_mcp_children`）证明。

此外，M6/M7 改变的是契约 fixture 现在也覆盖的 `details`/`cache_read`，按构造它们也会让两个 provider 的 `contract_7a_usage_shapes` 失败；**本条未单独实测**（只跑了表里列出的目标测试）。
