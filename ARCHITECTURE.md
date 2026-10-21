The ADRs in `docs/adr/` are the authority where this document differs.

# Minimal Headless Rust Agent Runtime

**架构研究与实施设计 · 2026-10-12（Toronto）**

## 1. 目标与范围

做一个可嵌入 Rust 应用的 Agent Runtime：把输入交给模型，按模型请求执行工具，将结果交回模型，并在完成、取消或错误时结束本次运行。

Core 以宿主管理的内存对话 `AgentSession` 为单位持有 transcript 与工具会话；每个用户轮次是一个 `ActiveRun`，提供 async `step()` 与 `run()`：`step()` 执行一次模型响应及其对应的工具调用；`run()` 循环调用同一份内部 `step_once()` 逻辑，直到本轮终态。轮次运行中支持 steering（宿主插入用户消息）、在固定点由 Core await 的 Hooks，以及对声明为可共享的工具并行执行。CLI、Desktop、Server 都是可选客户端。Core 不内置 Plan、Debug、Implementation、Workflow、Multi-agent、Session Persistence 或 Compaction。

本稿将**源码已验证事实**与**设计建议**分开。上游 `main`、`latest` 会变化；实现前应记录具体 commit、tag 和依赖版本。

## 2. 参考实现审查

### Pi

**源码已验证事实**

- `runAgentLoop` 与 `runAgentLoopContinue` 共用 `runLoop`。内层循环处理工具结果和 steering messages，外层循环检查 follow-up messages；loop 发出 model、tool、turn、agent 生命周期事件。[`agent-loop.ts`](https://github.com/earendil-works/pi/blob/main/packages/agent/src/agent-loop.ts)
- 当模型因长度限制停止且响应包含 tool calls 时，Pi 不执行这些可能不完整的调用，而是将失败结果回填；普通工具异常也成为 tool error result。它支持顺序和并行工具执行。[`agent-loop.ts`](https://github.com/earendil-works/pi/blob/main/packages/agent/src/agent-loop.ts)
- `Agent` 独立暴露 `steer()` 与 `followUp()` 队列；运行中再次提交 prompt 会被拒绝。[`agent.ts`](https://github.com/earendil-works/pi/blob/main/packages/agent/src/agent.ts)

**设计建议**：保留简单的“模型请求工具、Runtime 执行、结果回填”控制流，以及对终止和工具失败的明确区分。借鉴 Pi 的 steering、hooks 和并行工具这三项能力，但按本 Runtime 的终态与取消语义重新定义（§4 Steering / Hooks / 并行工具执行）：steering 与 follow-up 合并为一个队列，只在安全点注入；hooks 有固定调用点和失败即拒绝（fail closed）的错误规则；并行只用于宿主声明为可共享的工具，结果仍按调用顺序写入 transcript。不复制 coding-agent session 层。

### Rig

**源码已验证事实**

- Rig 0.39 用 Sans-I/O `AgentRun` 合并原先 streaming 和 non-streaming 两套 prompt loop。当前 0.44 API 文档仍描述 `AgentRun` 状态机及由 AgentRunner 驱动的执行方式。[0.39 发布说明](https://github.com/0xPlaygrounds/rig/discussions/1924) · [AgentRun API](https://docs.rs/rig/latest/rig/agent/run/index.html)
- `AgentRun` 将下一项工作表示为 `CallModel`、`CallTools` 或 `Done`；driver 做 I/O 后把结果交回。状态机检查调用顺序、待处理工具调用和 turn budget。[Durable runs](https://rig.rs/docs/concepts/runs)
- 状态机不持有 model、工具注册表、memory 或 hooks。其序列化包含对话和 provider 响应数据，文档提醒内容敏感、会随运行增长，格式也不承诺跨版本兼容。[AgentRun API](https://docs.rs/rig/latest/rig/agent/run/index.html)

**设计建议**：Rig 证明了 Sans-I/O 的可测试性，但它不是本项目的必选架构。当前 MVP 选择更直接的 async loop + 注入的 Model/ToolSession；不为了模式本身增加 Action/ActionResult/State Machine/Executor 公共抽象。若以后需要确定性重放或在任意工具调用间持久化，再单独评估 Sans-I/O。

### pi-rust（`bigfish1913/pi-rust`）

**源码已验证事实**

- 项目拆分 Provider/message、agent loop、工具、harness、TUI/CLI 和插件 crate；提供 faux provider 与内存执行环境用于离线测试。项目文档介绍了 JSONL session 和崩溃恢复。[README](https://github.com/bigfish1913/pi-rust) · [项目结构指南](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent/project-structure.md)
- `rpi-agent` 文档描述了 provider-agnostic loop、`AgentTool`、事件、队列和取消，并记录并行工具结果次序、截断调用不执行、迟到更新抑制等实现不变量。[crate 文档](https://docs.rs/rpi-agent/latest/rpi_agent/) · [`agent_loop` 文档](https://docs.rs/rpi-agent/latest/rpi_agent/agent_loop/index.html)
- pi-rust 的插件是带版本协商的 Rust `cdylib`/C ABI，工具生命周期为 `execute → poll → cancel → destroy`；这不是独立进程 IPC。[项目结构指南](https://github.com/bigfish1913/pi-rust/blob/main/docs/agent/project-structure.md)

**设计建议**：借鉴 Provider 解耦、离线测试和结果顺序约束；不照搬多 crate 拆分或 cdylib。扩展使用独立 MCP Server 子进程。

## 3. Core Architecture Design

```text
CLI / Desktop / Server / 自定义客户端
                   │ 调用 Rust API / 自有传输层
                   ▼
┌─────────────────────────────────────────────┐
│ agent-core                                   │
│ Agent + AgentSession + ActiveRun              │
│ async step_once() + run()                    │
│ Messages / Model / ToolSession / Events      │
└──────────────┬──────────────────┬───────────┘
               │                  │
        注入的 Model        AgentSession 持有 ToolSession
                                  │
                          agent-extensions
                          MCP stdio 子进程
```

### 模块职责与依赖

1. `agent-core` 定义消息、停止原因、Model 与 ToolProvider/ToolSession 接口、JSON Schema 参数验证、运行事件、取消和 async loop。它不依赖 CLI、UI、HTTP Server、数据库或具体 Provider。
2. `agent-extensions` 实现 MCP stdio client、Native 子进程生命周期，以及 MCP tools 到 Core ToolSession 的适配。依赖方向为 `agent-extensions → agent-core`。
3. Model adapter 可由宿主实现。首个正式 Provider（OpenAI 兼容 Chat Completions）是独立 crate `agent-provider-openai-compatible`，见 §6。
4. 客户端通过依赖/嵌入 `agent-core` 或自行提供 RPC/HTTP 调用面；Core 不负责网络 listener。

Core 的工具生命周期接口建议保持为两个小 trait，并用 associated type 静态分发，不使用 `Box<dyn ToolSession>`：`ToolProvider { type Session: ToolSession; async fn open_session(context) -> Result<Self::Session> }`；`ToolSession` 提供同步 `tool_specs()`、异步 `execute(call, cancellation)` 和 `close().await`。Agent 持有 provider/factory；AgentSession 持有具体 Session 类型；ActiveRun 借用 AgentSession。MCP 子进程的 supervisor 与 wait handle 由 ToolSession 所有，避免把每 Run 进程错误地挂在共享 provider 上，也避免 object-safety/`async_trait` 依赖。因为支持并行工具，`ToolSession::execute` 取 `&self` 并要求 `Sync`，同一会话内可有多个调用同时进行。

`Hooks` 同样作为 Agent 的泛型参数静态分发（默认 `()`，全部 no-op），见 §4 Hooks。trait 方法用返回 `impl Future + Send` 的形式，保证 future 可在 Tokio 多线程运行时中跨线程。

### Core 数据与边界

- `Agent`: 依赖注入的 Model、ToolProvider（会话工厂）与 Hooks；不隐式读取全局环境变量或访问终端。
- `AgentSession`: 宿主管理的一段内存对话，持有 transcript 与已打开的 `ToolSession`。它不是磁盘 Session Persistence；MCP 子进程和有状态工具连接在这段对话的多个用户轮次间保持存活。
- `ActiveRun`: 一次用户轮次，借用所属 `AgentSession`，持有本轮模型调用计数、终态、取消句柄和 steering 队列。每个 session 中轮次顺序执行；不同 session 彼此隔离。
- `Message`: 标准 role/content/tool call/result；Assistant message 另有可选 `provider_payload`，建议用 `Box<serde_json::value::RawValue>` 保存不透明 JSON 片段。Core 不解析、不重序列化、不改写；adapter 在下一轮请求时取回，以保留签名 reasoning block 等协议内容。开启 `serde_json/raw_value` feature，并用 round-trip 测试验证原始片段保留。
- `ModelResponse`: 标准 assistant message、工具调用及 `StopReason`。`StopReason` 至少包含 `EndTurn`、`ToolUse`、`MaxTokens`、`Refusal`、`ContentFiltered`、`PauseTurn` 和 `Other(String)`。
- `ToolSpec`: model-facing name、description、input schema，以及 `concurrency: ToolConcurrency`（`Exclusive` 默认 / `Shared`），用于并行调度。
- `ToolCall` / `ToolResult`: 调用 ID、model-facing name、arguments、`is_error`、可选结构化结果，以及不丢信息的 `ContentPart` 列表。`ContentPart` 至少覆盖 text、image、audio、resource link、embedded resource，并保留 annotations/meta；未知扩展块作为 opaque raw JSON 保留。Model 的 user/assistant/tool 消息内容也必须能携带多模态 part，由 Provider adapter 决定映射方式。
- `AgentEvent`: lifecycle、模型增量、tool start/progress/end（并行时按完成顺序发出，带调用序号）、`SteeringApplied`、`HookFailed` 和唯一的 `RunFinished`。

**不进入 Core**：文件系统、shell、API key 查找、UI、持久化 session、compaction、自动 retry policy、任务 workflow、角色/模式和 agent delegation。

Multi-agent 委派建议移到文末附录；MVP 不增加 Core API。

## 4. Agent Loop Specification

### API 形态：async loop，不公开 Sans-I/O effects

`Agent::open_session_with_history(history)` 先验证 history，再异步打开宿主管理的对话 session，并通过 `ToolProvider::open_session()` 获取 ToolSession。MCP 子进程在这里启动并执行一次 `tools/list`，在该 AgentSession 存续期间供多轮对话复用。初始化任一 server 失败时，整个打开操作失败，已启动的其他 server 先关闭/终止并回收，不返回半初始化 session。`AgentSession::start_turn(input)` 创建本轮 `ActiveRun`；一轮终止后可以在同一 AgentSession 上启动下一轮，history、MCP 进程状态和工具别名都保留。这个内存会话不提供崩溃恢复或跨进程持久化。`ActiveRun::step()` 执行一次模型响应及其对应工具调用；`run()` 循环调用同一内部 `step_once()` 直到本轮终态。

```rust
let mut chat = agent.open_session_with_history(history).await?;
let mut run = chat.start_turn(user_input)?;
let cancellation = run.cancellation_handle();

// 一次 LLM 响应及其工具调用；可以按应用自己的节奏重复调用。
let first = run.step(&events).await?;

// 便捷入口：等价于持续调用同一个 step_once() 直到终态。
let outcome = run.run(&events).await?;
drop(run); // turn 已结束，chat 仍保有 transcript 和 MCP 工具进程

// owned 便捷入口：内部 start_turn + run，便于在 tokio::spawn 的任务中持有 chat。
let outcome = chat.run_turn(next_user_input, TurnOptions::default(), &events).await?;

// 正常退出必须显式关闭；仅 drop 会强制 kill 扩展进程树。
chat.close().await?;
```

`ActiveRun<'a>` 借用 `AgentSession`，由借用检查器保证同一会话一次只有一个轮次。Server 等需要把会话移入后台任务的场景，可直接持有 `AgentSession` 并调用 `run_turn()`，避免在任务边界传递带生命周期的 `ActiveRun`。

在 `step()` 中，Core 调用当前 AgentSession 持有的 ToolSession。Host 需要权限检查、人工确认、改写参数、结果脱敏或审计时，使用 §4 Hooks；在 ToolProvider/ToolSession 外包一层仍然可行，但只适合工具层的适配。Custom Loop 可控制何时调用 `step()`。AgentSession 的轮次严格顺序执行；同一轮内的 `Shared` 工具可以并行，因此单个会话内的 MCP server 需要支持请求多路复用（§5）。多个 AgentSession 各自拥有进程。

`step()`/`run()` 的 future 在首次 poll 后不可安全 drop。实现必须以 Drop guard 保护进行中的状态转换；直接丢弃任一 in-flight future 都 poison 当前 Run/AgentSession。对进程型 ToolSession，正在执行的请求 future 自身也必须持有同步 abort guard：future 被 drop 时调用 supervisor 的同步进程树终止句柄，不依赖 Tokio task 执行清理。只有显式 `CancellationHandle::cancel()` 并继续 await，才是受支持的中途取消方式。

轮次限时不能用 `tokio::time::timeout()` 包住 `step()`/`run()`。`start_turn_with(input, TurnOptions { deadline, .. })`（`start_turn(input)` 等价于默认选项）可设置可选的整轮 deadline：到期时 Core 内部触发同一个 cancellation，走正常取消收敛，并以 `Cancelled { reason: DeadlineExceeded }` 结束。宿主自定义限时时，用定时任务调用 `CancellationHandle::cancel()`，然后继续 await 原 future：

```rust
let cancel = run.cancellation_handle();
let timer = tokio::spawn(async move {
    tokio::time::sleep(limit).await;
    cancel.cancel();
});
let outcome = run.run(&events).await; // 继续 await，直到取消收敛
timer.abort();
```

### 一次 step 的行为

1. 若 cancellation 已触发，结束为 `Cancelled`，不发起新的模型/工具调用。
2. 注入 steering 队列中已有的消息（安全点，见下文 Steering）。
3. 检查预算，构造请求并执行 `before_model_call` hook，然后请求 Model。Model 可经事件 sink 发送文本/工具参数增量；最终返回一个完整 `ModelResponse`，再执行 `after_model_response` hook。
4. Model 或 model hook 错误会被 `step()` 捕获为本次 Run 的 `Failed` 终态，返回 `RunError`，发且只发一次 `RunFinished(Failed)`。不得用 `?` 绕过终态收敛。
5. 若有工具调用且 stop reason 不是 `MaxTokens` 或 `ContentFiltered`，按响应原始顺序逐个 preflight：检查名称、参数 JSON 和 JSON Schema；无效调用合成 `is_error:true` 结果，不执行。有效调用依次经过 `before_tool_call` hook，再按并行规则调度给 ToolSession 执行；每个结果经过 `after_tool_call` hook。
6. 结果按**调用顺序以前缀方式**写入 transcript：一个结果在它和它之前的所有结果都已完成时写入。串行时等价于“每完成一个立即追加”。若之后取消，已写入的结果保留；对被取消、进行中和未启动的调用追加明确的 cancelled/not-executed 错误结果，保证 transcript 中每个 tool call ID 都有对应结果。
7. 如果没有工具调用且 stop reason 表示模型完成：steering 队列为空则 Run 结束；队列非空则不结束，返回 `Continue`，下一次 step 注入这些消息。若发生工具调用或截断工具调用被拒后有机会重试，返回 `Continue`。

### Stop reason 与工具调用预算

| 模型响应 | 行为 |
|---|---|
| `EndTurn` 且无工具调用 | `Completed` |
| `ToolUse` 且无工具调用 | 不判协议错误；按保守 fallback 返回 `Incomplete` |
| 除 `MaxTokens`/`ContentFiltered` 外任意 stop reason 且含 tool calls | 逐个验证/执行/回填；返回 `Continue`。工具调用是否存在是分发依据，stop reason 只作提示，不因 stop reason 与工具调用不一致而判模型协议错误 |
| `MaxTokens` 且无工具调用 | `Incomplete` 终态，保留 assistant 部分回复；不自动续写 |
| `MaxTokens` 且带 tool calls | 所有调用合成错误结果，零工具执行；预算允许时返回 `Continue` 让模型修正 |
| `Refusal` | `Refused` 终态，保留拒绝消息 |
| `ContentFiltered` 且无工具调用 | `Incomplete` 终态，保留 provider 给出的可用内容/原因 |
| `ContentFiltered` 且带 tool calls | 被过滤的响应与截断同等对待：所有调用合成 `content_filtered` 错误结果，零工具执行，以 `Incomplete` 终态结束，不自动让模型重试 |
| `PauseTurn` | `Continue`；不执行工具，将 assistant response 原样保留在 transcript，由下一次模型请求继续该轮并计入同一调用预算。Anthropic 文档要求发送原 assistant 内容继续 server-side loop。([stop reasons](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons)) |
| 未识别的 stop reason | 保留为 `Other(String)`，按终态 `Incomplete` 处理，不猜测并自动继续 |

除 `MaxTokens` 与 `ContentFiltered` 外，Core 以响应中是否实际存在 tool call 决定是否执行；adapter 保留 Provider 原生 stop reason，Core 不要求 adapter 将每个 stop reason/调用组合先归一为一致状态。无 tool call 时再按 stop reason 决定 `Completed`、`Refused`、`Incomplete` 或 `Continue`。`ToolUse` 但没有调用不属于协议错误；按其无调用 stop reason 的保守映射结束或继续。

若 `max_model_calls = N`，每次请求前检查预算；第 N 次是最后一次允许的请求。它返回 final answer 时正常完成；若返回 tool calls，因为没有剩余模型调用预算，**不执行任何工具**，为所有调用 ID 合成 `budget_exhausted` 结果并以 `BudgetExceeded` 结束。这样不会在没有后续模型读取结果的情况下产生副作用。默认上限建议 32 次，可由调用者配置；不限次不是默认值。

Adapter 负责保留 Provider 原生结束原因。拒绝也可由 content 标记表达；Core 不因拒绝文本误判工具调用。`MaxTokens` 截断与 `ContentFiltered` 过滤优先于工具分发：这两类响应里的调用均不执行。

`ToolCall` 只表示**需要宿主在本地执行**的客户端工具。Provider 在服务端执行的工具（例如 Anthropic 的 `server_tool_use` 与 `web_search_tool_result` 块）由 adapter 保留在 assistant 的 `provider_payload` 或 opaque `ContentPart` 中，不得暴露为 Core `ToolCall`，否则 Core 会尝试在本地分发它们。`PauseTurn` 响应通常只含此类服务端块。

### Steering（运行中插话）

`ActiveRun::steering_handle()` 返回可克隆、`Send + Sync` 的 `SteeringHandle`，它不借用 `ActiveRun`，宿主可以把它交给 UI 任务，在本轮运行中随时调用 `steer(message)`。消息进入本轮的 FIFO 队列，Core 只在**安全点**把它们作为 user 消息追加到 transcript：

| 安全点 | 时机 | 说明 |
|---|---|---|
| A | 一批工具结果全部写入之后、下一次模型请求之前 | 最常见的插话位置 |
| B | 模型给出最终回答（本应 `Completed`）且队列非空 | 不结束本轮，返回 `Continue`，下一次 step 注入后继续请求模型。这覆盖 Pi 的 follow-up 用途，不再单设 follow-up 队列 |
| C | 本轮第一次模型请求之前 | `start_turn` 之后、首个 `step()` 之前到达的消息 |

Steering 不打断进行中的模型请求或流式输出。需要立即打断时，宿主先 `cancel()`，再用 `unapplied_steering` 开新一轮。

一批工具执行中收到 steering 时，行为由 `TurnOptions::steer_mode` 决定：

- `AfterBatch`（默认）：当前批次照常跑完，在安全点 A 注入。
- `SkipRemaining`：已开始执行的调用照常完成；尚未启动的调用不执行，合成 `skipped_by_steering` 结果（写明“用户插入了新消息，此调用未执行”），然后注入。

其他规则：

- 注入本身不消耗预算，但它引发的下一次模型请求照常计入 `max_model_calls`。
- 任何终态下，队列里还没注入的消息随 `RunOutcome::unapplied_steering` 交还宿主，绝不静默丢弃。`Poisoned` 时没有可返回的 outcome，这些消息改由 `history_for_resume()` 的返回值一并带出（`ResumeState { history, unapplied_steering }`）。终态后再调用 `steer()` 返回 `Err(SteerError::RunFinished(message))`，把消息原样交还调用者。
- 注入时发出 `SteeringApplied { count }` 事件。steering 消息进入 transcript 后与普通 user 消息相同，`history_for_resume()` 也包含它们。
- 工具结果后紧跟 user 消息、或多条 user 消息相连时，如何映射到 Provider 格式由 adapter 负责（例如 Anthropic 要求 `tool_result` 块位于 user 消息开头，文本放在其后）。

### Hooks

Hooks 是 Core 在固定调用点 **await** 的策略接口，可以改变控制流；`EventSink` 只观察、不能改变控制流，两者分开。

```rust
pub trait Hooks: Send + Sync {
    fn before_model_call(&self, cx: &HookContext, req: &mut ModelRequest)
        -> impl Future<Output = Result<(), HookError>> + Send { async { Ok(()) } }
    fn after_model_response(&self, cx: &HookContext, resp: &ModelResponse)
        -> impl Future<Output = Result<(), HookError>> + Send { async { Ok(()) } }
    fn before_tool_call(&self, cx: &HookContext, call: &ToolCall)
        -> impl Future<Output = Result<ToolDecision, HookError>> + Send { async { Ok(ToolDecision::Allow) } }
    fn after_tool_call(&self, cx: &HookContext, call: &ToolCall, result: &mut ToolResult)
        -> impl Future<Output = Result<(), HookError>> + Send { async { Ok(()) } }
    fn on_turn_end(&self, cx: &HookContext, outcome: &RunOutcome) {}
}

pub enum ToolDecision {
    Allow,
    Deny { reason: String },        // 不执行，合成 tool error
    Rewrite { arguments: Value },   // 改参数后重新 preflight，再执行
    Respond(ToolResult),            // 不执行，直接用给定结果
}
```

| Hook | 调用时机 | 能做什么 | 出错时（fail closed） |
|---|---|---|---|
| `before_model_call` | 每次模型请求前，预算检查之后 | 修改**本次**请求：追加上下文、调整参数。不改 transcript | Run → `Failed(Hook)` |
| `after_model_response` | 完整响应到达、写入 transcript 之前 | 只读检查；返回错误即中止 | Run → `Failed(Hook)` |
| `before_tool_call` | preflight 通过后、调度执行前 | Allow / Deny / Rewrite / Respond | 视为 Deny，结果写明 hook 出错 |
| `after_tool_call` | 结果写入 transcript 之前 | 修改结果：脱敏、截断、补充说明；对 `call_id` 的修改会被丢弃 | 结果替换为 tool error，不让未处理的原结果流入模型 |
| `on_turn_end` | 终态收敛时，`RunFinished` 之后 | 同步观察，例如审计落盘的触发 | 无（不可失败） |

规则：

- **组合。** Agent 只有一个 `H: Hooks` 类型参数，默认 `()`。`(A, B)` 实现 `Hooks`，按顺序执行：`before_tool_call` 中第一个非 `Allow` 的决定短路后续 hook，`Rewrite` 的新参数传给下一个 hook；`after_tool_call` 依次修改同一个结果。
- **Rewrite 重新校验。** 改写后的参数重新走 preflight，但不再次调用 hooks，避免循环；校验失败则合成 tool error。
- **人工审批放在 `before_tool_call`。** 审批等待与其他 await 一样和取消竞速，并计入轮次 deadline。并行批次中，`before_tool_call` 在调度前**按调用顺序依次**执行，审批提示按模型给出的顺序出现；`after_tool_call` 在各调用完成时调用，可能并发，所以 `Hooks` 要求 `Send + Sync`。
- **不重入。** Hook 不能对同一 AgentSession 调用 `step()`/`start_turn()`（借用规则天然禁止）；可以调用 `SteeringHandle::steer()`。
- **事件。** hook 出错时发 `HookFailed { hook, message }` 事件，再按上表处理。
- `before_model_call` 修改的只是发给 Provider 的请求副本。改动 transcript 前部会让 prompt cache 失效，这由宿主自行权衡。

### 并行工具执行

并行只用于宿主声明为可共享的工具，有副作用的工具仍按模型给出的顺序执行，transcript 的结果顺序与串行时完全一致。

- **声明。** `ToolSpec::concurrency` 默认 `Exclusive`。本地工具自己声明；MCP 工具默认 `Exclusive`，宿主可在扩展配置中逐个标为 `Shared`。MCP 的 `readOnlyHint` 注解只是 server 自己的提示，默认不信任；只有宿主对该 server 开启 `trust_read_only_hint` 时才据此标为 `Shared`。
- **调度。** hooks 之后，按原始顺序扫描调用：连续的 `Shared` 调用组成一个并行组，并发数不超过 `TurnOptions::max_parallel_tools`（默认 4，设为 1 即完全串行）；`Exclusive` 调用是屏障，要等它之前的所有调用完成后单独执行，执行完才启动之后的调用。
- **结果顺序。** transcript 按调用顺序以前缀方式写入（见“一次 step 的行为”第 6 条）；`ToolStart`/`ToolEnd` 事件按实际发生顺序发出并带调用序号，宿主 UI 可以实时显示进度。transcript 内容与完成时序无关，便于测试和复现。
- **超时。** 每个调用有独立 deadline，一个调用超时不影响同组其他调用的计时。
- **取消。** 取消信号同时发给所有进行中的调用；它们各自得到取消结果，未启动的得到 not-executed 结果。`step()` 在所有进行中的工具 future 结束、相关进程回收之后才返回。
- **Poison。** future 被 drop 时，Drop guard 对所有进行中的调用同步执行 abort，并补齐 `interrupted` 结果。
- **与 steering 的关系。** `SkipRemaining` 模式下，已启动的调用（包括并行组中已经开始的）照常完成，未启动的被跳过。
- **预算。** 并行不改变模型调用预算的计算。

### 取消、错误和 Run 终态

- Model 网络/协议失败：`RunError::Model`，Run 进入 `Failed`，发唯一终态事件，不自动 retry。
- Tool 执行失败、未知 tool、参数/schema 错误：生成模型可读的 tool error result；Run 继续（若预算允许）。`before_tool_call` hook 的 Deny 与 ToolSession wrapper 的拒绝同样转为 tool error result。
- Hook 错误：按 §4 Hooks 表格 fail closed；model 相关 hook 出错时 Run 进入 `Failed(Hook)`。
- `CancellationHandle::cancel()` 可从其他任务同步触发取消。Model 与 ToolSession 接口接收同一取消信号；模型调用在取消时直接丢弃其 future。工具调用则先让工具看到取消信号，并继续等它自行收尾，最多 `TurnOptions::tool_cancel_grace`（默认 5 秒），超时才丢弃其 future；不论工具返回什么，该调用结果都是 `cancelled`。这样有子进程的扩展可以在 `step()` 返回前发出取消通知、终止并 wait/reap 子进程树；对远端 API，取消不保证撤销服务端已经接受的请求或副作用。
- `step()` 或 `run()` future 在首次 poll 后若被调用方直接 drop（例如 `timeout()` / `select!` 丢弃 losing future），属于**不安全中断**：同步 Drop guard 将 `ActiveRun` 标记为 `Poisoned`、发出唯一 `RunFinished(Poisoned)`，并将所属 AgentSession 标记不可继续；若工具子进程可能处于调用中，ToolSession 请求 future 的 Drop guard 同步终止该 session 的进程组。后续 `step()`/新 turn 都返回稳定的 `Poisoned` 错误。Poison 时 Drop guard 同步为所有尚无结果的 tool call 追加 `interrupted` 错误结果（与取消路径一致），保证 transcript 不含未配对调用；宿主可通过 `AgentSession::history_for_resume()` 取出该 transcript，再用 `open_session_with_history` 打开新会话继续对话。被中断的调用可能已产生副作用，`interrupted` 结果须如实说明“执行状态未知”。future 在首次 poll 前被 drop 没有执行、无需 poison。调用方要取消正在运行的操作，必须通过 `CancellationHandle::cancel()` 发信号并继续 await `step()`/`run()` 收敛；不要用会 drop future 的 timeout 包住它。
- `step()` 若正在执行一批工具（串行或并行），取消后保留已写入的结果，向所有进行中的调用发出取消，为进行中/未启动调用补齐取消结果，然后以 `Cancelled` 终态结束；未注入的 steering 消息随 `unapplied_steering` 返回。
- `RunFinished` 由 `ActiveRun` 的终态收敛路径发出，只能发生一次；`Completed`、`Incomplete`、`Refused`、`BudgetExceeded`、`Cancelled`、`Failed` 和 `Poisoned` 均覆盖。终态后不能继续调用 `step()`。
- 事件由 Core 的执行路径同步送入注入的 `EventSink`；sink 必须短小、非阻塞，不能反向 await 当前 Run。Core 保证调用顺序与终态事件恰好一次；宿主负责将事件转发到 UI/网络队列。宿主有界队列可合并/丢弃 progress，但不得吞掉终态。

取消不能撤销已发生的外部副作用，也不提供 exactly-once。MCP cancellation 是协作式信号，服务器可能已经完成操作；显式 cancel 路径会在返回前完成 grace/kill/wait。`AgentSession` drop 时同步向其子进程组发送 kill（Windows 对 Job Object 调用 `TerminateJobObject`）；wait/reap 交给不依赖 Tokio runtime 的进程 supervisor 线程，避免 runtime 正在关闭时后台 Tokio task 无法运行。step future 被直接 drop 时也走同步 kill 路径。

## 5. Native Extension：采用 MCP stdio

### 选择与版本

**ADR 建议：不设计自定义 JSON-RPC 协议；扩展进程作为 MCP stdio Server，Host 使用官方 Rust SDK `rmcp`。** MCP stdio 已定义 JSON-RPC framing、工具发现/调用、错误、progress、取消和子进程传输；另造一套协议会损失现成服务器互通性。[MCP stdio transport](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/basic/transports/index.mdx) · [官方 `rmcp` SDK](https://github.com/modelcontextprotocol/rust-sdk)

源码/规范核对结果：截至本文日期，MCP 最新正式规范是 **2026-07-28**。它移除了旧 `initialize` / `notifications/initialized` 会话握手，增加 `server/discover`；请求携带 protocol version/client capabilities。stdio 仍是子进程 stdin/stdout 上的换行 JSON-RPC。官方 `rmcp` 当前文档版本 **3.5.1** 声明支持 `2025-11-25` 和 `2026-07-28`；已核实（`agent-extensions` 以 `=3.5.1` 锁定）：

- rmcp 的 `serve()` 默认只走旧的 `initialize` 握手；必须显式使用 `ClientLifecycleMode::Auto`，才会先发 `server/discover`，收到非现代错误码时回退到 `initialize`。对 `server/discover` 完全不回复的旧 server 要等 10 秒才回退。
- 丢弃 `call_tool_once` 的 future 不会发送 `notifications/cancelled`；要通知 server，需用 `send_cancellable_request` 并在取消时调用 `RequestHandle::cancel`。
- rmcp 发出的每个请求都在 `_meta` 自动携带递增的 `progressToken`，`send_cancellable_request` 返回的 `RequestHandle` 上即该值；server 的 `notifications/progress` 经 `ClientHandler::on_progress` 到达。Host 在 handler 里按 token 把通知路由给对应调用：每个调用一个有界队列（上限 64 条，满了丢最旧的、保留最新值，因为进度是会被新值取代的显示值），推入同步、取出异步，所以 transport 循环永不阻塞，恶意或失控的 server 也撑不爆宿主内存。请求写出后、调用登记路由前到达的通知进入有界孤儿缓冲（256 条，满则丢最旧），登记时在同一把锁下按序回放进调用通道，登记竞态按构造闭合。这个窗口只有几微秒，端到端无法复现（去掉回放后，在 16 个 CPU 占用线程、24 路并发下 2880 次调用也没有丢失），所以由 `ProgressRouter` 的单元测试确定性地覆盖，而不是由集成测试。尾部进度是尽力而为：rmcp 把每条通知 spawn 成独立 task 路由、而响应在服务循环里直接投递给等待者，回复前发出的最后几条可能在响应被取走后才路由，先排空再取响应只能减少、不保证消除尾部丢失。同一 token 的 `progress` 规范要求单调，通知乱序到达时发 `AgentEvent::ToolProgress` 前丢弃不大于已发出最大值的通知，保证事件序列单调。调用以任何方式结束（响应、取消、超时、drop）都移除登记；迟到 token 的通知留在孤儿缓冲里直到被挤掉。select 循环里的 deadline 必须在循环外 pin，否则每条进度都会重置计时。
- rmcp 的 `TokioChildProcess` 在 drop 时用 `tokio::spawn` 杀进程，且不建进程组；因此 Host 自行 spawn 子进程（独立进程组、同步 `killpg`、专用线程 wait/reap），只把 stdin/stdout 交给 rmcp 的 async-rw 传输。
- rmcp 默认对 sampling 返回 method-not-found、roots 返回空列表、elicitation 返回 decline；sampling 与 roots 自 2026-07-28 起被 SEP-2577 标为弃用，但 2025-11-25 server 仍可能发来，所以 Host 覆盖为一律 method-not-found。
- 集成测试使用一个手写的 fixture server（不依赖 rmcp），分别以 2026-07-28 和 2025-11-25 生命周期运行，并断言收到的方法序列：前者为 `server/discover` → `tools/list`，后者为 `server/discover`（被拒）→ `initialize` → `notifications/initialized` → `tools/list`。[MCP releases](https://github.com/modelcontextprotocol/modelcontextprotocol/releases) · [2026-07-28 changelog](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/docs/specification/2026-07-28/changelog.mdx) · [`rmcp` compatibility/transports](https://github.com/modelcontextprotocol/rust-sdk)

MVP 使用标准 `tools/list` 与 `tools/call`。工具执行错误放在 MCP CallToolResult 的 `isError: true` 中；JSON-RPC error 留给协议/分发层错误。Host 把 MCP 的原始工具名映射为符合所选 Model Provider 命名约束的唯一 model-facing name；模型名与 MCP 真名通过本地注册表映射，**不把 `extension_id/tool_name` 直接发给 Provider**。

### AgentSession 内的进程作用域与工具别名

**MVP 每个 AgentSession（即一段宿主管理的对话）为每个配置的 Extension 启动一份子进程**，不是每个 ActiveRun 启动。进程内状态可跨多轮保留，解决浏览器、数据库连接、REPL 等工具的对话连续性。Session 内轮次顺序执行；同一轮中标为 `Shared` 的工具可能同时调用同一个 server，此时作为独立的 JSON-RPC 请求并发发送，由 `rmcp` 按 request id 配对响应，取消也按单个请求发送 `notifications/cancelled`。`Exclusive` 工具对同一 server 不会与其他调用重叠。不同 AgentSession 的子进程隔离。MCP 2026 的无协议级 session 不妨碍 stdio server 进程本身在存续期间保留应用状态；状态不承诺进程重启后恢复。

每次 AgentSession 初始化时通过 `tools/list` 读取一次工具定义，session 内固定快照；MVP 不处理运行中 tools/list 动态变更。

别名只由稳定配置的 `server_id` 与该工具自身的 MCP 原始名决定，**不依赖同一会话中的其他工具**：

1. 基本形式为 `sanitize(server_id) + "__" + sanitize(tool_name)`，`sanitize` 把 `[a-zA-Z0-9_-]` 以外的字符替换为 `_`。工具名对模型有语义，保持可读。
2. 只有当 sanitize 改变了原名，或结果超过 Provider 长度限制时，才截断可读部分，并追加固定 8 位十六进制后缀（`SHA-256(server_id + "\0" + tool_name)` 的前 8 位）。后缀长度固定，不随冲突变化。
3. 打开 AgentSession 时若两个工具得到相同别名，视为配置错误，打开失败并报告冲突双方；不自动延长散列，因为那会让既有别名随新增工具改变。

因此别名不依赖列表顺序、进程实例或工具集合的增减；历史中既有 tool call/result 名称可跨新 AgentSession 稳定解析。回调时用注册表映射回原始 server/tool 名。`tools/call` 的进度经 progress token 映射为 Core `ToolProgress` 事件。

MCP **子进程崩溃、坏帧、协议错误，以及任何一次超时或取消**，都会杀掉该 AgentSession 中对应 Extension 的整棵进程树，并把这个 ToolSession 标为 `Unavailable`；当前调用得到超时/不可用 tool error，当前轮可以继续。并行时，杀进程会连带终止同一 server 上其他进行中的调用，它们得到 `extension_unavailable` 结果，并写明是因同一扩展中另一调用超时或崩溃而终止。该对话后续调用同一 Extension 都立即返回 unavailable tool error，不在会话中自动重启，也不重放可能已产生副作用的调用。Host 新建 AgentSession 时才启动新进程并重读工具定义；其他 Extension sessions 不受影响。显式整轮取消仍以 `Cancelled` 结束。

不可用 Extension 的工具**继续保留在本会话的工具声明中**，以保持工具列表稳定、不让 Provider 的 prompt cache 失效；对它的调用立即返回 `extension_unavailable` 错误，内容明确写“该扩展在本会话中已不可用，不要再调用这些工具”，让模型改用其他路径。重复调用照常计入模型调用预算。

### Timeout、进程树和环境

- 每个工具请求有可配置的有限 deadline（`call_timeout`）。超时或整轮取消时，Host 发送 `notifications/cancelled`。规范要求 server 对被取消的请求不再回复，所以无法从回复判断它是否已停止；Host 在 `cancel_grace` 内先把通知写出，并用一次 `ping` 确认 server 已读到它，**然后无论 server 是否还能响应，一律终止其子进程树、等待回收，并在本会话内标为不可用**。能响应 `ping` 不代表被取消的工具已经停止：它可能仍在后台运行，若继续复用该 server，被放弃的工作会与下一次调用交错，这对浏览器、数据库、REPL 这类有状态 server 不安全。代价是取消或超时会丢失该 server 的跨轮状态。当前调用分别得到 `extension_timeout` 或 `cancelled` 结果，消息里说明 server 已被停止；整轮取消仍以 `Cancelled` 结束。
- Unix 子进程放入独立 process group/session，终止时杀整个 group；专用 reaper 线程先于子进程创建（OS 拒绝创建线程时根本不启动子进程；子进程启动后经 channel 交接，交接或管道设置失败都由当前线程自己 `kill` 并 `wait`，不留僵尸），它在 leader 退出后（包括 server 自己崩溃）立刻杀掉该 group 的剩余成员。pgid 就是 leader 的 pid，leader 被回收且组已空后可能被复用，所以顺序必须是：先 `waitid(WEXITED|WNOWAIT)` 等 leader 退出但不回收，在它还是僵尸（仍占着 pid 和 pgid）期间 `killpg`，再回收；所有 `killpg` 与回收共用一把锁，回收之后任何路径都不再向该 pgid 发信号。没有 `waitid(WNOWAIT)` 的平台退化为直接回收，留有极短的窗口，仅在 leader 是组内最后一个成员时才有意义。这个竞态无法用测试确定性复现，现有测试只保证行为不退化；Windows 计划使用 Job Object 并设置关闭时终止成员；当前实现暂以 `taskkill /T /F` 按 pid 终止进程树，尚未在 Windows 上测试；它沿父子关系遍历，到达不了已退出 leader 的子进程，所以 server 自己崩溃后的清理在 Windows 上要等 Job Object 方案才完整。单杀直接子进程不能满足无孤儿子进程验收。
- 子进程默认不继承宿主完整环境变量。Host 用 `env_clear()`，仅设置必要的启动环境（例如可执行文件解析所需 `PATH` 和 Windows 系统变量）；其他变量逐项由 Extension 配置 allowlist，API key 等 secret 不因默认继承而泄露。
- 启动失败、坏 MCP 帧或进程崩溃：当前调用转成 `extension_unavailable` / `extension_protocol_error` tool result，ToolSession 按上条规则失效；不重放失败调用。
- `AgentSession::close().await` 正常关闭所有 ToolSession。第一次调用起会话不再接受新轮次；只有底层关闭成功后会话才算已关闭，失败或 close future 被丢弃时可再次调用 `close()` 重试，因此 `ToolSession::close` 必须可重试；关闭成功后再调用直接返回 `Ok(())`。关闭过程：关闭 stdin，等待进程退出；超过 shutdown deadline 或发生关闭错误则 terminate/kill，并 wait/reap 所有进程后返回首个关闭错误。MCP 生命周期不定义通用 `shutdown` 方法；Host 不发明自定义 `shutdown`/`exit` RPC。AgentSession Drop 同步 kill 每个进程组/Job Object；独立 supervisor 线程负责最终 wait/reap，不依赖 Tokio runtime。Drop 是兜底路径而非正常关闭：不给 server 优雅退出的机会，API 文档须写明正常退出前调用 `close().await`。
- 旧版 2025-11-25 MCP 可主动请求 sampling、roots、elicitation。MVP 明确不提供这些能力：rmcp 3.5.1 默认 `create_message` 返回 method-not-found、`list_roots` 返回空列表、`create_elicitation` 返回 decline；这不满足统一拒绝策略。Adapter 应不声明这些能力，并覆盖 handler，对三种旧式请求都快速返回 MCP method-not-found。对 2026 MRTR，使用 rmcp 的单轮 `call_tool_once` 检查结果；若返回 `InputRequired`，MVP 返回 `extension_unsupported_interaction` tool error，不自动满足/重试。禁止用会等待 UI 或宿主回调的默认处理造成挂起。实现时为上述行为加无 hang 测试。[`ClientHandler` defaults](https://docs.rs/rmcp/latest/rmcp/handler/client/trait.ClientHandler.html) · [MRTR behavior](https://github.com/modelcontextprotocol/rust-sdk)

## 6. Rust Workspace 与 Crate 划分

MVP 的 crate：

```text
workspace/
  Cargo.toml
  crates/
    agent-core/          # async loop、类型、Model/ToolProvider/ToolSession、Hooks、steering、并行调度、事件、取消
    agent-extensions/    # rmcp client、AgentSession 级 Native 子进程监督、ToolSession adapter
    agent-provider-openai-compatible/  # OpenAI 兼容 Chat Completions 的流式 Model adapter
    agent-cli/           # 独立的命令行客户端（可选客户端，不是 Runtime 的必需部分）
```

> 实现后的 workspace 比这份 MVP 规划更大（`radian-*` 各层、第二个 provider adapter、`agent-provider-contract` 契约套件与 `radian-memory`）；以根 `README.md` 的 crate 表为准。

依赖方向 `agent-extensions → agent-core`。`agent-core` 的 JSON Schema preflight 使用 validator（候选 `jsonschema` 必须 `default-features = false`，禁用 HTTP/file resolver；MVP 拒绝外部 `$ref`，只验证内嵌或显式内存 registry 中的 schema），避免 Core 产生网络/文件 I/O。当前 `jsonschema` 文档说明默认会解析 HTTP 和文件引用，关闭全部默认 feature 可禁用解析器。([external references](https://docs.rs/jsonschema/latest/jsonschema/#external-references)) Core 是 async API，MVP 可明确选 Tokio；取消句柄和 `select!` 属于 async loop。`agent-extensions` 使用官方 `rmcp` child-process/stdio transport，并在 runtime-independent supervisor 中管理子进程生命周期；稳定 alias 使用小型 SHA-256 依赖（如 `sha2`）。Cargo.lock 锁定的版本必须通过 MCP lifecycle、MRTR 和 unsupported-request 测试。

### OpenAI 兼容 Provider adapter

`agent-provider-openai-compatible` 实现 `agent_core::Model`，POST `{base_url}/chat/completions`，`stream: true`，用 `reqwest`（rustls）。选这个 API 而不是 Anthropic Messages，是因为它覆盖面最广（OpenAI、GLM、OpenRouter、Ollama 等）；代价是设计稿里只对 Anthropic 有意义的几条（带签名的 thinking 块、`PauseTurn`、服务端工具块）在这个 adapter 上得不到真实验证。

- **库不读环境。** 密钥由宿主以 `ApiKey` 传入，`Debug` 与错误信息里都不出现它；有密钥时拒绝向非本机地址发明文 `http`；不跟随重定向（否则密钥会随请求发往别处）。
- **映射：** 文本与工具调用往返无损；图片、音频放到 API 接受的位置；资源链接、嵌入资源、不透明块、工具结果里的图片退化成简短文本占位（有损，已在文档与测试里写明）；工具错误加 `Error: ` 前缀，因为该 API 没有 `is_error`。工具参数不是合法 JSON 时作为字符串保存，由 Core 的参数检查返回给模型修正，原样发回。
- **流式：** 字节级 SSE 解析（多字节字符、`\r\n` 跨块拆分都正确）。只在收到 `finish_reason`（或 `[DONE]`）后交付完整响应，流中断是错误，绝不返回半截答案。`finish_reason` 映射：`stop`→EndTurn，`tool_calls`/`function_call`→ToolUse，`length`→MaxTokens，`content_filter`→ContentFiltered，其余保留为 `Other`；`refusal` 字段映射为 Refusal。流式 reasoning 文本（`reasoning_content`）忽略。
- **方言：** 缺 `index` 的 tool call 片段、缺 `id`、缺 `[DONE]`、只含 usage 的空 choices 块都能处理。部分服务器每个响应都复用 `call_0` 之类的 id，而 Core 要求 id 在整个 transcript 内唯一，所以 adapter 把与已有 id 重复的改名为 `{id}-2`。
- **边界：** 响应文本与工具参数总量有上限（默认 32 MiB），一行 SSE 有上限（4 MiB），连接后服务器沉默超过空闲超时（默认 120 秒）即失败；没有总时长限制。不自动重试，429/5xx 以 `Transport` 结束，其他 4xx 以 `Protocol` 结束，由宿主决定是否重试。
- **测试：** `testing` feature 提供进程内 mock 服务器（分块流、挂住、中途断开、固定状态码，并记录请求），单元测试与集成测试都在本地离线运行。

faux Model 与内存 Tool 可作为 `agent-core` 的 dev support 或 integration test fixtures，不单建 crate。Example 放在 `crates/agent-core/examples/minimal.rs`，命令为 `cargo run -p agent-core --example minimal`。Server 在有实际需求后再加。

## 7. MVP Implementation Plan（依赖顺序）

| 阶段 | 交付 | 完成标准 |
|---|---|---|
| 0. 固定语义与 ADR | async loop；step future poison；stop reason/tool-call 优先级；budget；AgentSession/ToolSession 作用域；steering 安全点；hook 调用点与 fail-closed 规则；并行调度规则；MCP 兼容策略；Drop/kill/reap | 本文边界明确；rmcp 与 schema validator 版本锁定后核对功能 |
| 1. Domain types | Message/ContentPart、opaque provider payload、StopReason、ToolSpec（含 concurrency）、ToolCall/Result、ToolDecision、RunOutcome（含 `unapplied_steering`）、事件、错误 | 多模态/MCP content 不丢失；Provider payload 无损 round-trip |
| 2. 会话接口与 history | Model、ToolProvider、ToolSession、Hooks、AgentSession、EventSink、`open_session_with_history`/`start_turn`、历史一致性验证 | session transcript 与 ToolSession 生命周期清楚；dangling tool call history 被拒绝 |
| 3. async `step()`（串行） | 一次模型响应 + 串行 preflight/hooks/执行/回填；终止、错误、取消、turn deadline、Poisoned 收敛与 `history_for_resume()` | 结果按调用顺序入 transcript；hook 出错 fail closed；drop guard 确定性 poison 并补齐 `interrupted` 结果；终态唯一 |
| 4. Steering | `SteeringHandle`、三个安全点、`AfterBatch`/`SkipRemaining`、`unapplied_steering` | 任何终态下消息不丢；注入只发生在安全点 |
| 5. `run()` 与离线验收 | `run()` 只循环调用同一 `step_once()`；faux model、内存工具 | 手动 step 和 run 的 transcript/outcome/events 一致 |
| 6. 并行工具 | `Shared`/`Exclusive` 调度、`max_parallel_tools`、前缀式写入、并行取消与 poison | 不同完成时序下 transcript 完全一致；`Exclusive` 永不与其他调用重叠 |
| 7. MCP stdio adapter | rmcp client、每 AgentSession 的 tools/list、可读且与工具集无关的稳定 alias、冲突即失败、tools/call、并发请求、progress、cancel、MRTR 拒绝 | 2025-11-25 与 2026-07-28 的交互行为有协议测试；不支持的请求快速返回 error |
| 8. Process supervision | 每 AgentSession 子进程、deadline、环境 allowlist、Unix process group、Windows Job Object、同步 Drop kill 与 runtime-independent reap | timeout/cancel/crash/drop 后进程树无残留；同一 server 状态跨 turn 保持 |
| 9. 首个真实 Provider 与 client sample | 单一 API adapter（OpenAI 兼容 Chat Completions，流式）；独立 `agent-cli` crate | Core API 不引入 UI/HTTP 类型；adapter 用本地 mock server 离线测试，真实 API 冒烟测试需要宿主提供密钥 |
| 10. 基准 | Core、并行加速、MCP round-trip、spawn/cancel 资源报告 | 以实测为依据决定是否优化或共享进程 |

磁盘 Session Persistence、Compaction、工作流扩展和 multi-agent 不属于 MVP 验收范围。未来增加委派扩展前，先做单独 ADR 定义子 Agent 的模型 profile、工具 allowlist、预算、产物传递和取消传播。

## 8. 可执行验收测试

### Core offline tests（`cargo test -p agent-core`）

使用 scripted Model 和内存 ToolSession，不访问网络/API key：

1. `run_returns_final_answer`: 一次 Model 请求，无工具调用，得到 `Completed`。
2. `run_executes_tool_then_continues`: Model → 工具 → Model final；transcript 顺序准确。
3. `tool_error_is_fed_back`: ToolSession error 成为模型可见结果；后续模型能读到 `is_error`。
4. `mixed_valid_invalid_tool_calls_preserve_order`: 同批调用有效/无效混合；无效的不执行，合成结果和执行结果按原始 ordinal 写入。
5. `truncated_tool_calls_are_never_executed`: 截断响应中所有工具都未执行且都有结果。
6. `max_calls_final_answer_is_allowed`: 第 N 次调用返回 final，正常 `Completed`。
7. `max_calls_tool_request_is_not_executed`: 第 N 次调用返回工具请求，零副作用执行，补齐 budget error result，终止为 `BudgetExceeded`。
8. `model_error_finishes_once`: Model 错误进入 Failed、返回 `RunError::Model`，只收到一次 `RunFinished(Failed)`。
9. `cancel_during_model_finishes_once`: 触发取消，停止调用，`Cancelled`，终态事件一次。
10. `cancel_mid_tool_batch_keeps_completed_results`: 前序已完成结果保留，活动/未启动 calls 都有取消结果，不再启动工具。
11. `stop_reasons_map_to_outcomes`: `MaxTokens`（无 tool）、`Refusal`、`ContentFiltered`、`PauseTurn`、未知 reason 都按表处理；任意非 MaxTokens/ContentFiltered response 含 tool call 时执行工具，不因 stop reason mismatch 报协议错误。
12. `session_history_continues_across_turns`: 旧 user/assistant history 加新输入后请求内容完整；同一 AgentSession 的 transcript 和 ToolSession 在多轮之间保持；带未配对 tool call 的 history 被拒绝。
13. `provider_payload_round_trips`: assistant opaque payload 序列化再交给 adapter 时值不变；多模态 ContentPart 不丢 image/audio/resource 数据和 annotations。
14. `step_and_run_share_semantics`: 相同脚本分别手动 step 与 run，比较 transcript、tool 调用、outcome 和核心事件序列。
15. `terminal_run_cannot_step_again`: 终态后调用 step 返回稳定的 `RunFinished` 状态错误。
16. `independent_agent_sessions_do_not_share_state`: 两个独立 AgentSession 状态、ToolSession、事件和取消互不串扰。
17. `dropping_polled_step_poisons_run`: 已 poll 的 step 或 run future 被 drop 后 Run/Session Poisoned、只发一次终态事件、后续不能 step；未 poll 的 future drop 不产生副作用。
18. `pause_turn_continues_same_run_budget`: PauseTurn 返回 Continue，不调用工具，原始 assistant 内容保留；后续请求计入同一预算，不能重置。
19. `schema_external_refs_are_disabled`: HTTP/file `$ref` 不发起网络或文件访问；只接受内嵌或显式 registry schema。
20. `poisoned_session_history_is_resumable`: 工具执行中 drop 已 poll 的 future 后，`history_for_resume()` 中每个 tool call 都有结果（未完成的为 `interrupted`）；用它 `open_session_with_history` 成功并能继续一轮。
21. `content_filtered_tool_calls_are_never_executed`: `ContentFiltered` 响应携带的工具调用零执行、都有 `content_filtered` 结果，Run 以 `Incomplete` 结束。
22. `server_side_tool_blocks_are_not_dispatched`: 含服务端工具块的响应（含 `PauseTurn`）不产生 Core `ToolCall`、不调用 ToolSession，块内容原样保留到下一次模型请求。
23. `turn_deadline_cancels_cleanly`: `TurnOptions::deadline` 到期走正常取消收敛，`Cancelled { reason: DeadlineExceeded }`、终态事件一次、不 poison 会话。

Steering：

24. `steer_during_batch_injects_before_next_model_call`: 工具执行中 steer，批次跑完后、下一次模型请求前注入；请求内容中消息位于工具结果之后。
25. `steer_on_final_answer_continues_turn`: 模型给出最终回答时队列非空，本轮不结束，注入后再请求模型，并计入预算。
26. `steer_skip_remaining_skips_unstarted_calls`: `SkipRemaining` 下已启动调用完成、未启动调用得到 `skipped_by_steering` 且未执行。
27. `unapplied_steering_survives_every_terminal`: Cancelled、BudgetExceeded、Failed、Poisoned 各终态下未注入消息都在 `unapplied_steering` 中；终态后 `steer()` 原样交还消息。
28. `steer_never_interrupts_model_stream`: 模型流式响应中 steer，消息不出现在本次请求中，下一安全点才注入。

Hooks：

29. `before_tool_decisions`: Allow / Deny / Rewrite / Respond 分别正确执行、合成错误、改参执行、跳过执行。
30. `rewritten_args_are_revalidated_without_rehooking`: Rewrite 后的非法参数被 preflight 拒绝；hook 只调用一次。
31. `hook_errors_fail_closed`: `before_model_call`/`after_model_response` 出错 → `Failed(Hook)`；`before_tool_call` 出错 → 不执行；`after_tool_call` 出错 → 原结果不进入 transcript。
32. `hook_composition_order`: `(A, B)` 按顺序执行，第一个非 Allow 决定短路，Rewrite 参数传递给后续 hook。
33. `approval_hook_races_cancellation`: `before_tool_call` 等待审批时取消，Run 以 `Cancelled` 结束且工具未执行。

并行工具：

34. `shared_calls_overlap_and_commit_in_order`: 带不同延迟的 `Shared` 工具实际并发；任意完成时序下 transcript 顺序与调用顺序一致；`ToolEnd` 事件按完成顺序并带序号。
35. `exclusive_call_is_a_barrier`: `Exclusive` 调用等之前所有调用完成后单独执行，之后的调用等它完成才启动。
36. `max_parallel_tools_bounds_concurrency`: 并发数从不超过上限；上限为 1 时行为与串行一致。
37. `cancel_during_parallel_batch_cancels_all_in_flight`: 所有进行中调用收到取消，每个 ID 恰有一个结果，`step()` 在全部结束后返回。
38. `drop_during_parallel_batch_poisons_and_fills_interrupted`: 并行批次中 drop future，所有进行中调用被 abort 并得到 `interrupted` 结果。

### MCP 子进程 tests（`cargo test -p agent-extensions --test subprocess`）

测试 fixture 覆盖正常 server、tool error、hang、crash、错误协议版本、坏 JSON、写 stderr 日志和派生子进程：

- `lists_tools_and_maps_provider_safe_names`: `tools/list` 原名映射为安全、唯一的 model tool name；回调时还原正确 MCP 名。
- `tool_alias_is_stable_across_sessions`: 相同稳定 `server_id` + 原始名在列表换序、进程重启、新 AgentSession 以及 server 新增/删除其他工具后映射为相同 alias；合法短名保持可读形式。
- `tool_alias_collision_fails_session_open`: 两个工具得到相同 alias 时打开 AgentSession 失败并报告双方，已启动进程被回收。
- `unavailable_extension_tools_stay_declared`: 扩展失效后工具声明不变，调用立即返回明确的 `extension_unavailable` 结果。
- `tool_error_uses_is_error_result`: MCP 标准错误结果成为 ToolResult，而不是协议错误。
- `progress_maps_to_tool_progress`: MCP progress 关联到当前 request/tool call。
- `cancel_sends_mcp_notification`: 取消发送 `notifications/cancelled`，对晚到响应安全忽略。
- `timeout_kills_and_reaps_process_tree`: deadline/cancel grace 后终止并 wait；Unix 检查 process group，Windows 检查 Job Object 中没有存活成员；该 ToolSession 在当前 AgentSession 剩余时间保持 unavailable。
- `drop_kills_without_tokio_runtime`: drop AgentSession 或已 poll 的 step/run future 时同步发出整棵进程树 kill；即使 Tokio runtime 随后关闭，runtime-independent supervisor 仍 wait/reap。
- `partial_session_start_reaps_children`: 一个 MCP server 启动成功、下一个启动失败时，打开 AgentSession 返回错误，并回收前一个已启动的进程树。
- `server_state_survives_multiple_turns`: fixture MCP server 在一个 AgentSession 的两个 user turn 间保留内存状态；新 AgentSession 使用新进程。
- `next_session_starts_fresh_server`: 当前 AgentSession 内崩溃不重启；下一次新 AgentSession spawn 新进程并重新发现 tools。
- `unsupported_server_requests_fail_without_hanging`: 旧式 sampling/roots/elicitation 不声明能力并快速返回 method-not-found；MRTR InputRequired 被一次性检测并返回 unsupported interaction error。
- `tool_content_blocks_round_trip`: text/image/audio/resource link/embedded resource、structured content 与 meta 均保留。
- `environment_is_allowlisted`: 未列出的 secret 环境变量不出现在子进程。
- `stdout_is_protocol_only`: stderr 日志允许；stdout 坏帧/超限帧导致当前 MCP session 失败并回收。
- `concurrent_agent_sessions_have_isolated_processes`: 两个并发 AgentSession 有不同 child process；一个会话取消不影响另一个。
- `shared_calls_multiplex_on_one_server`: 同一 server 上的多个 `Shared` 调用并发发送并按 request id 正确配对；单个取消只影响对应请求。
- `read_only_hint_not_trusted_by_default`: 未开启 `trust_read_only_hint` 时，带 `readOnlyHint` 的 MCP 工具仍按 `Exclusive` 调度。
- `timeout_kill_fails_sibling_calls_clearly`: 并行中一个调用超时导致杀进程，同一 server 上其他进行中调用得到说明原因的 `extension_unavailable` 结果。

建议验证命令：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo run -p agent-core --example minimal
cargo test -p agent-extensions --test subprocess
```

## 9. Benchmark 方案

隔离 Runtime 成本和 Provider/网络延迟。使用固定脚本、release profile，记录 rustc、OS、CPU、profile、commit、样本数；每个场景至少 warm-up 100 次、测量 1,000 次，报告 p50/p95、波动范围、RSS 和分配量。

| 基准 | 场景 | 指标 |
|---|---|---|
| Core no-tool | faux model 一次 final answer，重复 1k 次 | `step()` overhead、每 run 延迟 p50/p95、吞吐 |
| Core tool loop | 每轮一个 no-op tool，1/10/100 次 | 单轮开销、ToolSession 调度、完整 run 延迟、事件数 |
| Schema validation | 小/中/大 JSON Schema 与有效/无效 args | 验证延迟、分配量 |
| Parallel tools | 8/32 个 `Shared` 工具各 sleep 固定时长，`max_parallel_tools` = 1/4/8 | 批次墙钟时间、相对串行的加速比、调度开销 |
| Hooks | 0/1/4 个 no-op hook 组合 | 每次模型/工具调用的额外开销 |
| MCP call | 已启动 server 内重复 1/8/32 calls | request/response IPC p50/p95、payload bytes、RSS |
| MCP cold session | 每次创建 AgentSession 并 spawn/discover/调用/关闭 | 启动、关闭及总运行 p50/p95、进程资源 |
| MCP warm turns | 同一 AgentSession 连续完成 1/10/100 轮，复用 server | 轮次间延迟、工具状态连续性、会话内 RSS |
| Cancellation | Model/tool/MCP server 分别阻塞 | cancel-to-return、process-tree reap 时间、残留进程数 |

先建立一台参考机器基线，不预设 Pi/Rig 性能目标。目标建议：Core 单次 loop 调度 p50 < 100 μs、p95 < 500 μs；已启动 MCP server 的 no-op call p50 < 2 ms、p95 < 10 ms；显式取消返回或 AgentSession Drop 后进程树归零。cold-session spawn 与 warm multi-turn 分开报告。

## 附录 A：未来委派扩展（非 MVP）

当前通过 Herd 传递 instruction/shell commands 的工作，未来可由可选 `agent-delegation` MCP Server 承接。主 Agent 以普通 `delegate_task` 工具提交结构化任务；扩展使用独立模型、预算、deadline 和工具权限运行子 Agent，再返回有界摘要与产物引用。扩展可以内部复用 `agent-core` 或桥接 Herd；Core 不新增 Agent registry、任务图、角色系统、消息总线或自动分工。该扩展另做 ADR，MVP 仅确保 AgentSession/ToolSession 可独立创建与隔离。
