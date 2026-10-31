# journal 不记模型的停止原因

- 状态：idea
- 涉及 ADR：0006（session JSONL）、0007（usage）、0010（durable 与 live 事件）
- 来源：2026-10-31 本地线路统计截断调用和填满窗口的响应时发现

## 现象

核心在 live 事件 `ModelResponseReceived` 里有 `stop_reason`（`crates/radian-core/src/event.rs`），turn 按它决定截断调用怎么处理（ADR 0017），但它不入 journal；持久化的 `ModelUsageRecorded` 没有这个字段。`TurnFinished` 只在 turn 以 `incomplete` 结束时带 `max_tokens` 之类的 reason，一个 turn 中间被截断又继续的响应就没有记录。eval 指标（`benchmarks/journal_metrics.py`）只能用"有 `error_code = truncated` 的工具结果，或 usage 总量 ≥ C − 16"反推 MaxTokens。

## 待决方向

1. `ModelUsageRecorded` 加一个闭集字段 `stop_reason`（`stop` / `max_tokens` / `tool_calls` / `unknown:<原文>`，与 `TurnFinished.reason` 用同一张表），`docs/schema` 的 fixture 和参考检查器同步，老 journal 读回时字段缺省为缺失。
2. 不加，继续反推。不推荐：反推在 C 未知的线路上失效。

## 验收

- 一次 `MaxTokens` 响应后 journal 里的 `ModelUsageRecorded.stop_reason = "max_tokens"`；`journal-selfcheck` 回放通过；schema fixture 测试通过；`journal_metrics.py` 新增一列直接读它。
- 变异：字段不写、写错值，各有测试失败。
