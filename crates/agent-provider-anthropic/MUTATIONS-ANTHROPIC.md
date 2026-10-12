# 变异记录：Anthropic adapter 与契约的两个出口（Phase 3 提前）

日期：2026-10-12。全部变异在一次性检出 `/tmp/anthropic-mut`（`git worktree add --detach HEAD`，HEAD = `a5dd07e`）里做：变异 → `cargo build`（由 `cargo test` 隐式完成）→ 只跑对应测试（`cargo test ... -- --exact --no-fail-fast`）→ 恢复文件。跑完 `git status --short` 为空，worktree 已删除。

每条契约条目一个针对性变异（破坏 adapter 对应行为，确认该条契约失败），外加两个出口的四个反向变异。

| 变异 | 目标测试 | 结果 |
|---|---|---|
| M1 在 `finish` 丢掉文本内容 | `contract_1_plain_text` | 抓住（rc=101） |
| M2 停止发出 `ModelTextDelta` | `contract_2_streamed_deltas` | 抓住 |
| M3 忽略 `input_json_delta`（参数恒空） | `contract_3a_single_tool_call` | 抓住 |
| M4 只保留第一个 tool call | `contract_3b_multiple_tool_calls` | 抓住 |
| M5 分片参数只保留第一片 | `contract_4_split_arguments` | 抓住 |
| M6 重复 id 不改名 | `contract_5_call_ids_unique` | 抓住 |
| M7 去掉 `pause_turn` 映射 | `contract_6_finish_reasons` | 抓住 |
| M8 **adapter 丢掉 `total` 却不声明**（静默） | `contract_7a_usage_shapes` | 抓住（未声明字段必须按原值断言） |
| M9 `total = input + output` | `contract_7b_total_not_computed` | 抓住 |
| M10 把 500 归为 `Protocol` | `contract_8_error_classification` | 抓住 |
| M11 去掉 `complete` 的取消分支 | `contract_9_cancellation` | 抓住 |
| M12 不再 `mask()` 错误文本 | `contract_10_secret_not_leaked` | 抓住 |
| M13 出口 1：harness 删掉 `unsupported_reasons` 覆盖（静默不声明） | `contract_6_finish_reasons` | 抓住（`ContentFiltered` 段随即失败） |
| M14 出口 1：`reasons_to_check` 忽略声明、恒返回全部 | 契约 crate `tests::a_declared_reason_is_skipped` | 抓住 |
| M15 出口 2：`expected_usage` 忽略声明、保留脚本值 | 契约 crate `tests::a_declared_unreported_field_must_be_absent` | 抓住 |
| M16 出口 2：去掉「声明字段必须缺席」的断言 | 契约 crate `tests::a_declared_field_that_is_filled_fails` | 抓住（该 `#[should_panic]` 测试不再 panic，于是失败） |

结论：**12 条契约子检查（1/2/3a/3b/4/5/6/7a/7b/8/9/10）全部有「变异 → 测试失败」的证据**（7a 的 M8 证明：未声明就丢字段会被抓；若声明了却填值，由 M16 对应的契约测试证明会被抓）。两个出口各有正反两面：声明了就生效（M14/M15 破坏后失败）、没声明就必须满足（M13/M8 破坏后失败）、声明了却仍然填值必须被抓（M16 破坏后失败）。

adapter 专有行为（`tests/http.rs` 的 32 个用例）与 lib 单测（35 个）另有自己的断言，本次没有为它们逐个做变异（任务单只要求契约条目与出口）。
