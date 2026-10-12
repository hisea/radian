# Anthropic Messages API 线格式依据（`agent-provider-anthropic`）

**访问日期：2026-10-12（UTC 2026-10-12T21:35Z–21:40Z）。** 本文所有格式都来自 Anthropic **官方**开发者文档，逐条给出 URL。**没有**参考任何第三方 SDK 源码、社区整理或抓包。凡官方文档未确认的，写「未验证」，不实现。

官方文档提供 markdown 版本：把文档页 URL 加上 `.md` 即可（例如
`https://platform.claude.com/docs/en/build-with-claude/streaming.md`）。下列访问用的是该形式；括号里是页面 frontmatter 声明的规范 URL。

## 1. 端点与请求头

来源：`https://platform.claude.com/docs/en/build-with-claude/streaming.md`
（规范 URL `https://platform.claude.com/docs/en/build-with-claude/streaming`）；`https://platform.claude.com/docs/en/api/messages.md`（规范 URL `.../api/messages`）；`https://platform.claude.com/docs/en/api/versioning.md`（规范 URL `.../api/versioning`）。访问 2026-10-12。

| 项 | 值 | 出处 |
|---|---|---|
| 端点 | `POST https://api.anthropic.com/v1/messages` | streaming 页 cURL 示例；api/messages 的 Example（`curl https://api.anthropic.com/v1/messages`） |
| 认证头 | `x-api-key: $ANTHROPIC_API_KEY` | streaming 页 cURL 示例；api/messages Example（`X-Api-Key`） |
| 版本头 | `anthropic-version: 2023-06-01` | api/versioning 页开头：「you must send an `anthropic-version` request header. For example, `anthropic-version: 2023-06-01`」；版本历史里只有 `2023-06-01`（当前）与 `2023-01-01` |
| 内容类型 | `content-type: application/json` | streaming 页 cURL 示例 |
| 流式开关 | 请求体 `"stream": true` | streaming 页开头：「you can set `"stream": true`」 |

`anthropic-version` 的其它取值：api/versioning 列出 `2023-01-01`（初始）与 `2023-06-01`（当前，SSE 增量事件格式）。本 adapter 默认 `2023-06-01`，可由配置覆盖。

**未验证**：`anthropic-beta` 头（api/messages 的 Headers 一节没有列出它；个别 beta 功能页提到 beta 头，但本 adapter 不实现任何 beta 功能，故不使用）。api/messages 的 Headers 一节只列出 `anthropic-version`、`anthropic-user-profile-id`、`anthropic-workspace-id`；后两者本 adapter 不使用（也未实现）。

## 2. 请求体

来源：`https://platform.claude.com/docs/en/api/messages.md`（Body parameters，行 ~36 起）与 streaming 页示例。访问 2026-10-12。

| 字段 | 类型 | 本 adapter 是否发送 |
|---|---|---|
| `model` | string | 是（配置） |
| `max_tokens` | number（minimum 0） | 是（配置；必需字段，见下） |
| `messages` | array of `MessageParam` | 是 |
| `stream` | boolean | 是（恒为 `true`） |
| `system` | `string` 或 array of `TextBlockParam`（**optional**） | 有 system prompt 时发送（用 string 形式） |
| `tools` | array of `Tool`（optional） | 有工具时发送 |
| `tool_choice` | `ToolChoice`（optional） | 不发送（Core 的 `ModelRequest` 没有该字段；可由 `extra_body`/`settings` 提供） |
| 其它（`temperature`、`top_p`、`top_k`、`thinking` 等） | 各自 optional | 经 `extra_body`/`settings` 透传 |

`max_tokens` 是否为必需：Body parameters 一节把它列为 `- max_tokens: number`（未标 `optional`），而同一节里 `system`/`tools`/`tool_choice` 都标了 `optional`；且**每一个**官方示例（streaming 页、api/messages Example、errors 页）都带 `max_tokens`。本 adapter 因此**总是**发送 `max_tokens`（配置默认 4096，可改）。**未验证**：官方未用「required」字样明写，此处依据的是「未标 optional + 所有示例都带」这一文档证据。

### 2.1 `MessageParam` 与内容块

来源：api/messages 的 `Message Param` / `Text Block Param` / `Image Block Param` / `Tool Use Block Param` / `Tool Result Block Param`。访问 2026-10-12。

- 消息：`{"role": "user"|"assistant", "content": <string 或 array of ContentBlockParam>}`。role 只有 `user`/`assistant`；**没有** `system` role（system 是顶层参数）。
- 文本块：`{"type":"text","text":<string>}`（`text` minLength 1）。
- 图片块：`{"type":"image","source":{"type":"base64","media_type":"image/jpeg"|"image/png"|"image/gif"|"image/webp","data":<base64>}}`。
- assistant 的工具调用块：`{"type":"tool_use","id":<string>,"name":<string>,"input":<map>}`。
- 工具结果块：`{"type":"tool_result","tool_use_id":<string>,"content":<string 或 array>,"is_error":<optional bool>}`，放在**一条 `user` 消息**里。
- 官方往返示例（tool-use 页行 ~199）：
  `messages: [ {role:"user",...}, {role:"assistant", content:<response.content>}, {role:"user", content:[{type:"tool_result", tool_use_id:..., content:...}]} ]`。
- 连续同 role 的消息会被 API 合并：「Consecutive `user` or `assistant` turns in your request will be combined into a single turn.」（api/messages `messages` 说明）。本 adapter 因此把连续的 `Message::Tool` 合并进同一条 user 消息的 `tool_result` 块数组。

工具定义：`{"name":<string>,"description":<optional string>,"input_schema":<object>}`（`Tool` 域类型）。`name` 有 pattern `^[a-zA-Z0-9_-]{1,128}$`——注意 Core 的工具名可能含 `.`/`:` 等（MCP 别名），**未验证**这类名字在 Anthropic 是否会被拒；本 adapter 不重写工具名（由 host 的 tool 命名策略负责）。

### 2.2 音频/资源等 Core 内容块

Anthropic 的 `ContentBlockParam` 文档列出的输入块类型里**没有**音频块；也没有 MCP 的 `ResourceLink`/`EmbeddedResource`。本 adapter 对 Core 的 `ContentPart::Audio`/`ResourceLink`/`EmbeddedResource`/`Opaque` 一律转成**文本占位符**（与 OpenAI adapter 同样的有损策略），不伪造 Anthropic 不存在的块类型。

## 3. 响应（非流式，仅用于对照）

来源：api/messages `Message` / `Usage` 域类型。访问 2026-10-12。

`Message`：`id`、`type:"message"`、`role:"assistant"`、`content: array of ContentBlock`、`model`、`stop_reason`、`stop_sequence`、`usage`。

### 3.1 Anthropic 官方 usage 的完整字段集合（`unreported_usage_fields` 的依据）

来源：`https://platform.claude.com/docs/en/api/messages.md` 的 `Usage` 域类型（行 ~33732）与 `Message Delta Usage` 域类型（行 ~22678）。访问 2026-10-12。

| 字段 | 类型 | 在 `Usage` | 在 `MessageDeltaUsage` | 本 adapter 映射到 `ModelUsage` |
|---|---|---|---|---|
| `input_tokens` | number（Delta 里为 `number or null`，累计） | 有 | 有 | `input` |
| `output_tokens` | number（Delta 里为 `number`，累计） | 有 | 有 | `output` |
| `cache_read_input_tokens` | `number or null` | 有 | 有 | `cache_read` |
| `cache_creation_input_tokens` | `number or null` | 有 | 有 | `cache_write` |
| `cache_creation` | `CacheCreation or null`（`ephemeral_1h_input_tokens`、`ephemeral_5m_input_tokens`） | 有 | 无 | 不映射（见下） |
| `output_tokens_details` | `OutputTokensDetails or null`（`thinking_tokens`） | 有 | 有 | `details`（原样保留） |
| `server_tool_use` | `ServerToolUsage or null`（`web_fetch_requests`、`web_search_requests`） | 有 | 有 | 不映射 |
| `service_tier` | `"standard" or "priority" or "batch" or null` | 有 | 无 | 不映射 |
| `inference_geo` | `string or null` | 有 | 无 | 不映射 |

**没有 `total_tokens`**（对整份 api/messages 全文 grep `total_tokens` 无命中；`Usage` 与 `Message Delta Usage` 均无）。因此 `agent-provider-anthropic` 的 `ContractHarness::unreported_usage_fields()` 声明 `UsageField::Total`，`ModelUsage.total` 恒为 `None`（ADR 0007 J2：绝不计算）。不映射的 `cache_creation`/`server_tool_use`/`service_tier`/`inference_geo` 是本 adapter 的**已知有损**：它们不是 `ModelUsage` 的中性字段（`cache_creation` 的 TTL 明细只是 `cache_write` 的和的分解，`cache_write` 已承载总和）。

## 4. 流式事件（SSE）

来源：`https://platform.claude.com/docs/en/build-with-claude/streaming.md`（Event types / Content block delta types / Full HTTP stream response）。访问 2026-10-12。

事件序列（官方原文）：
1. `message_start`：带一个 `Message` 对象，`content` 为空；usage 的 `input_tokens`/`output_tokens` 在此。
2. 若干内容块：每块一个 `content_block_start`，若干 `content_block_delta`，一个 `content_block_stop`。块的 `index` 对应最终 `content` 数组下标。
3. 一个或多个 `message_delta`：顶层变化（含 `delta.stop_reason`、`delta.stop_sequence`）与 usage。**usage 计数是累积的**（官方 Warning）。
4. `message_stop`。

另外：`ping` 事件可出现任意次；流内错误是 `event: error` + `{"type":"error","error":{"type":...,"message":...}}`（例：`overloaded_error`）。官方明确「new event types may be added, and your code should handle unknown event types gracefully」。

每个事件都有 SSE 事件名（`event: message_stop`），且 `data` 里的 `type` 与事件名一致——本 adapter 以 `data.type` 为准（`event:` 行被忽略）。

### 4.1 `content_block_start`

官方示例形状：
- 文本：`{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}`
- 工具：`{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_...","name":"get_weather","input":{}}}`
- 思考：`{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}`
- 服务器工具：`{"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_...","name":"web_search","input":{}}}`

### 4.2 `content_block_delta`

- 文本：`{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ello frien"}}`
- 工具参数（**分片 JSON 字符串**）：`{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"location\": \"San Fra"}}`；官方说明最终 `tool_use.input` 是对象，delta 是 partial JSON 字符串。
- 思考：`{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"..."}}`
- 思考签名：`{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQB..."}}`（在 `content_block_stop` 前发出，用于校验思考块完整性）

### 4.3 `message_delta`

`{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":15}}`。官方在 handling-stop-reasons 页明确：流式下 `stop_reason` 在 `message_start` 里是 `null`，只在 `message_delta` 里给出，「Not provided in any other events」。

### 4.4 `stop_reason` 官方取值集合

来源：`https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons.md`（Quick reference 表）与 api/messages 的 `Stop Reason` 域类型。访问 2026-10-12。

官方共 **7** 个值：`end_turn`、`max_tokens`、`stop_sequence`、`tool_use`、`pause_turn`、`refusal`、`model_context_window_exceeded`。

**因此 Anthropic 有 `pause_turn`**（服务器工具循环达到迭代上限时返回），本 adapter **正常实现**它，不使用「不支持」出口。

`refusal` 的补充字段：官方说读 `stop_details`（handling-stop-reasons 的 refusal 一节与响应示例里的 `stop_details`）。本 adapter 不解析 `stop_details`（Core 没有对应字段；拒绝文本来自 content 文本块）。

## 5. 本 adapter 的映射决定（不是官方线格式，是设计选择）

| Anthropic `stop_reason` | Core `StopReason` | 理由 |
|---|---|---|
| `end_turn` | `EndTurn` | 直接对应 |
| `tool_use` | `ToolUse` | 直接对应 |
| `max_tokens` | `MaxTokens` | 直接对应 |
| `pause_turn` | `PauseTurn` | 官方存在，直接对应；Core 对 `PauseTurn` 是继续本轮 |
| `refusal` | `Refusal` | 直接对应 |
| `stop_sequence` | `EndTurn` | Core 无 `StopSequence` 变体；该响应是**正常完成**（触发了 stop sequence），映射成「自然结束」在 turn 语义上正确，代价是丢失「哪个 sequence」这一信息（Core 也不承载 `stop_sequence`）。**这是有损的中性化，不是官方格式** |
| `model_context_window_exceeded` | `MaxTokens` | 官方原文「Treat the response as truncated.」；Core 对 `MaxTokens` 的处理正是「截断、且不执行可能被截断的工具调用」（`turn.rs` 的 `dispatch_tools`），映射到 `MaxTokens` 可避免执行截断的调用 |
| 其它任意值 | `Other(<原文>)` | 官方明确取值会增长；未知值必须原样呈现，不能静默落到某个已知值 |

`usage` 映射：`input`←`input_tokens`，`output`←`output_tokens`，`cache_read`←`cache_read_input_tokens`，`cache_write`←`cache_creation_input_tokens`，`details`←`output_tokens_details`（原样保留的嵌套对象），`total`←**无**（Anthropic 不提供）。

流式 usage 合并：`message_start.message.usage` 与每个 `message_delta.usage` **逐字段**取「最后一次出现的非 null 值」（官方说 message_delta 的计数是累积的，所以后出现的值更完整；`message_start` 提供 input/cache，`message_delta` 提供 output）。`details` 取最后一次带 `output_tokens_details` 的对象，原样保留。

## 6. 未验证 / 未实现（不假装支持）

- **未验证**：`max_tokens` 是否被官方标为 required（见 §2 的证据与说明）。
- **未验证**：工具名含 `.`/`:` 等非 `^[a-zA-Z0-9_-]{1,128}$` 字符时 Anthropic 是否拒绝（未实测，官方只给了 pattern）。
- **未实现**：扩展思考（extended thinking）。请求侧不主动发送 `thinking`（可由 `extra_body` 传入，但本 adapter 不为其做映射）；响应侧 `thinking`/`signature` 块与 `thinking_delta`/`signature_delta` 一律**忽略**，不作为 `ModelTextDelta` 发出（否则会污染 Core 的取消前缀，ADR 0003 F2），也不写入 `provider_payload`。后果：开了 thinking 且用工具时，官方要求把思考块**原样**回传（api/messages 的 thinking 块说明），本 adapter 做不到——**该组合未实现**。
- **未实现**：服务器工具块（`server_tool_use`、`web_search_tool_result`、`web_fetch_*`、`code_execution_*` 等）——一律忽略，不作为 `ToolCall`（Core 规定「provider 自己执行的工具绝不表示为 `ToolCall`」）。
- **未实现**：`stop_details`、`container`、`citations`、`tool_choice`（请求侧）、`anthropic-beta` 头、prompt caching 的 `cache_control`、`metadata`。
- **未验证**：官方 API 的真实网络行为（本 adapter 只在进程内 mock 上测试，未访问真实 API，未使用任何真实 key）。
- **未验证**：`Message Delta Usage` 的 `input_tokens` 在实际流里是否总是出现（官方类型允许 `null`）；本 adapter 的逐字段合并对两种情形都成立。

## 7. 官方文档页面清单（访问 2026-10-12）

- `https://platform.claude.com/docs/en/build-with-claude/streaming`（事件序列、delta 类型、错误事件）
- `https://platform.claude.com/docs/en/api/messages`（端点、头、请求体、域类型：`Message`/`Usage`/`ContentBlock*`/`StopReason`/`Tool*`）
- `https://platform.claude.com/docs/en/api/versioning`（`anthropic-version`）
- `https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons`（`stop_reason` 7 值、流式下出现位置）
- `https://platform.claude.com/docs/en/api/errors`（HTTP 错误码与错误体形状）
- `https://platform.claude.com/docs/en/build-with-claude/tool-use`（工具往返示例）
- `https://platform.claude.com/docs/en/build-with-claude/thinking`（思考流式与签名，仅用于说明本 adapter 未实现它）

## 8. 与 provider 契约套件的关系：两处出口

`agent-provider-contract` 的契约按「语义（`Wire`）」描述，但两处把 OpenAI 的形状当成了所有 provider 的形状。两处都用**显式声明 + 断言缺席/跳过**解决，**没有**静默跳过，也**没有**为了让契约通过而伪造 provider 行为：

1. **finish reason**：`check_finish_reasons` 原先要求每个 adapter 对每个 `Reason` 都有线上形式。Anthropic 的官方 `stop_reason` 集合（§4.4）里**没有** `content_filter`（它用 `refusal`，由契约的另一条子检查覆盖），所以 Anthropic harness 用 `unsupported_reasons()` 显式声明 `Reason::ContentFiltered`，该条被**跳过**（它在本 provider 下无意义）。
2. **usage 字段**：`check_usage_shapes` 原先脚本一个含 `total: Some(15)` 的 `ModelUsage` 并要求逐字段相等。Anthropic 官方 usage **没有** total 字段（§3.1），所以 Anthropic harness 用 `unreported_usage_fields()` 显式声明 `UsageField::Total`；契约据此把 `total` **断言为 `None`**（而不是跳过）：provider 没报的数字，adapter 永不得编（ADR 0007 J2）。这处出口让检查**更严**。
