# 深挖 5：模型与 provider 层

> 范围：协议、prompt cache、流式、重试与超时、推理档位、小模型分流、token 与成本、每轮请求体。
> Radian 基线：`main` @ `356df1e`。对手源码在 scratchpad 的 `comp/` 下（只读，没有运行）。Claude Code 只用官方文档。
> 记号：**已核实** = 读过源码或文档原文；**推断** = 从代码或文档推出来，没有实测；**未核实** = 没有找到证据。

## 1. 一句话结论

Radian 的 provider 层正确性好（三个 adapter 都过契约套件，用量口径统一，按调用计价），但**省时省钱的手段几乎都还没做**：
- ChatGPT 路由每次调用都新建 HTTP 客户端，连接不能复用；
- OpenAI 路由不发 `prompt_cache_key`；
- Anthropic 只用 3 个缓存断点，单轮新增超过 20 个块时会整段 miss；
- 压缩摘要请求不共享对话前缀，缓存完全用不上；
- Responses 每轮都全量重发历史，没有 WebSocket 增量；
- 看不到缓存命中率；
- 没有小模型分流。

前四项改动小，收益直接，应该先做。

## 2. 对比表

| 维度 | Radian | pi (pi-mono) | Codex CLI | Claude Code | omp 18.12.1 | opencode |
|---|---|---|---|---|---|---|
| 支持的协议 | Chat Completions、Anthropic Messages、Responses（SSE） | 很多 api（Messages、Completions、Responses、Codex Responses、Bedrock、Google 等）（`pi-mono/packages/ai/src/api/`） | 只有 Responses，HTTP SSE 和 WebSocket 两种（`codex/codex-rs/core/src/client.rs:11-21`） | 只有 Anthropic Messages（及其云厂商托管，文档） | 用 pi-ai 的协议集（本机只有 coding-agent 包，**未核实**细节） | 很多协议（`opencode/packages/ai/src/protocols/`） |
| Anthropic 缓存断点 | 3 个：最后一个工具、system 末尾、最后一条消息的最后一个可缓存块（`crates/agent-provider-anthropic/src/request.rs:21-25,71-107`） | system 和最后一个工具，最后一条 user 消息的最后一块（`pi-mono/packages/ai/src/api/anthropic-messages.ts:1228-1252,1529-1552,1659`） | 不适用 | 自动管理；会话中途追加的系统上下文块也单独打标记（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)） | 不明 | 最后一个工具，第一段和最后一段 system，对话末尾，最多 4 个（`opencode/packages/ai/src/cache-policy.ts:5-11,17-21`） |
| Anthropic TTL | 只有 5 分钟，不发 `ttl`（`crates/agent-provider-anthropic/WIRE.md:67`） | `short`/`long`，`long` 发 `ttl:"1h"`（`anthropic-messages.ts:79-92`） | 不适用 | 主对话在订阅额度内用 1h，其余 5m；`promptCacheTtl` 和 `subagentPromptCacheTtl` 可以按桶设置（文档） | 不明 | 可以配 `ttlSeconds`（`cache-policy.ts:68-69`） |
| OpenAI `prompt_cache_key` | **不发**；chatgpt 路由发了 `session_id` 头（`crates/radian-host/src/provider.rs:574-586`） | 发会话 id，截到 64 字符（`pi-mono/packages/ai/src/api/openai-responses.ts:355-364`） | 每个请求都发；子 agent 用 `{source}:{parent_thread_id}`（`client.rs:646-668,1060`） | 不适用 | 不明 | 发会话亲和 id（`opencode/packages/core/src/session/model-request.ts:278-295`；`ai/src/protocols/shared.ts:57-71`） |
| Responses 增量 | 无；`store:false`，每轮全量（`crates/agent-provider-openai-responses/src/request.rs:48-53`） | Codex 默认 `transport:"auto"`，WebSocket 上用 `previous_response_id` 只发增量（`openai-codex-responses.ts:295,1467-1476,1519`；`coding-agent/src/core/settings-defaults.ts:5`） | WebSocket 默认开（OpenAI provider `supports_websockets: true`），比较上一请求后只发新增条目，还有 `generate=false` 预热（`model-provider-info/src/lib.rs:561`；`client.rs:1500-1531,2137-2160`） | 不适用 | 不明 | OpenAI Responses 走 WebSocket channel transport（`ai/src/protocols/openai-responses.ts:21,310-315`） |
| 请求压缩 | 无 | 未核实 | ChatGPT 后端用 zstd，默认开（`client.rs:1733-1742`；`features/src/lib.rs:1446-1450`） | 未核实 | 未核实 | 未核实 |
| 连接复用 | Anthropic 和兼容路由按 route 缓存 adapter，可以复用；**ChatGPT 路由每次调用新建 `reqwest::Client`**（`crates/radian-host/src/model.rs:229-252`） | 未核实 | 复用 WebSocket，跨轮缓存连接（`client.rs:11-15`） | 未核实 | 未核实 | 未核实 |
| 缓存保温 | 无 | 默认 `streaming`：快到 TTL 时用 `maxTokens:1` 重放，预期省下 ≥$0.05 才发（`coding-agent/src/core/cache-warmer.ts:15-31,377-399`；`settings-defaults.ts:61`） | 无 | 无（用 1h TTL 代替） | 有，从 pi 移植，默认 `idle`（`CHANGELOG.md:1465`） | 无 |
| 缓存命中可见 | 只计费，不显示命中率和 miss | `cache-stats.ts` 检测每轮 miss，可开 `showCacheMissNotices` | 记录 incremental 和 full 的原因（`client.rs:2091-2118`） | `/usage` 有 `Prompt cache (main)` 行：命中比例、miss 次数、可能原因、是否还热（[costs](https://code.claude.com/docs/en/costs)） | 有 cache 回归修复记录 | 记录 cache read/write（`core/src/session/usage.ts`） |
| 重试 | 3 次，2/4/8 s 加抖动，尊重 `Retry-After`，总等待 ≤60 s（`crates/agent-core/src/turn.rs:161-182`；`agent.rs:22`） | 3 次，base 2 s，单次 ≤60 s；认 `x-should-retry` 和 `retry-after-ms`（`ai/src/utils/provider-retry.ts:24-37,54`） | 请求重试 4 次，base 200 ms，**不重试 429**；流重试 5 次（`model-provider-info/src/lib.rs:66-71,455-460`；`codex-client/src/retry.rs:43-52`） | 默认 10 次（`CLAUDE_CODE_MAX_RETRIES`，[env-vars](https://code.claude.com/docs/en/env-vars)） | 未核实 | 10 次，2 s 起，单次间隔 ≤10 s，总共约 84 s；`retry-after` 上限 15 分钟（`core/src/model-retry.ts:12,22-28`） |
| 超时 | connect 30 s，idle 120 s（`agent-provider-anthropic/src/config.rs:131-132`；responses 同值） | 可配 | stream idle 300 s，WebSocket connect 15 s（`model-provider-info/src/lib.rs:66,71`） | 请求 600 s，body idle 5 分钟（文档） | 未核实 | 未核实 |
| 默认推理档位 | `medium`；Anthropic 的 medium = `adaptive`（`provider.rs:872-885,2049`）；压缩摘要默认 `low`（ADR 0018） | 未核实 | 按模型在 `models.json` 给出 low 或 medium（`models-manager/models.json`） | 5.5 系列默认 medium，其它多为 high（[model-config](https://code.claude.com/docs/en/model-config)） | 按角色 | GPT-5 默认 medium 并带 `reasoningSummary:auto`（`ai/src/providers/openai-options.ts:35-51`） |
| 小模型分流 | **无**：子 agent 用父模型（`radian-host/src/subagent.rs:76-81`），压缩模型只能是 `current`（ADR 0018:321-323），没有标题生成 | 未核实 | `review_model`；压缩失败时可以换模型（`core/src/config/mod.rs:649`；`core/src/compact_model_fallback.rs`） | Haiku 档跑后台功能（`ANTHROPIC_DEFAULT_HAIKU_MODEL`）；子 agent 可设 `model: haiku`（文档） | 角色 `smol`/`tiny`/`memory`/`slow`/`advisor`；标题、提交信息、记忆、自动修复走 smol（`src/config/model-resolver.ts:1103-1132`；`src/edit/auto-repair.ts:349`） | `small()` 按 luna、flash-lite、flash、haiku 顺序挑模型生成标题（`core/src/model.ts:271-283`；`session/context.ts:95-103`） |
| 压缩请求和对话前缀 | **不共享**：另一个 system，没有 tools，历史压成纯文本（`crates/agent-core/src/compaction.rs:356-385`） | 同 Radian，`serializeConversation` 加专用 system（`coding-agent/src/core/compaction/compaction.ts:684,722`） | 共享：原历史加原 base instructions（`core/src/compact.rs:296-323`）；OpenAI provider 还可以远程压缩（`model-provider/src/capabilities.rs:33`） | 共享：同 system、同 tools、同历史，末尾追加一条摘要指令（文档） | 未核实 | 有 provider 原生压缩（`compaction_trigger` 和 `context_management`，`openai-responses.ts:218-237`） |
| 成本统计 | 每次调用按当时价格计价，缓存 token 按缓存价，费用不全时标 `≥`（`crates/radian-host/src/cost.rs:1-19`） | `calculateCost`，保温时做经济判断 | 遥测 | `/usage`，本地按牌价估算（文档） | `omp-stats` | `session/stats.ts` |

## 3. 各家关键做法（只列和快、省有关的）

**Codex CLI**
- **WebSocket 加增量**：`get_incremental_items` 把上一请求的 input 加上服务端新产出的条目，和这次的 input 逐项比较；前缀一致时只发多出来的部分，并带上 `previous_response_id`（`client.rs:1500-1531,2137-2160`）。模型、tools、reasoning、`prompt_cache_key` 等任何属性变了，就退回全量请求，并记下原因，比如 `tools_changed`（`client.rs:384-458`）。每轮开始前会发 `generate=false` 预热，提前建连并拿到 response id（`client.rs:17-21`）。整个过程一直是 `store:false`（`client.rs:1085`）。由此推断：服务端把上一次 response 保存在连接本地。
- **prompt_cache_key**：默认用会话 id；内部子会话用 `{source}:{parent_thread_id}`，让子会话和父会话落在同一个缓存亲和桶里（`client.rs:646-668`）。代码注释写明「ChatGPT derives cache affinity from the Responses session-id header」（`client.rs:659-660`）。
- **zstd 压缩请求体**：只对 ChatGPT 后端和 OpenAI provider 开启，状态 Stable，默认开（`client.rs:1733-1742`）。
- **压缩复用前缀**：本地压缩把原历史和原 base instructions 原样发出，末尾追加摘要提示，同一个 client session 里重试（`compact.rs:283-323`）。
- **重试**：429 不重试（`retry_429:false`，`model-provider-info/src/lib.rs:457`），5xx 和传输错误重试，base 200 ms，抖动 0.9–1.1。

**pi（pi-mono）**
- **缓存保温**（`coding-agent/src/core/cache-warmer.ts`）：在 TTL 的 90% 处、且至少留 10 s 余量时，重放上一个请求，`maxTokens:1`（`:29-31,335`）。只有「预期省下的 miss 成本 − 保温成本 ≥ $0.05」才发（`:20,377-399`）。idle 阶段假设下一次真实请求在缓存过期前到来的概率是 15%（`:26`）。保温最长 60 分钟，idle 阶段 30 分钟（`:15-18`）。Anthropic 预算式 thinking 不重放，因为 `budget_tokens` 跟 `max_tokens` 绑定，会改变缓存键（`:47-57`）。
- **Cache miss 统计**（`cache-stats.ts:4-11,144-168`）：把每轮相对上一轮应该命中却被重新计费的 token 记为浪费，并在 transcript 里提示。
- **长 TTL**：`PI_CACHE_RETENTION=long` 时，Anthropic 用 `ttl:"1h"`，OpenAI 用 `prompt_cache_retention:"24h"` 或 `prompt_cache_options.ttl:"30m"`（`anthropic-messages.ts:69-92`；`openai-responses.ts:117-132`）。
- **会话亲和头**：`session_id`、`x-session-affinity`、`x-client-request-id`（`openai-completions.ts:783-786`；`compat-schema.ts:332`）。

**Claude Code（文档）**
- 文档把缓存当成一等指标：列出每个会让缓存失效的操作，比如换模型、改 effort、开关 MCP、压缩、图片过多。缓存还热时，`/model` 会先让用户确认（[prompt-caching](https://code.claude.com/docs/en/prompt-caching)）。
- 摘要请求「same system prompt, tools, and history … plus a summarization instruction appended」，所以 `/compact` 在缓存热的时候只需付很小一部分费用（同上）。
- TTL 分桶：订阅额度内主对话用 1h，子 agent 和压缩用 5m；workflow 扇出时同前缀的 agent 晚 5 s 再发，让后面的读前一个写下的缓存（同上）。
- 后台功能用 Haiku 档模型（`ANTHROPIC_DEFAULT_HAIKU_MODEL`，[env-vars](https://code.claude.com/docs/en/env-vars)），文档说后台花费一般每会话不到 $0.04（[costs](https://code.claude.com/docs/en/costs)）。
- 网关把 `cache_control` 拒成 400 时，Claude Code 把标记挪到最后一条消息，再重发（prompt-caching 文档）。

**omp**
- 模型角色：`smol` 和 `tiny` 跑标题、提交信息、记忆、编辑自动修复；`slow` 跑 advisor（`src/config/model-resolver.ts:1103-1132`；`src/utils/commit-message-generator.ts:59`）。
- 做过实验：标题生成改为从主请求 fork 出来，复用同一个缓存键，用 `bench-title-fork.ts` 量 `cacheRead`（`scripts/bench-title-fork.ts:1-20`）。
- CHANGELOG 里有大量「某操作打破 prompt cache」的修复，例如 MCP 指令顺序、`/tan` 子会话工具表和父会话不一致、思考档位切换（`CHANGELOG.md:282,292,459`）。可见缓存稳定性要靠持续的回归测试来守。

**opencode**
- 缓存策略按路由打表：Gemini 只认最后一个断点，对话末尾的断点每一步都写新缓存，反而更贵，所以 Gemini 不打断点；Qwen 不认工具上的断点（`ai/src/cache-policy.ts:57-66`）。
- system 首尾各一个断点（`cache-policy.ts:94-104`）：base 和 project 前缀可以分别复用。
- `prompt_cache_key` 截到 64 字符（`shared.ts:63-71`）；带多个会话亲和头（`model-request.ts:281-292`）。
- 重试 10 次，单次间隔 ≤10 s；超时类重试最多 3 次（`model-retry.ts:22-40`）。

## 4. Radian 的优势

1. **用量口径对**：`input_includes_cache` 由 adapter 自己声明，缓存 token 只按缓存价计一次。每次调用按当时 route 的价格计价，费用不全时标 `≥`（`cost.rs:1-19`；ADR 0007）。很多实现在这里多算或少算（见 `docs/research/2026-10-14-cost-accounting.md` 的 F1）。
2. **前缀天然稳定**：系统提示在进程内只组装一次（ADR 0036）；会话中途的变化都追加成消息，工具定义不撤；Anthropic 默认打断点。Claude Code 文档列出的多种失效原因，Radian 在架构上已经避开了一大半。
3. **重试有上限、可中断**：总等待 ≤60 s；backoff 期间取消立即生效（`turn.rs:236-318`）。不会像 opencode 那样把 `retry-after` 放到 15 分钟。
4. **压缩摘要已降档**：默认 `low`（ADR 0018），比沿用对话档位省输出 token。
5. **三个 adapter 都过契约测试**：新加一个线上字段（例如 `prompt_cache_key`）有现成的测试框架可以套用。
6. **不额外调模型**：没有标题生成、提示建议这类后台请求，所以没有隐性成本。

## 5. Radian 的劣势和缺口

1. **ChatGPT 路由每次调用新建 `reqwest::Client`**（`radian-host/src/model.rs:240-251` → `agent-provider-openai-responses/src/model.rs:90-96`）。
   - 新 client 有自己的空连接池，所以每次模型调用（工具往返中的每一步都算）都要重新做 TCP、TLS 和 HTTP/2 握手。
   - `model.rs:210-212` 的注释说「per call costs nothing a long-lived instance would have saved」，这个判断**不对**：它漏掉了连接复用。
   - 每次多出的延迟按 1–3 个 RTT 估，约几十到几百毫秒（**推断，未实测**）。
   - 另外两条路由由 `SwitchableModel` 缓存 adapter（`model.rs:807-829`），没有这个问题。
2. **OpenAI 路由不发 `prompt_cache_key`**（`agent-provider-openai-responses/src/request.rs` 全文没有这个字段；gap 报告 1b 也提过）。
   - chatgpt 路由有 `session_id` 头。按 Codex 的注释，ChatGPT 可能靠这个头做亲和（**推断**）。
   - API key 的 `openai` 描述符用 `no_headers`（`provider.rs:562-564,1085-1118`），既没有键也没有亲和头。
3. **Anthropic 只用 3 个断点，单轮新增超过 20 个块时整段 miss**。
   - 官方规则：读缓存时从每个断点往前最多回看 20 个块。Radian 的 wire notes 记下了这条，但判断「本 adapter 不受影响」（`crates/agent-provider-anthropic/WIRE.md:69`）。
   - 在 agent 循环里，一次回复里有 N 个并行工具调用，下一请求就多出约 2N 个块（N 个 `tool_use` 加 N 个 `tool_result`）。N ≥ 10 时，最后一个断点回看不到上一轮的断点，对话部分只剩 tools 和 system 能命中（**推断**，依据是文档规则）。
   - Radian 的 `subagent` 支持并行，不少工具是 `Shared` 并发，这种情况会出现。第 4 个断点空着没用（`request.rs:21-25`）。
4. **压缩摘要请求不共享前缀**（`agent-core/src/compaction.rs:356-385`）。
   - 它换了 system，去掉 tools，历史压成纯文本，所以被摘要的部分全部按未缓存价计费，并且要完整 prefill。
   - Claude Code 和 Codex 都复用前缀（见 §3）。
   - 这是从 pi 继承的设计。当初的理由是防止工具调用配对被弄坏。
5. **没有 Responses 增量和 WebSocket**：每轮把全部历史序列化后上传（`Anthropic::complete` 和 Responses 的 `build_body(...).to_string()`）。
   - 200k token 上下文大约 0.8 MB（按 4 字节/token 估）。上行 10 Mbps 时光上传就约 0.6 s（**推断**）。
   - Codex、pi、opencode 三家在 OpenAI 路由上都做了 WebSocket 增量。
6. **看不到缓存**：footer 和 `/usage` 只显示费用，没有命中率和 miss 原因；也没有每次调用的首 token 延迟。`benchmarks/perf-baseline.md:207` 写明真实 provider 的首 token 未测。没有这些数据，上面几项改进都无法验证。
7. **没有小模型分流**：子 agent 只能用父模型，`subagent` 的 schema 只有 `agent` 和 `task`（`subagent.rs:189-203`）。`explore` 只读、只搜索，却按主模型的价格跑。
8. **子 agent 不和父会话共享缓存**：`general` 子 agent 的工具表是「父工具表去掉 `subagent`」，所以从 tools 层就和父会话前缀不同（`subagent.rs:183`）。OpenAI 路由上，子进程用自己的会话 id（`subagent.rs:279`）。Codex 让子会话沿用父会话的缓存键，omp 修复过 `/tan` 工具表不一致造成的 miss（`CHANGELOG.md:292`）。
9. **只有 5 分钟 TTL，没有保温**：用户离开超过 5 分钟回来，第一轮要整段重写缓存。Anthropic 缓存写入是 1.25 倍价，按官方价格表，**未在本文复核**。
10. **`/thinking` 和 `/model` 切换不提示缓存代价**。改 thinking 参数会让 Anthropic 的消息缓存失效（官方文档；Claude Code 文档也说「most models」如此）。Claude Code 在缓存热的时候会先让用户确认。
11. **idle 120 s**，比 Codex 的 300 s 和 Claude Code 的 5 分钟都短。chatgpt 路由没有设 `reasoning_summary`（`model.rs:240-247`）。高推理档时服务端可能长时间不发字节，导致误判超时，整次调用从头重来（**推断，未实测**）。
12. **重试信号不全**：没有读 `x-should-retry` 和 `retry-after-ms`。pi 按 SDK 的策略读了这两个（`provider-retry.ts:24-37,54`）。

## 6. 改进方案（按收益/成本排序）

### P1 ChatGPT 路由复用 HTTP 连接
- **做什么**：每个 route 只建一次 `reqwest::Client`，鉴权头改为每次调用时加。具体有两种办法：
  - 给 `OpenAiResponses` 加一个 per-call 额外头的入口，例如 `complete_with_headers`，或在 config 里放 `Arc<dyn Fn>` 头提供者；
  - 或者 `ChatGptModel` 自己持有一个 `Client`，传进 adapter。

  access token 仍然每次调用都解析，ADR 0026/3 不变。顺手修正 `model.rs:210-212` 的错误注释。
- **为什么**：工具往返中的每一步都省一次握手（见 §5-1）。另外两条路由已经这样做了。
- **成本**：小，半天左右。adapter 的公共 API 要加一个入口（是 adapter crate，不是 `agent-core`）。
- **ADR**：不推翻决定，在 ADR 0026 补一条实施记录即可。
- **验证**：mock server 统计 accept 次数，连续 3 次调用只应有 1 次 accept；变异测试：每次调用都新建 client，测试应该变红。再用真实 chatgpt 路由对比改前改后工具往返的中位延迟，各跑 n≥10。

### P2 Responses 路由发 `prompt_cache_key`；子 agent 沿用父会话的键
- **做什么**：
  - `OpenAiResponsesConfig` 加 `prompt_cache_key: Option<String>`，宿主填会话 id，截到 64 字符（opencode `shared.ts:63-71`）。
  - 子进程收到父会话的 id 当缓存键，这是 Codex 的做法（`client.rs:650-655`）。
  - Chat Completions 路由只对确认支持的描述符发这个字段。opencode 默认对 chat 不发，见 `openai-chat.ts:793`，不支持的端点可能拒绝未知字段。
- **为什么**：OpenAI 的前缀缓存靠路由亲和，没有这个键时并发和长会话的命中率下降（**推断**）。四家里有三家都这样做。
- **成本**：小。
- **ADR**：ADR 0026 决定 4 列了请求字段，要补一条修订；只是加字段，不是推翻。还要守契约规则「不编造 provider 不发的值」：这个字段是请求侧的，不受影响。
- **验证**：契约测试加一例，断言请求体带键且长度 ≤64。线上 A/B：同一任务比较 `cache_read / input` 的比例。

### P3 Anthropic 第 4 个断点：放在上一请求的末尾
- **做什么**：除了最后一条消息，再在**上一次请求的最后一条消息**上打断点，也就是倒数第二个 user 消息里最后一个可缓存块。只在两者之间的块数超过 20 时才打也可以，但总是打更简单。总数仍 ≤4（`MAX_CACHE_BREAKPOINTS`）。
- **为什么**：并行工具轮会超出 20 块的回看范围（见 §5-3）。补这个断点后，上一轮写下的条目一定能被读到。
- **成本**：小，只改 `cache_last_message_block` 和测试。
- **ADR**：没有冲突；修正 wire notes §2.4 中「不受影响」的判断。
- **验证**：单测构造一轮 12 个工具调用，断言两个消息断点的位置。线上用 12 路并行 `read` 的回放对比下一请求的 `cache_read_input_tokens`。

### P4 缓存命中和首 token 延迟的可见性（其它几项都靠它来衡量）
- **做什么**：
  - 每次调用记录 `cache_read / (input + cache_read + cache_write)`，和首个 delta 的时间。
  - 学 pi 的 `cache-stats.ts`：本轮应命中的前缀没有命中，就记一次 miss。
  - footer 或 `/usage` 显示会话命中率和最近一次 miss，参照 Claude Code 的 `Prompt cache (main)` 行。
  - `radian exec --json` 也输出这些数字，评测时可以直接读。
- **成本**：小到中。
- **ADR**：footer 内容碰 ADR 0027；事件要不要持久化碰 ADR 0010。首 token 延迟是 live 事件，不进 journal 也不违反 ADR 0010，但要写明。
- **验证**：mock adapter 给出固定 usage，断言显示的比例；变异测试：分母漏掉 `cache_write`，测试应该变红。

### P5 压缩摘要复用对话前缀
- **做什么**：摘要请求用**原 system、原 tools、原消息**，末尾追加一条 user 摘要指令。指令里写明「不要调用工具」，可以的话再设 `tool_choice:none`。摘要输出的长度上限和三档兜底不变。
- **为什么**：被摘要的历史这时按缓存价计费（Anthropic 缓存读取约为输入价的十分之一，**按官方价格表，未在本文复核**），prefill 也更快。在热缓存下，压缩的花费和等待都会大幅下降，Claude Code 文档原话是「costs a fraction」。
- **难点**：
  1. 现在把历史压成纯文本，是为了避开工具配对问题（`compaction.rs:349-355`）。改成原样发送后，必须保证切点前的历史本身配对完整。core 的 transcript 已经保证了这一点（ADR 0002），需要复核。
  2. ADR 0018 让摘要用 `low` 档。在 Anthropic 上改 thinking 会让消息缓存失效，所以复用前缀和降档冲突，只能二选一。可以按 provider 分：Anthropic 用 `current`，OpenAI 路由保留 `low`（reasoning effort 是否进缓存键，**未核实**）。
- **成本**：中。
- **ADR**：要给 ADR 0018 新开修订，改「摘要请求的形状」，并说明修订 8 决定 1 在 Anthropic 上的例外。还会碰 journal-selfcheck 的回放，只要摘要本身怎么记录不变就行。
- **验证**：真实会话里摘要调用的 `cache_read` 应该大于 0；压缩耗时前后对比；两套契约测试都保持绿。

### P6 可选的 1h TTL 和缓存保温
- **做什么**：
  - 加 `[providers.anthropic] cache_ttl = "5m" | "1h"`，默认仍 5m；
  - 成本较低的方案：学 pi 的保温器，但只在 `streaming` 阶段（工具执行超过 TTL 时）保温，并用 pi 的经济阈值（≥$0.05）。
- **为什么**：长时间的构建或测试（`shell` 后台任务，ADR 0037）常超过 5 分钟。回到下一步时整段缓存要重写。
- **成本**：TTL 选项小；保温中等，需要定时器、经济判断和取消逻辑。
- **ADR**：保温是 Radian 主动发出的额外模型调用，要先确认它和 ADR 0013（默认不发遥测）、ADR 0017 的边界，并开新 ADR。TTL 只是配置项，ADR 0008 允许加字段。
- **验证**：`cache_creation.ephemeral_1h_input_tokens` 出现；有保温和无保温的长工具场景对比 `cache_write`。

### P7 Responses 走 WebSocket，增量发送
- **做什么**：在 `agent-provider-openai-responses` 里加 WebSocket transport。adapter 记住上一请求的 input 和本次响应产出的条目；新请求是它的严格扩展、且其余属性都不变时，只发新增条目和 `previous_response_id`，否则发全量（照 Codex `client.rs:384-458,1500-1531`）。WebSocket 失败就退回 SSE（pi `openai-codex-responses.ts:297-368`）。core 的 `ModelRequest` 不变，所以 `agent-core` 不动。
- **为什么**：上传量从整段历史降到一步的新增，服务端也不必重新解析整段历史，首 token 延迟和带宽都下降。三家都做了，可见收益是实在的（**推断**；Radian 尚未实测）。
- **成本**：高。要加 WebSocket 依赖，例如 `tokio-tungstenite`，需要说明理由；要处理连接上限和 `previous_response_not_found` 等错误；契约套件要能覆盖第二种 transport。chatgpt 后端是否接受非 Codex 的 originator 走 WebSocket，**未核实**。
- **ADR**：新 ADR，作为 ADR 0026 决定 4 的扩展。`store:false` 保持不变。
- **验证**：同一任务记录每步请求的字节数和首 token 中位延迟；变异测试：改了 tools 仍走增量，测试应该变红。

### P8 小模型分流（`explore` 子 agent 优先）
- **做什么**：`[subagent] explore_model = "<provider>/<model>"`，可选，默认继承父模型。如果再给 `subagent` 工具加 `model` 参数，就要按 ADR 0043 的单 session 设计来做。
- **为什么**：`explore` 只读、只搜索，用 haiku 或 flash 档就够。Claude Code（`model: haiku`）、omp（smol）、opencode（small）都这么做。具体能省多少取决于目录里的价差，这里不编数字。
- **成本**：中。
- **ADR**：ADR 0043（子 agent）和 ADR 0018（压缩模型目前只能是 `current`）都要开新修订。
- **验证**：同一组探索任务对比成本和成功率，成功率不能下降。

### P9 子 agent 和父会话共享 tools 前缀
- **做什么**：`general` 子 agent 拿到和父会话**完全相同的工具表**，`subagent` 工具保留但在深度 1 时拒绝调用（omp `/tan` 的修法）。OpenAI 路由上用父会话的缓存键（与 P2 一起做）。
- **为什么**：Anthropic 的缓存顺序是 tools → system → messages。工具表一致，子 agent 的第一次请求至少能读到父会话已缓存的 tools 段（**推断**；system 末尾有易变的 `<session>` 段，所以 system 段多半仍会 miss）。
- **成本**：小到中。
- **ADR**：ADR 0043 修订。
- **验证**：子进程第一次调用的 `cache_read_input_tokens` 应该大于 0。

### P10 小修补
- 缓存还热时，`/model` 和 `/thinking` 先提示代价（Claude Code 的做法）。碰 ADR 0027 和 0035 的 UI 约定。
- 读 `x-should-retry` 和 `retry-after-ms`。碰 ADR 0017，只是增加信号来源，不改策略。
- chatgpt 路由的 idle 超时提到 300 s，或者设 `reasoning_summary:"auto"`，让长推理期间有字节流出来（这条要先实测，确认是否真有误判超时）。
- Anthropic 描述符的 thinking 预算，`provider.rs:876-877` 自己标着「a proposal … marked for the owner」，需要 定。

### 建议的顺序
P4（先能量）→ P1 → P2 → P3（三项都小，都能用 P4 的数字验证）→ P5 → P8 和 P9 → P6 → P7。P7 收益可能最大，但成本最高，应该在 P4 的数据证明上传和 prefill 确实是瓶颈之后再做。
