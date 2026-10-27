# 不做的两种协议：Mistral Conversations 与 pi-messages

- 状态：idea（当前结论：不做）
- 涉及 ADR：0026
- 来源：pi 的 `mistral-conversations`、`pi-messages`

## 背景

- `mistral-conversations`：Mistral 同时提供 OpenAI 兼容 chat，radian 现有的 openai-compatible adapter 已能用。
- `pi-messages`：pi 自家 Radius 网关的私有协议，请求体是 pi 内部消息结构（`{ model, context, options }` POST 到 `<baseUrl>/messages`，SSE 返回 pi 的消息事件），不是公开的模型接口。

## 重新考虑的条件

- Mistral：出现只有原生接口才有的能力，而且有用户需要。
- pi-messages：radian 要接入 Radius 网关。

## 验收

无；条件出现时改成具体条目再定。
