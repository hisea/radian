# Anthropic Messages API 线格式依据（`agent-provider-anthropic`）

**访问日期：2026-10-12（UTC 2026-10-12T21:35Z–21:40Z）。** 本文所有格式都来自 Anthropic **官方**开发者文档，逐条给出 URL。**没有**参考任何第三方 SDK 源码、社区整理或抓包。凡官方文档未确认的，写「未验证」，不实现。

**扩展思考（extended thinking）补充访问：2026-10-13（UTC 2026-10-13T02:30Z–02:45Z）**，见 §2.3、§4.2、§6；那一批事实来自 `extended-thinking`、`thinking`、`thinking-tool-workflows` 与 `streaming` 页。

**提示缓存（prompt caching）补充访问：2026-10-15**，见 §2.4、§6；那一批事实只来自官方 `prompt-caching` 页。

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
| `system` | `string` 或 array of `TextBlockParam`（**optional**） | 有 system prompt 时发送；默认用 string 形式，开启提示缓存（`CacheMode::Auto`）且非空时用单块 array 并在该块上带 `cache_control`（§2.4） |
| `tools` | array of `Tool`（optional） | 有工具时发送 |
| `tool_choice` | `ToolChoice`（optional） | 不发送（Core 的 `ModelRequest` 没有该字段；可由 `extra_body`/`settings` 提供） |
| `thinking` | `ThinkingConfig`（`{"type":"enabled","budget_tokens":N}` 或 `{"type":"adaptive"}`） | 有配置才发送；默认关闭（不发送该键）。见 §2.3 |
| 其它（`temperature`、`top_p`、`top_k` 等） | 各自 optional | 经 `extra_body`/`settings` 透传 |

### 2.3 `thinking` 参数（新增，访问 2026-10-13）

来源：`https://platform.claude.com/docs/en/build-with-claude/extended-thinking.md`（规范 URL `.../extended-thinking`）与 `https://platform.claude.com/docs/en/build-with-claude/thinking.md`（规范 URL `.../thinking`）。访问 2026-10-13。

- **两种开启方式**：(1) 手动 `thinking: {"type":"enabled","budget_tokens": N}`（本文写为 `ThinkingConfig::Enabled`）；(2) 自适应 `thinking: {"type":"adaptive"}`（`ThinkingConfig::Adaptive`）。手册明写：`type: "enabled"` 在 Claude 4.6 上 **deprecated**（请求仍成功），Claude 4.7 及以后 **拒绝** `type: "enabled"` 并返回 400；4.6+ 推荐 `adaptive`。两种模式都能开；本 adapter 两种都支持，由 host 选择。
- **`budget_tokens` 约束**（仅 `Enabled`）：最小 **1024**（API 拒绝更小值）；必须 **小于 `max_tokens`**（思考 token 计入 `max_tokens`），唯一例外是 interleaved thinking（预算跨整轮，可超过 `max_tokens`）。本 adapter **不在本地校验**这两个约束（服务器是权威），配置错误会得到与 `extra_body` 相同的 400。
- **`temperature` / 采样参数**：在老模型上、**开了 thinking 时**，`temperature` 与 `top_k` **不兼容**（`top_p` 只在 0.95–1 之间允许）；在新模型（Claude Opus 5.5/5、Sonnet 5.5/5、Haiku 5.5、Fable/Mythos 等）上，非默认采样参数在**每个请求**上都返回 400（不论是否 thinking）。本 adapter **不**替 host 增删这些参数：它照常透传 `settings`/`extra_body` 里的值；组合是否合法由服务器判定，错误原样冒泡（与 ADR 0015 对 provider 报错的立场一致）。
- **`tool_choice`**：手动 `type:"enabled"` 只支持 `auto`（默认）与 `none`；`any` / 指定 `tool` 会报错（强制工具调用与手动思考不兼容）。`adaptive` 支持强制工具调用，但 Claude Opus 5.5 / Sonnet 5.5 / Fable 5.1 / Mythos 5.1 在每个请求上都拒绝强制工具调用。本 adapter 本就不发 `tool_choice`（wire notes §2 旧行），由 host 经 `extra_body` 提供，所以这里只是记录限制，不放宽。
- **提示缓存**：改 thinking 配置（模式、`budget_tokens`）会使缓存断点失效。
- **文档未说明**：本 adapter 不解析、不依赖 `display`（`summarized`/`omitted`/`updates`）——它是 `thinking` 对象里的另一个可选字段；只想开 thinking 时手册示例可以只给 `type`，所以本 adapter 只发 `type`（`Enabled` 时另发 `budget_tokens`），不发 `display`。若要 `display` 或 `block_binding` 等新字段，经 `extra_body` 传入。**未验证**：这些附加字段在 `thinking` 被 adapter 顶层插入后能否再经 `extra_body` 合并（两个来源都是 `thinking` 键，后者整体覆盖前者，文档未说明应否合并）。

`max_tokens` 是否为必需：Body parameters 一节把它列为 `- max_tokens: number`（未标 `optional`），而同一节里 `system`/`tools`/`tool_choice` 都标了 `optional`；且**每一个**官方示例（streaming 页、api/messages Example、errors 页）都带 `max_tokens`。本 adapter 因此**总是**发送 `max_tokens`（配置默认 4096，可改）。**未验证**：官方未用「required」字样明写，此处依据的是「未标 optional + 所有示例都带」这一文档证据。

### 2.4 提示缓存（`cache_control`）（新增，访问 2026-10-15）

来源：`https://platform.claude.com/docs/en/build-with-claude/prompt-caching.md`（规范 URL `https://platform.claude.com/docs/en/build-with-claude/prompt-caching`）。访问 2026-10-15。提示缓存的全部事实只取这一页官方文档；下面每条都能在该页找到原文。

- **两种方式**：(1) **自动缓存（automatic caching）**：在请求体**顶层**放一个 `cache_control` 字段（`{"type":"ephemeral"}`），系统自动把断点放在最后一个可缓存块上，并随对话增长前移；(2) **显式断点（explicit cache breakpoints）**：把 `cache_control` 直接放在**单个内容块**上。两者可以混用，但**自动缓存也占 4 个断点槽位之一**。本 adapter 用**显式断点**（顶层自动缓存会引入一个新的顶层键，且本 adapter 要在固定三处放断点）。
- **断点上限**：一次请求**最多 4 个** `cache_control` 断点。官方原文：「You can define up to 4 cache breakpoints」；「If 4 explicit block-level breakpoints already exist, the API returns a 400 error (no slots left for automatic caching)」。本 adapter 最多放 **3** 个（见下），不碰第 4 个。
- **缓存前缀顺序**：`tools` → `system` → `messages`。某一级的改动使该级及其后**所有**级失效（官方 §What invalidates the cache）。
- **可缓存的内容**：`tools` 数组里的工具定义；`system` 数组里的内容块；`messages.content` 里的文本、图片、`tool_use`、`tool_result` 块。
- **不可直接缓存**：`thinking`/`redacted_thinking` 块**不能**直接带 `cache_control`（它们可以在后续轮次随其它内容一起被缓存，且被读到时计入输入 token）；**空文本块**不能缓存（`text` 有 `minLength 1`）；引用等**子内容块**不能，要缓存其顶层块。
- **`cache_control` 形状**：`{"type":"ephemeral"}`。目前 `ephemeral` 是**唯一**支持的 `type`，默认 5 分钟 TTL；`{"type":"ephemeral","ttl":"1h"}` 是 1 小时（2 倍价）。本 adapter 只发 `{"type":"ephemeral"}`，不发 `ttl`。
- **`system` 的形状**：官方类型是 `string` **或** array of `TextBlockParam`。要把断点放在 system 上，就必须用 array 形式（断点在某个块上）。
- **最小可缓存长度**：按模型不同为 512 / 1024 / 2048 / 4096 token；**短于**该长度时请求照常处理，但**不缓存**且**不报错**（此时 `cache_creation_input_tokens` 与 `cache_read_input_tokens` 都为 0）。官方还给了一条「缓存写入只在断点处发生、读取向前回看最多 20 个块」的规则，本 adapter 一次请求只关心当前请求，不受它影响。
- **usage 口径**：`input_tokens` 是**最后一个断点之后**、既未读也未写缓存的 token 数；官方原文：`total_input_tokens = cache_read_input_tokens + cache_creation_input_tokens + input_tokens`。这再次印证 Anthropic 的 `input` **不含**缓存命中（ADR 0007 的前提），与 §3.1 的字段映射一致。
- **本 adapter 的断点**（`CacheMode::Auto`，**默认**）：(1) **最后一件**工具定义；(2) system 末尾（system 非空时把它发成带 `cache_control` 的单块 array）；(3) **最后一条消息**里**最后一个可缓存块**（跳过 `thinking`/`redacted_thinking` 与空文本）。对应部分为空时该断点不存在。`CacheMode::None` 完全不放 `cache_control`，请求体与实现本特性前**逐字节相同**。
- **未验证**：本 adapter 只发显式断点、不发顶层自动缓存；也**没有**在真实端点上观察过缓存命中（本仓库不持有 Anthropic key，见 §6 的手测项）。

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
- 若 `display: "omitted"`：思考块仍出现，`thinking` 字段为空，只有 `signature`；`thinking_delta` 可能带空串（见 §2.3、§6）。

### 4.5 思考块的回传要求（新增，访问 2026-10-13）

来源：`https://platform.claude.com/docs/en/build-with-claude/thinking.md`（Preserving thinking blocks / Redacted thinking blocks / Thinking encryption）与 `https://platform.claude.com/docs/en/build-with-claude/thinking-tool-workflows.md`（Echo the assistant message exactly as received）。访问 2026-10-13。

- **工具轮必须回传**：调用工具时，assistant 消息里的 `thinking` 块必须**完整、未修改**地回传，并与它所伴随的 `tool_use` 一起。官方原文：「Pass every `thinking` block back to the API complete and unmodified, alongside the `tool_use` block it accompanied.」
- **顺序**：「Within the latest assistant message, the sequence of consecutive `thinking` blocks must match what the model generated in the original request: you can't rearrange, edit, or partially drop them.」修改过的思考块会被 400 拒绝。
- **必须包含 `redacted_thinking`**：它形状为 `{"type":"redacted_thinking","data":"..."}`，`data` 不透明；官方明确「pass `redacted_thinking` blocks back ... unchanged」，并警告只过滤 `block.type == "thinking"` 会静默丢掉它们。
- **`signature` 不透明**：「don't interpret or parse it」；回传时值不得改。它是块级加密思考，服务器用它重建思考。
- **更早轮次可省**：「Required: within a tool-use turn, pass thinking blocks back. Recommended: across turns, pass everything back. Allowed: outside tool use, omit prior turns' thinking.」即**同一个 assistant 轮**内的思考块必须回传；更早轮的可以省略。本 adapter 选择**不省略**：凡是收到过的思考块都保留在 `provider_payload` 并回传（更简单，且官方说服务端会自行过滤。）
- **本 adapter 的回传方式**：`ModelResponse.provider_payload` 存一个 JSON **数组**，元素是流里按序出现的 `thinking`/`redacted_thinking` 块（`thinking` 块的 `thinking`/`signature` 由 delta 拼出，其它字段来自 `content_block_start`；`redacted_thinking` 块整个 `content_block_start` 对象原样保留）。`request.rs` 构造 assistant 消息时把这些块放在 `content` 文本块与 `tool_use` 块**之前**（官方 `[thinking, text, tool_use]` 形状）。`OpaqueJson` 保字节，JSONL 往返后签名值不变；重建请求时会经 `serde_json::Value` 序列化，键序可能规整（与 ADR 0007 J3 同类取舍），但**每个块的字段值与文本不变**。

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
- **已实现（2026-10-13）**：扩展思考。请求侧由 `AnthropicConfig::thinking`（`ThinkingConfig::Enabled{budget_tokens}` / `Adaptive`）控制，默认关闭；响应侧 `thinking`/`redacted_thinking` 块存入 `ModelResponse.provider_payload`，下次请求原样前置回传（ADR 0014 R10）。**不**接进 `reasoning`，也不发 UI 事件（验收 8）。未实现的部分：不解析 `display`/`block_binding` 等 `thinking` 子字段（可经 `extra_body` 传，但本 adapter 不看）；不做 `budget_tokens`/`temperature`/`tool_choice` 的本地合法性校验（服务器权威，见 §2.3）。
- **未验证**：`redacted_thinking` 块是否可能带 delta（官方示例只在 `content_block_start` 给 `data`；本 adapter 对其 delta 视同无）。
- **未验证**：`thinking` 块在流里能否穿插在文本/工具块之间（官方示例总是思考在前；本 adapter 按块下标保序，不论位置，回传时统一前置。**若**官方允许思考块出现在 `tool_use` 之后且要求同样保持相对位置，本 adapter 的前置策略会不满足——官方文档未说明这种顺序，记此风险）。
- **未实现**：服务器工具块（`server_tool_use`、`web_search_tool_result`、`web_fetch_*`、`code_execution_*` 等）——一律忽略，不作为 `ToolCall`（Core 规定「provider 自己执行的工具绝不表示为 `ToolCall`」）。
- **已实现（2026-10-15）**：提示缓存（请求侧）。`AnthropicConfig::cache`（`CacheMode::Auto` 默认 / `None`）控制。Auto 在**三处**放显式断点（最后一件工具、system 末尾、最后一条消息的最后一个可缓存块），最多 3 个；None 不放，请求体与实现前逐字节相同（§2.4）。**未实现的部分**：顶层自动缓存字段、`ttl`、按最小长度或对话增长动态调整断点（官方自己会在短于阈值时静默不缓存，本 adapter 不做本地长度判断）。
- **未实现**：`stop_details`、`container`、`citations`、`tool_choice`（请求侧）、`anthropic-beta` 头、`metadata`。
- **未验证**：官方 API 的真实网络行为（本 adapter 只在进程内 mock 上测试，未访问真实 API，未使用任何真实 key）。
- **未验证**：`Message Delta Usage` 的 `input_tokens` 在实际流里是否总是出现（官方类型允许 `null`）；本 adapter 的逐字段合并对两种情形都成立。

## 7. 官方文档页面清单（访问 2026-10-12）

- `https://platform.claude.com/docs/en/build-with-claude/prompt-caching`（显式断点、自动缓存、4 断点上限、缓存顺序、可缓存/不可缓存块、最小长度、usage 口径；访问 2026-10-15）
- `https://platform.claude.com/docs/en/build-with-claude/thinking`（思考概念、`display`、流式、工具轮回传、加密、`redacted_thinking`；访问 2026-10-13）
- `https://platform.claude.com/docs/en/build-with-claude/extended-thinking`（`thinking: {type:"enabled",budget_tokens}`、约束、adaptive 迁移；访问 2026-10-13）
- `https://platform.claude.com/docs/en/build-with-claude/thinking-tool-workflows`（两轮工具往返的逐字示例；访问 2026-10-13）
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

## 9. 限额响应头（ADR 0027 切片 ④，2026-10-17）

来源：`https://docs.claude.com/en/api/rate-limits`（Anthropic 官方 rate-limits 页：请求数与
token 数的 limit / remaining / reset 六个头）。本离线 worktree 未重拉该页；头名按切片 ④
任务单的方向取，adapter 只按前缀 `anthropic-ratelimit-` 原样保留（`model.rs` 的
`RATE_LIMIT_PREFIX`），具体名字只出现在宿主的解析表（`radian-host` 的 `ANTHROPIC_HEADERS`），
改名不需要动 adapter。

| 头 | 含义 | 宿主怎么用 |
|---|---|---|
| `anthropic-ratelimit-requests-limit` | 请求数上限 | 保留在原始头里，不进快照 |
| `anthropic-ratelimit-requests-remaining` | 请求剩余数 | `requests` 窗口的 `remaining`（计数） |
| `anthropic-ratelimit-requests-reset` | 请求窗口重置（RFC 3339 UTC 时间戳） | `requests` 窗口的倒计时 |
| `anthropic-ratelimit-tokens-limit` | token 上限 | 保留在原始头里，不进快照 |
| `anthropic-ratelimit-tokens-remaining` | token 剩余数 | `tokens` 窗口的 `remaining`（计数） |
| `anthropic-ratelimit-tokens-reset` | token 窗口重置（RFC 3339 UTC 时间戳） | `tokens` 窗口的倒计时 |

规则：只解析上表六个名字（精确小写匹配；文档未命名的一律忽略）；值保持原文，计数解析不出
是整数则该窗口只剩倒计时，倒计时解析不出则只剩计数（ADR 0027：一个坏字段只丢自己）。
429 的 `retry-after` 仍走旧的 `Retry-After` 解析（`Unavailable.retry_after`），与这六个头无关。
