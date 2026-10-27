# Gemini 原生接口 adapter（第二步：Vertex）

- 状态：idea（方向已定：做；设计未定）
- 涉及 ADR：0026（D0/D1 descriptor 与 per-model 协议、D10 命名 provider 的 `kind`、D11 thinking 映射）、0014（推理内容与回放）、0007（usage）、0015（上下文预算）、0017（模型错误结束回合）
- 来源：对照 pi 的 10 种 `api`（`@earendil-works/pi-ai` `types.d.ts` 的 `KnownApi`）；radian 现有三个 adapter 覆盖其中 `openai-completions`、`openai-responses`、`anthropic-messages`，`chatgpt` descriptor 覆盖 `openai-codex-responses`

## 背景

radian 只能经 OpenAI 兼容 chat 访问 Gemini。原生接口（`generateContent` / `streamGenerateContent`）才有完整的思考内容与签名、缓存、工具调用语义。pi 用 `google-generative-ai`，Vertex 用同一种请求体加 GCP 认证（pi 的 `google-vertex`）。

## 待决问题

- 新 crate `agent-provider-gemini` 还是放进现有 crate；它必须过 `agent-provider-contract`，Gemini 没有的概念按 contract README 声明缺失。
- 思考内容与签名怎么进 ADR 0014 的回放（Gemini 要求原样带回签名）。
- thinking 档位映射（`thinkingBudget` / `thinkingLevel`）进 D11 的表。
- 命名 provider 的 `kind` 加 `gemini`；model 列表字段 `supportedGenerationMethods`、new-api 的 `gemini` 端点类型是否进 PL0066 的识别顺序。
- 第二步 Vertex：认证方式（ADC、服务账号文件、`gcloud` token）与 ADR 0026 凭据规则的关系。

## 验收

一个 Gemini 模型经原生接口跑通工具调用与思考回放，contract 套件全绿；ADR 0026 写成新决定后再派发。
