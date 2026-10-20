# `/retry`：模型错误结束回合后重发上一条用户消息

- 状态：decided＼�第一档）
- 涉及 ADR：0017（模型错误只结束这一轮，§5 宿主显示 `turn failed`）、0011（followup）、0003（中断前缀保留）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第一档

## 背景

ADR 0017 下模型错误结束回合，TUI 显示 `turn failed: …` 后回到可输入状态，用户得把上一条消息重打一遍。omp 有 `/retry`。

## 决定（已定）

- `/retry` 只在空闲且**上一轮以模型错误结束**时可用：把上一条用户消息的文本原样作为新的 `followup`（`source = user`）入队，等同用户重新输入；不改写 transcript，ADR 0003 的中断前缀原样保留。
- 其他情况（上一轮正常结束、被取消、还没有过回合）以通知拒绝「nothing to retry」。
- `turn failed` 的通知末尾加一句「/retry to resend」。
- 带附件/`@file` 的消息按当时折叠后的文本重发，不重新读文件。

## 验收

- mock provider 第一次失败：`/retry` 后第二次请求的最后一条用户消息文本与第一次相同；transcript 多出一条用户消息，没有删改。
- 正常结束后 `/retry` 被拒绝；取消后被拒绝。
