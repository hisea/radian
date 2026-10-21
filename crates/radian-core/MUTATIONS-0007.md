# 变异记录：ADR 0007 / Usage（Phase 0 / A5）

日期：2026-10-12。全部变异在一次性检出 `/tmp/a5-mut`（`git worktree add`，HEAD = 本提交）里做，每项变异后跑对应测试、恢复源码，提交前 `git status --short` 为空（已核对）。失败遍历 `usage_append_failure_sweep` 与 provider http 套件连跑 10 遍通过，完整输出在 `/tmp/a5-runs/usage-{1..10}.log` 与 `/tmp/a5-runs/http-{1..10}.log`。

| # | 变异 | 应抓住的验收 | 结果 |
|---|---|---|---|
| M-D1 | 末尾单独的 usage 块被丢弃（usage 读取移回 choices 判断之后） | AC-A5-2 | ❌ stream 单测：`usage_only_chunks_carry_the_usage`、`usage_in_every_chunk_takes_the_last_non_empty_one`、`usage_maps_openai_and_anthropic_field_names`、`total_is_never_invented…` 全 FAILED（usage 整体丢失） |
| M-D2 | `total` 用 input+output 填充 | AC-A5-3 | ❌ `total_is_never_invented_from_input_plus_output` FAILED |
| M-D3 | `include_usage` 对所有 host 默认开启（`unwrap_or(true)`） | AC-A5-4 | ❌ `include_usage_defaults_to_the_openai_host_only` FAILED |
| M-D4 | `Some(false)` 被忽略（任何情况都发 `stream_options`） | AC-A5-4 | ❌ `the_request_body_carries_stream_options_only_when_enabled` FAILED |
| M-D5 | `details` 被丢弃（`details: None`） | AC-A5-2/6 | ❌ `usage_maps_openai_and_anthropic_field_names` FAILED（details 断言） |
| M-D6 | `ModelUsage::default()` 序列化出全部 null（去掉 `skip_serializing_if`） | AC-A5-6 | ❌ `default_usage_serializes_as_an_empty_object`（另两个 partial 测试同败） |
| M-D7 | usage 根本不写 journal | AC-A5-7 | ❌ `usage_is_journaled_after_the_assistant_message_and_only_when_present`、`usage_append_failure_sweep` FAILED |
| M-D8 | 无 usage 也写一条空记录（`unwrap_or_default`） | AC-A5-7 | ❌ `usage_is_journaled_after_…` FAILED（turn2 多出记录） |
| M-D9 | usage 记录写在 assistant 消息之前 | AC-A5-7 | ❌ `usage_is_journaled_after_…` FAILED（顺序断言） |
| M-D10 | usage append 失败被忽略（不置 RecoveryRequired） | AC-A5-10 | ❌ `usage_append_failure_sweep` FAILED |
| M-D11 | `replay` 把 usage 记录当消息处理（占据 index 槽位，夹在消息间即破坏连续性） | AC-A5-9 | ❌ `usage_append_failure_sweep`、`replay_and_recover_ignore_usage_records` FAILED。**第一版变异是惰性的**（一个不影响行为的公式，8 个测试全过）——是我变异写错，不是测试漏洞；换成"usage 记录推进 index"后才成立。 |
| M-D12 | 400 的提示里不提 `include_usage` | AC-A5-5 | ❌ `a_400_for_stream_options_tells_you_to_turn_it_off` FAILED |

结论：M-D1…M-D12 全部被抓住；没有等价变异或测试漏洞（M-D11 的第一次失败尝试为变异自身问题，已换成成立形态并验证）。