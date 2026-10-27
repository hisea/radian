# 模型兼容开关（按需逐个加）

- 状态：idea
- 涉及 ADR：0026（D10 命名 provider、D11 thinking 映射）、0014（`reasoning_replay`）、0008（配置 schema）
- 来源：pi 的 `compat`（`@earendil-works/pi-ai` `providers/compat-schema`，每个模型约 50 个开关）

## 背景

各家"OpenAI 兼容"服务细节不同。pi 用每模型的 `compat` 开关处理，例如 `thinkingFormat`、`maxTokensField`（`max_tokens` 还是 `max_completion_tokens`）、`supportsDeveloperRole`、`requiresReasoningContentOnAssistantMessages`、`sessionAffinityFormat`（会话粘性 header）、`cacheControlFormat`、`supportsUsageInStreaming`。radian 今天只有 `reasoning_replay` 一个。

## 决定（已定）

- 不一次照搬。只在真实故障出现时加一个开关，附上故障来源。
- 每个开关进一份列表，配置解析器与文档漂移测试共读，与其他闭集一样。

## 待决问题

- 开关放在 `[providers.<name>]`、PL0066 的按模型规则里，还是两处都可以。
- 第一个候选：会话粘性 header（例如 magpie 的 `X-Magpie-Session`），用来保住网关后面的提示缓存。

## 验收

每个开关落地时：手册写全取值与默认值，测试覆盖开与关两种请求体。
