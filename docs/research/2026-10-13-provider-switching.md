# 调研：ADR 0026 / 0027 实施前的三项未核实

日期：2026-10-13 · 分支 `pi/research-0026` · 状态：结论文档，不改产品代码。

本片回答 ADR 0026 D6、ADR 0026 D0 / ADR 0027 C2、ADR 0027 C4 里标着**未核实**的三项。每条结论附出处
（`文件:行号`，或 URL + 访问日期 + 原文短引用）。查不到的写「未核实」，不猜。

---

## 1. 跨 provider 切换时 transcript 的兼容性（ADR 0026 D6）

情形：同一会话里前几轮由 provider A 生成，下一轮改用 provider B 发送整段历史。下面逐项核对两家
适配器**构造请求**时的行为（`Model::complete` 之前，纯函数）。

### 1.1 两家构造请求时怎么处理 `reasoning`、`provider_payload`、工具调用 id

**OpenAI-compatible**（`crates/agent-provider-openai-compatible/src/request.rs`）：

- `message()` 对 assistant 的解构是 `Message::Assistant { content, tool_calls, .. }`
  （`request.rs:74-78`）：`reasoning` 与 `provider_payload` 都落在 `..` 里，**根本不读**。
- 因此 `assistant()`（`request.rs:111-135`）只发 `role`、`content`（`plain_text(content)`）、
  `tool_calls`；每个调用发 `{"id": call.id, "type": "function", "function": {...}}`
  （`request.rs:127`）。工具结果的 `tool_call_id` 取 `result.call_id` 原文（`request.rs:152`）。
- 有一个针对 `reasoning` 的回归测试 `reasoning_is_not_sent_to_the_provider`
  （`request.rs:241-265`）：带 `reasoning` 与不带的历史，请求体逐字节相同，且正文不含 `reasoning`。
  这与 ADR 0014 R4 的「provider 不可见」一致。
- `ContentPart::Opaque`（不是 `provider_payload`，是 `content` 里的一个 part）会变成
  `"[unsupported content omitted]"` 占位文本（`request.rs:177`）。

**Anthropic**（`crates/agent-provider-anthropic/src/request.rs`）：

- `messages()` 对 assistant 的解构是
  `Message::Assistant { content, tool_calls, provider_payload, .. }`（`request.rs:100-112`）：
  `reasoning` 落在 `..` 里，**不读**；`provider_payload` **会读**并交给 `assistant_message`。
- `assistant_message`（`request.rs:138-153`）把 `provider_payload` 里的
  `thinking` / `redacted_thinking` 块（`thinking_blocks`，`request.rs:159-180`）放在 `content` 文本块
  与 `tool_use` 块**之前**回传。`thinking_blocks` 要求 `provider_payload` 是 JSON **数组**：不是数组
  就返回 `ModelError::Protocol("provider_payload must be a JSON array of content blocks")`
  （`request.rs:167-169`），**不是**静默丢弃。
- `reasoning` 同样有回归测试 `reasoning_is_not_sent_to_the_provider`（`request.rs:334-356`）。
- 工具调用 id 原文发给 `tool_use.id`（`tool_use_block`，`request.rs:207-214`）；工具结果的 id 原文
  发给 `tool_result.tool_use_id`（`tool_result_block`，`request.rs:228-241`）。**不做前缀校验**。
- `ContentPart::Opaque` 同样变占位文本（`request.rs:266`）。

### 1.2 另一家的内容发过来会怎样：原样发出、丢弃，还是报错？

逐组合（现状，均有代码位置）：

| 内容 / 来源 → 目标 | OpenAI-compatible 构造请求 | Anthropic 构造请求 |
|---|---|---|
| `reasoning`（来自 deepseek 等） | **丢弃**（`..`，`request.rs:74`）；有逐字节回归测试 | **丢弃**（`..`，`request.rs:100`）；有逐字节回归测试 |
| `provider_payload` 为 `None`（OpenAI adapter 只写 `None`，`stream.rs:225`） | 无影响 | `thinking_blocks(None)` → 空（`request.rs:160-162`），无影响 |
| `provider_payload` 为 JSON **数组**（Anthropic 的 thinking 块） | **丢弃**（`..`，`request.rs:74`） | 回传 `thinking`/`redacted_thinking` 块（`request.rs:147`） |
| `provider_payload` 为**非数组** JSON（未来某 provider） | 丢弃 | **报错** `ModelError::Protocol`（`request.rs:167-169`） |
| `content` 里的 `ContentPart::Opaque` | 变占位文本 | 变占位文本 |

结论（现状）：

- **Anthropic → OpenAI-compatible**：assistant 消息带 `provider_payload`（thinking 块数组）、
  `reasoning` 为空。OpenAI 构造器忽略两者，请求体只含 `content` + `tool_calls`。**不报错，静默丢弃**
  对方特有的 thinking 内容。
- **OpenAI-compatible → Anthropic**：assistant 消息带 `reasoning`（`reasoning_content` 累积）、
  `provider_payload = None`。Anthropic 构造器忽略 `reasoning`、`thinking_blocks(None)` 为空。
  **不报错，静默丢弃**对方的推理内容。
- 唯一会**报错**的组合：历史里有一条 assistant 消息的 `provider_payload` 是**非数组** JSON，且下一轮
  发给 Anthropic。今天两个内建 adapter 都不会产生这种 payload（OpenAI 恒 `None`；Anthropic 恒数组），
  所以今天不可达；未来加第三家 provider 时要防。

### 1.3 工具调用 id 的格式约束

- **Anthropic**：官方 Messages API 文档对 `ToolUseBlockParam.id` 的约束是
  `pattern: ^[a-zA-Z0-9_-]+$`（`https://platform.claude.com/docs/en/api/messages.md`，访问
  2026-10-13，原文 `pattern: ^[a-zA-Z0-9_-]+$`）；`ToolResultBlockParam.tool_use_id` 同一 pattern。
  **没有** `toolu_` 前缀要求——文档示例用 `"toolu_01D7FLrfh4GYq7yT1ULFeyMV"`，但 schema 只约束字符集。
  `srvtoolu_` 前缀只用于 server-tool 的 `ServerToolCaller.tool_id`（文档同页），与普通 `tool_use.id`
  无关。
- **OpenAI-compatible**：适配器不校验也不改写 id，把 provider 给的字符串原样发回（`request.rs:127`、
  `request.rs:152`）。OpenAI 官方对 `tool_call_id` 的格式约束**未核实**（本片没有查 OpenAI 文档；适配器
  服务的是许多兼容端点，各家的约束也可能不同）。
- 两家的 id 在**生成**时都做了去重，保证整段 transcript 内唯一：
  - OpenAI adapter：provider 的 id 或 `call_{n}`，重复时加 `-{n}` 后缀（`stream.rs:191`、`stream.rs:298-306`）；
  - Anthropic adapter：provider 的 id 或 `toolu_{n}`，同样加 `-{n}`（`stream.rs:317`、`stream.rs:389-396`）。
  `-` 在 Anthropic 的 pattern 里合法，所以 `call_0-2` 这类 id 会被 Anthropic 接受。
- **历史里另一家的 id 会不会被拒绝？** 按 Anthropic 文档的 pattern，只要 id 只含
  `[a-zA-Z0-9_-]`（两家 adapter 今天产生的都满足），就不会被 schema 拒绝。OpenAI 侧未核实。**未在真实
  端点上验证**（本片不发请求）。

### 1.4 用现有契约测试 harness 能否写「上一轮来自另一家」的用例？

**不能直接写在共享契约 crate 里。** `crates/agent-provider-contract/src/lib.rs` 的 `ContractHarness`
只提供 `secret()`、`script(wires)`、三个「声明不支持」的出口（`unsupported_reasons`、
`unreported_usage_fields`、`context_overflow`），**没有请求体访问器**；`complete()` 只驱动
`Model::complete`，所有 `check_*` 断言的是**响应**（`lib.rs:142-216`、`lib.rs:220-224`）。契约是
「adapter 对每条 `Wire` 的响应语义」，与「请求里带了什么」无关。

**能写**，但要放在 adapter 自己的测试里，用现有的 `MockServer`（它记录请求）：

- `agent-provider-*/src/testing.rs` 的 `MockServer` 有 `requests() -> Vec<RecordedRequest>`、
  `RecordedRequest::header()`、`RecordedRequest::json()`（`testing.rs:94`、`:102`、`:134`）。
- 现有 `tests/http.rs` 已经在用（如 `crates/agent-provider-openai-compatible/tests/http.rs:132`、
  `:227`）。

用例形状（不用实现）：

1. **纯请求体单元测试**（最快，`src/request.rs` 的 `#[cfg(test)]`）：
   - 对 OpenAI-compatible：构造两条历史，一条 assistant 带 `provider_payload`（Anthropic thinking 块
     数组）和 `reasoning`，另一条只有 `content` + `tool_calls`；`build_body` 的字节必须相同。
     （现有 `reasoning_is_not_sent_to_the_provider` 是同一形状，只差 `provider_payload`。）
   - 对 Anthropic：构造一条 assistant 带 `reasoning`、`provider_payload = None` 的历史；
     `build_body` 的 `messages[0].content` 只含 `text`/`tool_use`，不含 `thinking`；再加一条
     `provider_payload` 为非数组的历史，断言 `Err(ModelError::Protocol)`。
2. **端到端（`tests/http.rs`）**：用 `MockServer` 跑 `complete`，请求的历史里混入另一家的
   `provider_payload` / `reasoning`，断言 `server.requests()[0].json()["messages"]` 里没有那些内容
   （或 Anthropic 侧只回传 `thinking`/`redacted_thinking`）。

若要把这条放进**共享契约**，需要给 `ContractHarness` 加一个请求捕获出口（例如
`fn last_request(&self) -> Option<Value>`），并新增一条 `check_foreign_history`。这是接口扩展，不是现有
契约能表达的。

### 1.5 结论：要让跨 provider 切换安全，适配器最少要改哪几处（只列，不实现）

1. **明确并测试「丢弃对方特有内容」**：OpenAI-compatible 侧现在**丢弃** `provider_payload`（含
   Anthropic thinking 签名块）；Anthropic 侧现在**丢弃** `reasoning`。这是**已经发生**的行为，不是
   新逻辑；要各加一条「上一轮来自另一家」的请求体测试把它钉住（见 1.4）。
2. **防非数组 `provider_payload`**：Anthropic 侧对非数组 payload 直接报 `Protocol`。跨 provider 时，
   若历史里有第三方 provider 写入的非数组 payload，会让整轮失败。要么在**写入端**保证
   `provider_payload` 只在 Anthropic adapter 里产生（今天如此），要么在 Anthropic 读取端把
   「不是数组」降级为「忽略」（**需要 决定**：静默忽略会丢掉必须回传的 thinking 块，
   ADR 0014 R10 的立场是宁可报错）。
3. **id 兼容性**：今天两家产生的 id 都满足 Anthropic 的 `^[a-zA-Z0-9_-]+$`。若要更稳，可在
   Anthropic 写入端对不匹配 pattern 的 id 做一次规范化；但**今天没有证据表明需要**（未在真实端点验证）。
4. **测试落点**：契约 crate 加请求捕获出口 + `check_foreign_history`，或在两家的 `tests/http.rs` 各加
   一条；ADR 0026 D6 说的「两套契约测试里各加一条」按现状**做不到**，要改成 adapter 测试（或先扩契约
   接口）。
5. **不做**：为了让请求通过而把对方的 `provider_payload` / `reasoning` 原样发出去（AGENTS.md：不得
   编造线上值；ADR 0014 R4：推理 provider 不可见）。

---

## 2. models.dev 目录里有哪些字段（ADR 0026 D0、ADR 0027 C2 的 `{display_name}`）

**来源**：本机缓存 `~/.radian/cache/catalog/models.json`（权限 `0600`，mtime `2026-10-13 19:05`，
5.6 MB）。这正是 `radian-host` 的 `cache_path()` 读的文件（`crates/radian-host/src/catalog.rs:471-478`
的 `cache_path()` = `~/.radian/cache/catalog/models.json`），TTL 24h（`catalog.rs:20`），**本片没有为此
发网络请求**（`models.dev/api.json` 只做了一次连通性探测，未下载）。

**解析现状**：`Catalog::parse`（`catalog.rs:181-218`）**只**读
`<provider>.models.<model>.limit.{context,output,input}`。provider 层的 `api`/`env`/`name`/`npm`、
model 层的 `name` 都**没有**被解析。

### 2.1 provider 层字段

四个例子原样摘录（`python3` 从缓存 JSON 提取；只摘相关字段）：

| provider id | `name` | `api` | `env` | `npm` |
|---|---|---|---|---|
| `opencode-go` | `"OpenCode Go"` | `"https://opencode.ai/inference/go/v1"` | `["OPENCODE_API_KEY"]` | `"@ai-sdk/openai-compatible"` |
| `opencode` | `"OpenCode"` | `"https://opencode.ai/inference/v1"` | `["OPENCODE_API_KEY"]` | `"@ai-sdk/openai-compatible"` |
| `anthropic` | `"Anthropic"` | **缺失** | `["ANTHROPIC_API_KEY"]` | `"@ai-sdk/anthropic"` |
| `openai` | `"OpenAI"` | **缺失** | `["OPENAI_API_KEY"]` | `"@ai-sdk/openai"` |
| `deepseek` | `"DeepSeek"` | `"https://api.deepseek.com"` | `["DEEPSEEK_API_KEY"]` | `"@ai-sdk/openai-compatible"` |

- **`api`（base URL）存在，但不是每个 provider 都有**：`opencode-go`/`opencode`/`deepseek` 有；
  `anthropic` 与 `openai` 在缓存里**没有 `api` 键**（它们靠 SDK 的默认端点）。所以「用目录的 `api`
  当默认 base URL」对 Anthropic/OpenAI 拿不到值，必须回退到 adapter 的默认。
- **`env` 是字符串数组**（key 的环境变量名），不是单个字符串。
- **`npm`** 是 AI SDK 包名（`@ai-sdk/openai-compatible` / `@ai-sdk/anthropic` / `@ai-sdk/openai`），
  是「用哪个线上协议」的一个现成线索（`anthropic` → Anthropic Messages，其余 → OpenAI 兼容）。
- 还有 `doc`（文档 URL）等字段（本片只列相关的）。

### 2.2 model 层字段

**有 `name`（显示名）**。四个例子原样摘录：

| provider / model id | `name` | `limit` |
|---|---|---|
| `anthropic` / `claude-opus-5-5` | `"Claude Opus 5.5"` | `{"context": 1000000, "output": 128000}` |
| `openai` / `gpt-5.4` | `"GPT-5.4"` | `{"context": 1050000, "input": 922000, "output": 128000}` |
| `deepseek` / `deepseek-v4-flash` | `"DeepSeek V4 Flash"` | `{"context": 1000000, "output": 393216}` |
| `opencode-go` / `qwen3.7-max` | `"Qwen3.7 Max"` | `{"context": 1000000, "output": 65536}` |
| `opencode-go` / `deepseek-v4.1-flash`（ADR 0026 D0 的例子） | `"DeepSeek V4.1 Flash"` | `{"context": 1000000, "output": 384000}` |

一个 model 对象还有 `id`、`description`、`family`、`attachment`、`reasoning`、
`reasoning_options`、`tool_call`、`structured_output`、`temperature`、`knowledge`、
`release_date`、`last_updated`、`modalities`、`open_weights`、`limit`、`cost`、
`canonical_model_id` 等（本片只列相关的）。`reasoning: true` / `tool_call: true` 是布尔能力位，
可作为描述符模型列表的补充（**未核实**这些位是否对所有 provider 都可靠）。

### 2.3 对 ADR 0026 D0 / ADR 0027 C2 的直接影响

- ADR 0027 C2 的 `model` 条目 `{display_name}`：**目录有 model 的 `name`**，可直接用作 `display_name`
  的回退来源（描述符拿不到时）。但 `Catalog::parse` 今天不读它，要加解析（只加字段，不改现有
  `limit` 语义）。
- ADR 0026 D0 的「描述符默认 base URL」：目录的 `api` 可作为 `opencode-go`/`opencode`/`deepseek`
  的来源；`anthropic`/`openai` 没有 `api`，描述符要自带默认。
- `env` 数组可作为「该 provider 的 key 环境变量名」的参考，但 ADR 0026 D2 已决定 key **只从
  `auth.json`**，所以 `env` 只用于 `/login` 的提示，不作为运行时来源。

---

## 3. opencode-go 用量接口的响应格式（ADR 0027 C4）

**来源**：`monotykamary/pi-opencode-go-provider`，commit
`aec40ba1708832a14e50cc9975ece45a2d6346dc`（2026-10-13T06:42:55Z，release v1.1.23），文件
`usage.ts`、`usage-controller.ts`、`config.ts`。从 `raw.githubusercontent.com` 只读读取，访问
2026-10-13。**没有**用任何真实 key 调这个接口。

### 3.1 请求

`usage.ts` 顶部的注释与 `USAGE_URL` 常量：

- URL：`GET https://opencode.ai/zen/go/v1/usage`（`usage.ts`：`export const USAGE_URL = "..."`）。
- 方法：`GET`。
- 请求头（`usage-controller.ts` 的 `fetch` 调用）：`accept: application/json`、
  `authorization: Bearer <api-key>`。key 来自 `ctx.modelRegistry.getApiKeyForProvider("opencode-go")`，
  回退到 `process.env.OPENCODE_API_KEY`。
- 超时：`REQUEST_TIMEOUT_MS = 10_000`（10 秒），与调用方的 `AbortController` 一起用
  `AbortSignal.any`。
- 服务器来源（注释给出）：`sst/opencode packages/console/app/src/routes/zen/go/v1/usage.ts`、
  `packages/console/core/src/subscription.ts`。

### 3.2 响应 JSON 形状

`usage.ts` 顶部注释原文：

> `200 { "usage": { "rolling" | "weekly" | "monthly": { "status": "ok" | "rate-limited", "percent": 0-100, "resetsAt": "<ISO-8601>" } } }`
> `401 AuthError (missing/unknown key), 403 EntitlementError (no Go plan)`

- 顶层键 `usage`；三个窗口键 `rolling` / `weekly` / `monthly`（`UsageWindowKey`）。
- 每个窗口：`status`（`"ok"` / `"rate-limited"`，其它归 `"unknown"`）、`percent`（0–100，**已用**占比）、
  `resetsAt`（ISO-8601 字符串；`parseResetAt` 也接受 epoch 秒/毫秒）。
- 窗口标签：`rolling`→`"5h"`、`weekly`→`"7d"`、`monthly`→`"30d"`（`USAGE_WINDOW_LABELS`）。
- **`bankedResets` 是可选的**，注释写明「The Go API does not expose this today」，`parseUsageSnapshot`
  里 `parseBankedResets` 取不到就返回 `null`，不显示。
- 金额上限**不在线上**：`USAGE_LIMITS_NOTE = "5h $12 · 7d $30 · 30d $60"` 只是展示用的常量。
- 解析失败（没有可用窗口）返回 `undefined`，调用方报「no data」，不编 0。

### 3.3 `5h: 80%` 是剩余量还是已用量

**剩余量。** `usage.ts` 的 `UsageWindow` 注释与 `parseWindow` 写得很明确：

- 线上 `percent` = **已用**占比：注释原文「`percent` is the share of the window budget already spent
  (`floor(min(100, usage / limit * 100))`, forced to 100 once the window is rate limited)」。
- `parseWindow` 存 `usedPercent = clampPercent(percent)`，并算
  `remainingPercent: 100 - usedPercent`。
- 所有展示（`usageSegments`、`formatUsageDetail`、`line()`）都用 `remainingPercent`。
  所以 widget 里的 `5h: 80%` 是**还剩 80%**。这与 ADR 0027 C4「百分比是剩余量，不是已用量」一致。
- 颜色阈值也按剩余量：剩余 ≤ 30% → `warning`，剩余 ≤ 10% 或 `rate-limited` → `critical`
  （`WARNING_REMAINING_PERCENT = 30`、`CRITICAL_REMAINING_PERCENT = 10`）。

### 3.4 刷新周期与错误处理

- **默认 60 秒**：`config.ts` 的 `DEFAULT_USAGE_CONFIG.refreshIntervalMs = 60_000`；可配范围
  `MIN_USAGE_REFRESH_MS = 15_000`、`MAX_USAGE_REFRESH_MS = 10 * 60_000`（15 秒–10 分钟，
  `clampRefreshInterval`）。与 ADR 0027 C4「默认 60 秒，可在 15 秒到 10 分钟之间配置」一致。
- **触发时机**（`usage-controller.ts`）：`start()` 时 `refresh(..., { force: true })` 一次；然后
  `setInterval(..., config.refreshIntervalMs)`；另外「session start, timer, turn/model events」也会
  触发（模块注释）；`/opencode-go-usage`（`notify`）**总是**轮询。没有 UI（print/JSON 模式、子 agent）
  时**完全不轮询**（`hasUI(ctx)`）。
- **跨进程去重**：`USAGE_SYNC_TTL_MS = 30_000`（默认轮询间隔的一半）。每 30 秒最多一个进程真正轮询，
  把快照写到 `<agentDir>/cache/opencode-go-usage.json`，其余进程读该文件。写盘的对象是
  `{ version: 1, keyFingerprint, fetchedAt, snapshot }`——**只存 key 指纹，不存 key**。
- **错误处理**：
  - 非 2xx：`usageHttpErrorMessage(status, payload)` 给可读信息——401 →
    `"OpenCode Go rejected the API key (401)"`；403 → `"This key has no OpenCode Go subscription (403)"`；
    其它 → `"OpenCode Go usage request failed ({status})"`；若 `payload.error.message` 有值则附上。
  - 失败时**保留上一次的快照**，把 `usageError` 设上；`isStale()` 为真时 `line()` 在末尾加 ` · stale`。
  - 没有 key → `MISSING_KEY_MESSAGE`（提示设 `OPENCODE_API_KEY` 或写进 auth.json），清空快照。
  - 失败被 `catch` 住，不向调用方抛；只有 `/opencode-go-usage`（notify）会额外发一条 warning notice。
- 其它默认（`config.ts`）：`placement: "belowEditor"`、`showResetTimes: true`、
  `showOnlyOnProvider: true`（只在当前模型来自 opencode-go 时显示）。

---

## 对 ADR 0026 / 0027 的影响

### 成立的提议

- **ADR 0026 D6（跨 provider 切换进首版）**：现有适配器**已经**丢弃对方特有的
  `reasoning`（两家）与 `provider_payload`（OpenAI 侧），不会把对方的内容原样发出去；两家的 id 都满足
  Anthropic 的 `^[a-zA-Z0-9_-]+$`。所以「切到另一家」在请求构造上是**安全**的（内容被丢弃，不报错，
  除 1.2 里那个今天不可达的非数组 payload 例外）。
- **ADR 0026 D0**：目录的 provider `api` 可作为 `opencode-go`/`opencode`/`deepseek` 的默认 base URL；
  model `name` 可作为显示名来源。
- **ADR 0027 C2** 的 `{display_name}`：目录**有** model `name`，可作回退。
- **ADR 0027 C4** 的 URL、认证、响应形状、刷新周期、错误处理、`5h: 80%` 是剩余量——**全部与代码一致**。

### 要改的提议 / 实现注意

- **ADR 0026 D6 的「两套契约测试里各加一条」按现状做不到**：共享契约 crate 的 `ContractHarness`
  没有请求体访问器，`check_*` 只断言响应。要么把用例放在两家的 `tests/http.rs`（用 `MockServer` 的
  `requests()`），要么先给契约加一个请求捕获出口（例如 `fn last_request(&self) -> Option<Value>`）再写
  `check_foreign_history`。这是接口扩展，需要 决定。
- **`Catalog::parse` 要加解析**：现在只读 `limit.{context,output,input}`；要用 `name` 作 `display_name`
  或 `api` 作默认 base URL，得加字段（只加，不改现有语义）。
- **非数组 `provider_payload`**：Anthropic 侧现在报 `Protocol`。跨 provider 时若第三方 provider 写了
  非数组 payload，会让整轮失败。今天不可达；加第三家前要么保证 payload 只在 Anthropic adapter 里产生，
  要么决定是否把「不是数组」降级为忽略（**需要 决定**；ADR 0014 R10 的立场是宁可报错）。

### 仍未核实

- **OpenAI 对 `tool_call_id` 的格式约束**：本片没有查 OpenAI 官方文档；OpenAI 兼容适配器服务许多端点，
  各家的约束可能不同。Anthropic 侧的 `^[a-zA-Z0-9_-]+$` 已从官方 API 文档核实。
- **`provider_payload` 非数组时 Anthropic 的真实服务器反应**：本片只核实了 adapter 会返回
  `Protocol`（本地），没有真实端点验证服务器会怎么拒绝。
- **`reasoning_options` / `reasoning` / `tool_call` 等能力位是否对所有 provider 可靠**：只看了缓存里的
  取值，没有核对 models.dev 的 schema 或各 provider 文档。
- **opencode-go 的 `bankedResets` 未来是否上线**：代码注释说今天不暴露；若上线，ADR 0027 C4 的显示
  可能要加一段。
- **`resetsAt` 是否总是 ISO-8601**：`parseResetAt` 同时接受 epoch 秒/毫秒，说明实现者见过别的形态；
  线上实际形态本片未在真实响应里验证。
