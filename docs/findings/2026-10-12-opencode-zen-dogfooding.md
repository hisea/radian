# 实测记录：对 OpenCode Zen（OpenAI 兼容端点）发第一次真实请求

日期：2026-10-12 · 端点 `https://opencode.ai/zen/go/v1` · 模型 `deepseek-v4.1-flash`
方法：两轮 curl 探测（脚本不入库，输出全程把 API key 替换为 `<REDACTED_KEY>`）。**未使用 `agent-cli`**——原因见缺口 1。

这是 radian 第一次面对一个真实的第三方 OpenAI 兼容端点。下面每条都有探测证据。

## 端点的实际行为（已核实）

| 项 | 结果 |
|---|---|
| 认证 | `Authorization: Bearer <key>` 被接受；`GET /models` 返回真实列表，`deepseek-v4.1-flash` 在其中 |
| `POST /chat/completions`（无 `x-opencode-session`） | **HTTP 400** `{"type":"MissingSessionID","message":"Request is missing x-opencode-session and cannot be routed efficiently"}`；流式与非流式都一样 |
| 加上 `x-opencode-session: <任意稳定字符串>` | **HTTP 200**，正常 SSE |
| 只改 `user-agent`、不给 session 头 | 仍 **400** —— session 头是硬要求，UA 不能替代 |
| `stream_options: {"include_usage": true}` | **HTTP 200**，不报错 |
| usage 形态 | **末尾单独一块**（`choices: []`），字段：`prompt_tokens`、`completion_tokens`、`total_tokens`、`prompt_tokens_details.cached_tokens`、`completion_tokens_details.reasoning_tokens` |
| `tools` 字段 | **支持**。流式 `tool_calls` 带 `index`/`id`/`function.name`，`arguments` **分片到达**（`"{"`、`"\""`…） |
| 响应头 | `x-opencode-endpoint-id`、`x-opencode-upstream-model-id: deepseek/deepseek-v4.1-flash`、`x-zen-model`、`cf-ray`（Cloudflare 前置） |

官方文档（`https://opencode.ai/v2/docs/console/go/`，访问 2026-10-12）的要求：
1. `x-opencode-session` 每个会话一个**稳定** ID，同会话复用（用于路由与 prompt 缓存）；**未规定格式**。
2. 客户端用**自己的** user agent（例如 `my-coding-agent/1.0`），而不是通用 SDK/HTTP 库名。
3. 只面向"编码 agent 类流量"，有滥用监控。

（`https://opencode.ai/docs/go/` 与 `/docs/zen/` 均 404；正确路径是上面那个 `v2/docs/console/go/`。）

## 发现的缺口

### 缺口 1（阻塞）：`agent-cli` 无法发送自定义 header

适配器**支持** `OpenAiCompatibleConfig::headers`（值还会参与脱敏），但 `agent-cli` 没有任何传 header 的参数（`main.rs` 里 "header" 出现 0 次）。因此**这个端点目前完全无法通过 CLI 使用**——不是适配器的问题，是 CLI 的缺口。

另外没有任何地方会生成"会话 ID"。Phase 1 的 `radian-session` 有 `session_id`，但 CLI 不开会话文件时没有它。

### 缺口 2（真实 bug，影响取消前缀）：适配器丢弃 `delta.reasoning_content`

这个模型把**推理过程**流在 `delta.reasoning_content` 里，同时 `delta.content` 一直是 `""`：

```
delta: {"role":"assistant","content":"","reasoning_content":"The"}
delta: {"role":"assistant","content":"","reasoning_content":" user is asking"}
```

`stream.rs` 只读 `delta["content"]`，所以**整个推理阶段 radian 收到零个 `ModelTextDelta`**。后果：

- **ADR 0003 的取消前缀在推理阶段恒为空。** 用户在模型"思考"时取消，明明已经产生了大量文本，我们却什么都不提交——而 ADR 0003 的 F2 正是靠累计 `ModelTextDelta` 实现的。这类"推理模型"是当下的主流，所以这不是边缘情况。
- TUI（Phase 4 计划里的 "thinking/tool cards"）将来要显示推理过程，现在这条数据根本没进来。

**怎么处理需要一个决定**，至少三种：
1. 继续忽略（现状）：取消前缀对推理模型失效。
2. 当普通文本发出：会把推理混进 assistant 的可见文本与 transcript，污染后续请求的历史。
3. 新增一类事件/字段（例如 `AgentEvent::ModelReasoningDelta`，以及 `Message::Assistant` 上一个独立的推理字段）：代价是 Core 要认识"推理"这个概念，并牵动 ADR 0003 与 0006 的 schema。

### 缺口 3：`completion_tokens_details` 被丢掉，推理 token 不进账

`usage_from_json` 的 `details` 只取 `prompt_tokens_details`（或 `details`），所以 `completion_tokens_details.reasoning_tokens: 30` **整个丢失**。推理 token 是计费的，而 ADR 0007 的本意是"provider 报什么就记什么"。本次实测里 `completion_tokens: 40` 中有 **30 是推理 token**，占 75%。

### 缺口 4（小）：user-agent 叫 `agent-runtime`

`model.rs` 发 `agent-runtime/<version>`。符合"自己的名字"这条要求，但叫 `radian/<version>` 更贴切，也便于对端识别。

## 被实测证实做对了的地方

- **usage 的"末尾单独一块"形态**正是 A5 修掉的那个缺口（`stream.rs` 原先在"无 choices 的 chunk"处直接 `Continue`、丢掉 usage）。这个端点就是这种形态，**如果没修，`total_tokens` 会全丢**。
- **`arguments` 分片到达**正是契约第 4 条覆盖的情形，适配器的组装逻辑对得上。
- **`include_usage` 的默认策略**（只对 `api.openai.com` 默认开）在这里表现正确：这个端点默认不会收到 `stream_options`，而实测它其实接受——所以用户需要能显式开启（`Option<bool>` 已支持，但 CLI 同样没有参数）。

## 未验证

- 未经 `agent-cli` 跑通端到端（缺口 1 阻塞）。
- 未验证同一 session ID 复用是否真的带来 prompt 缓存效果。
- 未验证取消行为、idle timeout、错误体形态（4xx/5xx 的 JSON 结构）、并发限制。
- 未验证 `--workspace` 内建工具与该模型的实际配合（它支持 `tools`，但没测多轮工具循环）。
- 未测其他模型（`/models` 列了 `glm-5.3`、`grok-4.7`、`deepseek-v4-pro` 等）。
