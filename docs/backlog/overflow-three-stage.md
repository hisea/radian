# 超长兜底三档推进（70% → 50% → 35%）

- 状态：idea（先不做）
- 涉及 ADR：0018、0017
- 来源：原 ADR 0018 §6/§9 及验证要求（`git show 4637077:docs/adr/0018-model-catalog-and-auto-compaction.md`，约 84 行起；当前单档实现见 `crates/agent-core/src/turn.rs:1610-1720` 的 `Gain::Half` 与 `overflow_retried`）

## 背景

今天只有一档：超长后压到 `reference / 2` 并重发一次，再超长就 `failed`。原 ADR 0018 描述了三档，但代码实现始终只有一档 50%；ADR 已按代码更正为单档描述。

数据（本机 158 个会话、2026-10-20）：压缩一共发生 1 次，超长兜底 0 次。对手核对：pi 1.3.0 也是「压一次、重试一次」；opencode 的 `SHRINK_STEPS = [0.7, 0.5, 0.35]` 用在**摘要请求**上（第一次超长触发的压缩瞄准被拒请求的 70%；摘要请求自己被拒时按三档丢最旧的 exchange 重发，不多出摘要调用），没有一家对主请求做三轮摘要。若将来要动，更便宜且对标 opencode 的做法是：第一档瞄 70%（一行常量）、摘要请求自身超长时裁它的输入而不是失败。

## 设计（原 spec；做不做未定）

provider 返回**可识别的**上下文超长错误时（`classify_overflow`；只在 400/413 且能识别时；429/5xx 不算），以**第一次**被拒请求的投影 `T_before` 为参照（窗口已知取 `min(T_before, B)`，未知取 `T_before`），按 **70% → 50% → 35%** 三档压缩，每档提交后重发一次：第 1 档 `T_after ≤ 0.7·reference`，第 2 档 `≤ 0.5·reference`，第 3 档 `≤ 0.35·reference`；第 3 档后再被拒本轮 `failed`，错误文本写明「已压缩三次并重发，仍超长」。每档以同一 `reference` 为基数；每档要求 `T_after < T_before` 并通过 `validate_history`；每档一次真实摘要调用、滚动累积、各写各的 `CompactionUsageRecorded`、各走 ADR 0019 的提交；重发不计入 `model_calls`；不计入 ADR 0017 的 3 次重试；`--no-auto-compact` 仍只关主动路径。

## 验收

- `compaction_overflow.rs` 有逐档断言：同一 `reference` 下 70%/50%/35%、每档一次摘要调用与提交、第四档失败、每档 `T_after < T_before`。
- 落地时同分支改 ADR 0018（docs-only commit）并删掉本文件。
