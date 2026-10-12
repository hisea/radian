# 变异记录：provider 契约套件（Phase 0 收尾）

日期：2026-10-12。全部变异在一次性检出 `/tmp/contract-mut`（`git worktree add`，HEAD = 契约提交）里做：变异 → 跑对应契约测试 → `git checkout` 恢复，提交前 `git status --short` 为空（已核对）。每一条契约条目都对应一个（或更多）变异，全部被抓住。

| 契约条目 | 变异 | 抓住它的测试 |
|---|---|---|
| 1 纯文本 | C1：流里 content 被丢弃（文本从不入队） | `contract_1_plain_text` FAILED（content 为空；连带 `contract_2` 也败） |
| 2 流式 delta | C2：content 仍入队但 `ModelTextDelta` 事件被抑制 | `contract_2_streamed_deltas` FAILED（deltas 为空） |
| 3 工具调用 | C3：`tool_call_fragment` 不累计 name/arguments（`name = ""; arguments = ""`） | `contract_3a_single_tool_call`、`contract_3b_multiple_tool_calls` FAILED（连带 4/5 也败） |
| 4 分片参数 | C4b：只保留第一片 arguments（`arguments.is_empty()` 才写入）——**单 chunk 调用不受影响**，只有分片装配坏 | `contract_4_split_arguments` FAILED，`contract_3a_single_tool_call` 仍过（变异是条目 4 特异性的） |
| 5 call id 唯一 | C5：`unique_id` 不再改名（原样返回） | `contract_5_call_ids_unique` FAILED（重复 id 与缺失 id 两段） |
| 6 finish reason | C6a：去掉 `"pause_turn"` 映射；C6b：未知 reason 静默落到 `EndTurn` | `contract_6_finish_reasons` FAILED ×2（PauseTurn 段；Other 段） |
| 7 usage 三形态 | C7：usage 读取移回 choices 判断之后（末尾块丢失） | `contract_7a_usage_shapes` FAILED |
| 8 错误分类 | C8：500 归为 `Protocol`（Transport/Protocol 互换） | `contract_8_error_classification` FAILED |
| 9 取消 | C9：`complete` 里的 `tokio::select!` 取消分支被移除 | `contract_9_cancellation` FAILED（预取消段立即失败；在飞段靠看门狗兜住） |
| 10 凭据不泄漏 | C10：`mask()` 不再调用（错误原文透出） | `contract_10_secret_not_leaked` FAILED（错误文本含 secret） |

结论：10 条契约全部有"变异 → 契约测试失败"的证据。没有没抓住的变异；C4b 专门验证了条目 4 的变异不误伤条目 3（分片装配是独立行为）。