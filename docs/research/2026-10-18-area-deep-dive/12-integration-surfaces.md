# 深挖 12：接入场景（入口与集成面）

2026-10-18 · Radian `hisea/radian@356df1e` · 对手源码：codex `8c07694`（HEAD）、opencode `packages/cli`、pi-mono `482c138`、omp 18.12.1 npm 包；Claude Code 只用官方文档。网页均为 2026-10-18 抓取。

本区域不比速度，比的是 **Radian 能在多少种场景里被用起来**：编辑器、CI、嵌入、远程、聊天平台、终端复用器。分报告 4 和 6 已经指出 `radian exec` 没有机器可读输出，这里不再重复它对评测的意义，只讲它作为入口的价值。

## 1. 一句话结论

Radian 只有两个入口：全屏 TUI 和 `radian exec`，外加一个只有本机进程能用的控制 socket。五个对手里有四家（opencode、omp 原生，Codex、Claude Code 通过 Zed 维护的适配器）进了 ACP 注册表，**装一次就能在 Zed、JetBrains 等 13 个客户端里用**。Codex、Claude Code、pi、opencode 的无头模式都有 JSON 事件流和结构化的最终结果。Radian 的分层（`open_session` + `HostBindings` + `Approver` trait + 控制口的事件编码器）很适合再挂一个前端，所以**最该先开的三个入口**按顺序是：① `radian exec --output jsonl`，附带结果记录和细分的退出码；② `radian acp`（ACP agent，stdio）；③ 把控制 socket 做成有文档的本机协议，加上客户端子命令，补齐 herdr 的“发消息”。远程/手机、Slack、自建 app-server 协议、各语言 SDK 都先不做。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 编辑器（ACP） | 无 | 上游没有；社区适配器 `pi-acp` 起 `pi --mode rpc` 再翻译成 ACP（[DxTa/pi-acp](https://github.com/DxTa/pi-acp)） | 上游没有；注册表里的 "Codex 2.2.2" 写明是 "ACP adapter"（[registry](https://agentclientprotocol.com/registry)） | 上游没有；注册表里的 "Claude Agent 0.93.0" 是 "ACP wrapper"（同上） | 原生 `omp acp`，`src/modes/acp/` 共 4 265 行，支持 `loadSession`，并使用客户端提供的 `fs/*` 和 `terminal` 能力（`acp-client-bridge.ts:31-65`） | 原生 `opencode acp`，`cli/src/acp/` 共 3 369 行，用 `@agentclientprotocol/sdk` 1.6.0（`cli/package.json:27`）；支持 load/list/resume/fork（`acp/agent.ts:82-102`） |
| 编辑器（自有插件） | 无 | 无 | VS Code 扩展和桌面 app 都内嵌 app-server 二进制（[OpenAI 博客 2026-02-04](https://openai.com/kk-Cyrl/index/unlocking-the-codex-harness/)） | VS Code、JetBrains 插件：在 IDE 终端里跑 `claude`，通过本机 `ws://` MCP server 拿选区、diff 和诊断（[jetbrains](https://code.claude.com/docs/en/jetbrains.md)） | 无（未核实） | `packages/desktop`（Electron） |
| 无头输出格式 | `-p`：stdout 只有模型文本，状态走 stderr（ADR 0032 A3）；`--export --format json` 只对已结束的会话 | text / `--mode json`（严格 LF 分隔的 JSONL）/ `--mode rpc`（`docs/json.md`、`cli-integration.md`） | `exec --json`，事件为 `thread.*`、`turn.*`、`item.*`（`exec/src/exec_events.rs:8-36`）；`--output-schema`、`--output-last-message`（`exec/src/cli.rs:52-80`） | `--output-format text\|json\|stream-json`，加 `--json-schema`；最后一行 `result` 带费用和 session id（[headless](https://code.claude.com/docs/en/headless)） | print 模式和 rpc（`src/modes/print-mode.ts`、`rpc/` 共 8 259 行） | `run --format default\|json`（`cli/src/commands/commands.ts:376-416`） |
| 退出码 | 0 完成；1 表示其他所有结局（失败、未完成、拒答、超预算、取消）；2 用法；130（`agent-cli/src/turn.rs:103-112`） | print 模式下 `error`、`aborted`、`length` 都非 0；json 模式不因失败退非 0（`cli-integration.md:30,46`） | 失败时 `exit(1)`（`exec/src/lib.rs` 多处） | 0 或非 0；SIGTERM 退 143（headless 文档） | 未核实 | 未核实 |
| 无人值守的审批 | `--yes`；否则无终端时 ask 一律拒绝（`approval.rs:35-36`）；不注册 `ask_user`（ADR 0024 Q5） | 扩展的对话框转成 `extension_ui_request`，由客户端回答（`rpc-extension-ui.md`） | `--dangerously-bypass-approvals-and-sandbox`；app-server 把审批作为 item 交给客户端 | `--permission-mode`、`--allowedTools`、`--permission-prompts none`、`--permission-prompt-tool`（MCP 审批人） | `--approval-mode`（`cli-commands.ts:451`） | `--auto`（`commands.ts:26-36`） |
| CI / GitHub Action | 无 | 无（未核实） | `openai/codex-action@v1`，内部跑 `codex exec`，有 `final-message` 输出（[文档](https://learn.chatgpt.com/docs/github-action.md)） | GitHub Actions、GitLab CI/CD 文档；`--bare` 让结果在每台机器上一样（headless 文档） | 无（未核实） | 仓库内的 `github/action.yml`（在 issue 里 `/opencode` 触发） |
| SDK / 嵌入 | `agent-core` 是库，没有 I/O，可以发布；但 provider、`radian-host`、`agent-cli` 都标了 `publish = false`，嵌入方只能拿到一个空循环 | `createAgentSession()` 进程内 SDK，另有 `RpcClient`（`docs/sdk.md`、`rpc.md`） | TS SDK 子进程跑 `codex exec --experimental-json`（`sdk/typescript/src/exec.ts:95,203`）；Python SDK（`sdk/python`） | Agent SDK（Py/TS），内部运行 CLI 二进制 | `src/sdk.ts` | HTTP server 加生成的 `packages/sdk` |
| app-server / RPC | 每个进程一个 Unix socket，JSON 行；动词有 `followup steer inject cancel list_sessions subscribe`，**没有审批动词**；输入来源固定不为 `user`（`control.rs:300-338`，`CONTROL.md`） | `--mode rpc`：约 40 个命令，包括 steer、follow_up、fork、compact、bash（`rpc-commands.md`） | app-server：JSON-RPC，thread/turn/item，transport 有 stdio、ws、uds；官方客户端语言有 Go、Py、TS、Swift、Kotlin（博客） | SDK 的 stream-json 双向流（`--input-format stream-json`） | rpc 模式加 jsonrpc | 缺省 client/server（`serve`、`service`、`api`） |
| 远程 / 手机 | `radian attach --follow` 本机只读（ADR 0043 §8）；跨机器列为远期 | 无 | `remote-control`、app-server daemon，给桌面和手机用（`app-server-daemon/README.md`）；`cloud` 任务 | Remote Control：本机只发出站 HTTPS，经 Anthropic 中继，手机和网页都能接（[remote-control](https://code.claude.com/docs/en/remote-control)）；另有云端会话 | `collab/`：中继 + AES-256-GCM 的多人会话（`collab/protocol.ts:1-8`） | `opencode pair`：一次性链接或二维码，`--remote` 走隧道（`handlers/pair.ts`） |
| 聊天平台 | 无 | 无 | Slack、Linear 触发云端任务（[codex slack](https://developers.openai.com/codex/integrations/slack)） | Slack 里 @Claude 会开一个云端会话，产出 PR（[slack](https://code.claude.com/docs/en/slack.md)） | `irc/`（内部 agent 总线，不是聊天平台） | 无（未核实） |
| 终端复用器 | herdr 自报状态、元数据和会话身份，还有插件（ADR 0034）；但 **`herdr agent prompt` 发不进 radian** | herdr 扩展（ADR 0034 背景 3） | 无 | herdr hook 只报会话身份（同上） | 无 | 无 |

## 3. 各家的关键做法

**opencode**
- `opencode acp` 先在本进程起 standalone server，再用 `@agentclientprotocol/sdk` 的 `ndJsonStream` 接上 stdin 和 stdout（`cli/src/commands/handlers/acp.ts:26-37`）。ACP 只是又一个客户端，翻译层 3 369 行，测试目录 `test/acp/` 有 19 个文件。
- 声明的能力：`loadSession`、`mcpCapabilities{http}`、`promptCapabilities{embeddedContext,image}`（`acp/service.ts:120-124`）。权限选项映射成 `allow_once / allow_always / reject_once`（`acp/permission.ts:40-42`）。
- 同时有 GitHub Action（`github/action.yml`，按 `/opencode` 触发）、`pair`（给浏览器或手机一次性链接，`handlers/pair.ts`）和 Electron 桌面端。接入面最全，代价是缺省 client/server（分报告 4）。

**omp**
- `omp acp`（`cli-commands.ts:36`）。如果客户端声明了 `fs.readTextFile`、`fs.writeTextFile` 或 `terminal`，文件读写和终端就交给编辑器执行（`modes/acp/acp-client-bridge.ts:31-65`）。编辑器里未保存的缓冲区因此也能被 agent 看到。
- `collab/` 是端到端加密的多人会话：中继只看得到信封（`collab/protocol.ts:1-8`）。

**pi**
- 四种模式一套内核：交互、print、json、rpc（`docs/cli-integration.md`）。rpc 有 `steer`、`follow_up`、`abort`、`fork`、`compact`、`bash` 等命令（`rpc-commands.md`），和 Radian 控制口的动词几乎一一对应。
- 扩展发起的对话框（`confirm`、`select`）在 rpc 模式下变成请求和响应，交给客户端回答（`rpc-extension-ui.md`）。**审批能交给外部前端**，这正是 Radian 控制口刻意不做的部分。
- 上游没有 ACP，社区用“`pi --mode rpc` 加翻译”做出 `pi-acp`（[DxTa/pi-acp](https://github.com/DxTa/pi-acp)；不支持 `fs/*`、`terminal/*` 委托）。所以只要有 RPC，ACP 适配就能在仓库外做。

**Codex**
- 所有前端都是 app-server 的客户端，包括 TUI、exec、VS Code、桌面端和网页。OpenAI 自述先试过把 Codex 做成 MCP server，但 VS Code 需要的语义很难放进 MCP，于是自建了 JSON-RPC 协议（[博客](https://openai.com/kk-Cyrl/index/unlocking-the-codex-harness/)，2026-02-04）。代价是分报告 4 记录的 208 万行代码。
- SDK 很薄：TS SDK 就是子进程加 `exec --experimental-json`（`sdk/typescript/src/exec.ts:95`）。GitHub Action 也直接跑 `codex exec`。**一个稳定的 JSONL 事件流同时撑起了 SDK 和 CI 两个入口。**
- `codex queue`：“Queue a message for an existing session”（`cli/src/main.rs:210-211`），对应 Radian 控制口的 `followup`，只是做成了 CLI。

**Claude Code**（全部来自文档）
- `-p` 有三种输出，`stream-json` 的最后一行是 `result`，带 `total_cost_usd` 和 session id。`system/init` 列出 MCP 加载失败的项，CI 可以据此判失败。无人值守时用 `--permission-prompts none` 明确拒绝，拒绝记录汇总进 `permission_denials`（[headless](https://code.claude.com/docs/en/headless)）。
- IDE 集成不走 ACP：插件在 IDE 终端里跑 CLI，IDE 侧开一个本机 MCP server，提供 diff、选区和 `getDiagnostics`（[jetbrains](https://code.claude.com/docs/en/jetbrains.md)）。
- Remote Control：本机进程只发出站 HTTPS，向 Anthropic API 注册后轮询，手机和网页的消息都经中继转发；只支持订阅账号，不支持 API key（[remote-control](https://code.claude.com/docs/en/remote-control)）。Slack 入口实际是开一个**云端**会话（[slack](https://code.claude.com/docs/en/slack.md)）。这两项都依赖厂商自己的云。

**ACP 生态**（2026-10-18）
- zed.dev/acp 列出 13 个客户端（Zed、JetBrains、VS Code、Emacs、Neovim、Obsidian、marimo 等）和 47 个 agent（[zed.dev/acp](https://zed.dev/acp)）。JetBrains 2025.3 起内置 ACP 注册表，一键安装（[JetBrains 博客](https://blog.jetbrains.com/ai/2026/01/acp-agent-registry/)）。
- 上架条件：`agent.json` 必须有 `id/name/version/description/distribution`；`binary` 分发**必须同时覆盖 darwin、linux、windows**；`initialize` 必须返回非空的 `authMethods`（Agent Auth 或 Terminal Auth），由 CI 检查（[注册表 RFD](https://agentclientprotocol.com/rfds/acp-agent-registry.md)）。
- 官方 Rust SDK `agent-client-protocol` 2.0.0，2026-07-23 发布，Apache-2.0，运行时依赖是 async-io 和 futures，tokio 只是 dev 依赖（[docs.rs](https://docs.rs/agent-client-protocol/=2.0.0/)）。MSRV 文档没写，**未核实是否 ≤ 1.88**。

## 4. Radian 的优势

1. **加一个前端几乎不用动核心。** `open_session(setup, bindings)` 是建会话的唯一入口（`radian-host/src/session/open.rs:32`）。`HostBindings` 已经把 `approvals`、`ask_user`、`events`、`audit`、`projection` 都做成可注入的（同文件 `:36-48`）。审批是 `Approver` trait（`radian-permissions/src/approval.rs:77-82`）。TUI 和 exec 就是这些绑定的两种实现，ADR 0023 U5 把它定义成“Facade：门面，不是所有者”。ACP 或 JSONL 前端是第三种实现，**不用改 `agent-core`**。
2. **事件的 JSON 编码已经写好。** 控制口的 `agent_event_json` 和 `runtime_event_json`（`control.rs:430-660`）已经把每种 `AgentEvent` 和 `RuntimeEvent` 编成 JSON，也已经按 ADR 0010 区分了 live 和 durable。exec 的 JSONL 输出和 ACP 的 `session/update` 都能直接用。
3. **会话语义齐全，能直接对上 ACP。** resume、fork（ADR 0022）、只读观察（`JournalObserver`，`radian attach`）、steer 和 followup（ADR 0011），对应 ACP 的 `session/load`、fork、`session/cancel` 和中途追加输入。`RunStatus` 的 `Completed / Incomplete(max_tokens) / Refused / BudgetExceeded / Cancelled`（`agent-core/src/outcome.rs:28-39`）和 ACP 的 stopReason（`end_turn / max_tokens / refusal / max_turn_requests / cancelled`）几乎一一对应（ACP 枚举按记忆，本轮未抓取 schema 核对）。
4. **单个原生二进制，没有运行时依赖**，符合 ACP 注册表的 `binary` 分发方式（opencode 和 omp 要用 npx 或 bun）。启动约 32–55 ms（分报告 4），编辑器每开一个会话就起一个进程，这个延迟负担得起。
5. **herdr 集成是各家里最深的**：状态、元数据、`blocked` 标签、会话身份、`HERDR_*` 放行，还有仓库内插件（ADR 0034 “已落地”）。
6. **入口的安全边界写得很清楚**：控制口拒绝 `user` 来源和审批（`CONTROL.md`）；混入非人输入时有效来源只降不升（ADR 0012 规则 5）；导出时强制脱敏（ADR 0013）。新开入口时，这些规则可以直接拿来回答“这个入口算不算人”。

## 5. Radian 的劣势和缺口

1. **不能在任何编辑器里用。** 没有 ACP，没有插件，也没有能让第三方写适配器的 RPC。控制口不转发文本 delta（`control.rs:540-546` 只转发 committed 事件，`ModelTextDelta` 的 payload 被编成空对象，`:571`），也不能回答审批，所以连 pi-acp 那种仓库外适配器都做不出来。这是用户量上最大的缺口：47 个 agent 都在注册表里，Radian 不在。
2. **无头模式只能管道传文本。** 没有事件流，没有结果记录，**退出码把五种结局都并成 1**（`turn.rs:103-112`）。CI 分不清“模型说完了但被截断”和“provider 报错”。`-p` 不能从 stdin 读 prompt（`agent-cli` 只有 memory 子命令读 `-`，`command.rs:191-201`）。也没有 `--output-schema` 或 `--output-last-message`。
3. **无人值守时被拒绝的调用不透明。** 无终端时 ask 一律拒绝，但拒绝了哪些只写在 journal 的 `PermissionResolved` 里，没有汇总。Claude Code 有 `permission_denials`。
4. **嵌入只剩骨架。** `agent-core` 可以发布，但 provider、host、coding 层的组合件（`radian-host`、`agent-provider-*`、`agent-cli`）都是 `publish = false`。Rust 用户拿不到“开一个 coding 会话”的库 API；其他语言的用户在没有 JSONL 之前连子进程 SDK 都写不了。
5. **herdr 只完成了一半。** `herdr agent prompt` 对 radian 返回 `not an active named agent`，因为 herdr 0.9.5 不把自报的未知 kind 当成有名 agent。给 herdr 提 `--kind radian` 的 PR，owner 已决定暂缓（ADR 0034 “手测结果”）。p1 只能用 `pane send-text` 发消息，发丢过一次。控制口能收 followup，但 herdr 和脚本不知道某个 pane 对应哪个 socket，也没有现成的客户端命令。
6. **没有远程和聊天入口，而且短期也不该有。** Claude Code、Codex 的手机和 Slack 入口都建立在各自厂商的云中继或云会话上。Radian 没有服务端，ADR 0013 缺省不出网，ADR 0023 U5 写明“不设计远程协议”。

## 6. 改进方案（按对 Radian 整体价值的收益/成本排序）

### ① `radian exec --output jsonl`，附带结果记录、细分退出码和 stdin prompt（收益高，成本低）

- **做什么。**
  - `--output text|jsonl`，缺省 `text`，行为不变。`jsonl` 时 stdout 只输出 LF 分隔的 JSON：第一行是 `session`（id、workspace、profile、model），中间是事件（复用 `control.rs` 的两个编码器；`--include-partial` 时再加上 `model_text_delta`，带文本），最后一行是 `result`（RunStatus、final text、usage 四类、`fee`、模型和工具调用数、被拒绝的调用列表、耗时）。
  - 退出码细分：0 完成；1 provider 或驱动错误；3 未完成（max_tokens 或超预算）；4 拒答；5 超时或取消；2 和 130 不变。具体编号写 ADR 时定。
  - `-p -` 从 stdin 读 prompt。
  - 加一个 `--output-last-message FILE`。
- **复用多少。** 编码器、`RunStatus`、`fee`、`PermissionResolved` 都已经有了，约八成。要补的是 stdout 的写入端（一个 `EventSink` 实现）、result 的汇总、schema 和 fixture。
- **为什么有价值。** 一份事件流能同时撑起 CI、GitHub Action、Python/TS 子进程 SDK、评测框架（分报告 6）和 subagent 的 usage 回传，Codex 的 SDK 和 Action 就是这么做的（§3）。之后写 Action 只是一个十几行的 composite yml。
- **成本。** 约 2–3 天：代码 400–600 行，加上 golden 测试和 schema 校验。
- **ADR。** ADR 0032 A3 原文是“若将来要统一 JSON 输出，另立决定”，退出码也是 A3 的契约，所以要**写一个新 ADR**（或 0032 的修订）。只在本地输出，符合 ADR 0013。delta 是否进入流要按 ADR 0010 写明“live、不保证完整”。
- **验证。** 用 `check-fixtures.sh` 那套方式校验每一行的 schema；断言 result 里的数字等于从同一份 journal 离线算出的数字；对每种 RunStatus 各写一个退出码测试，并做变异：把任意两种结局合并成同一个退出码，测试应当变红。

### ② `radian acp`：ACP agent over stdio（收益最高，成本中）

- **做什么。** 新子命令。它本身就是一个前端：进程内调 `open_session`，**不经控制 socket**，因为编辑器背后就是人，输入来源应当是 `User`，而控制口按规定拒绝 `user`。映射关系：
  - `initialize`：`loadSession: true`，`promptCapabilities{embeddedContext}`，`authMethods` 用 Terminal Auth 指向 `radian login`。
  - `session/new`：用 `cwd` 当 workspace，走现有的 profile 和 trust 解析。
  - `session/prompt`：`inbox.followup`；回合进行中再来的输入走 steer。
  - `session/update`：`ModelTextDelta` 映射为 `agent_message_chunk`，`ModelReasoningDelta` 映射为 `agent_thought_chunk`，`ToolStarted` 和 `ToolFinished` 映射为 `tool_call` 和 `tool_call_update`；`patch` 和 `write` 的结果带 diff。
  - `session/request_permission`：一个新的 `Approver` 实现，把 y/a/拒绝三种选择映射成 `allow_once / allow_always / reject_once`。
  - `session/cancel`：`inbox.cancel`。
  - `session/load`：resume 之后用 `JournalObserver` 回放历史。
  - stopReason：从 `RunStatus` 映射。
  - 第一版不做 `fs/*` 和 `terminal` 委托（omp 做了，留到第二步），也不注册 `ask_user`（ADR 0024 Q5）；之后再评估用 ACP elicitation 实现 `ask_user`（opencode 有 `acp/elicitation.ts`）。
- **复用多少。** `open_session`、`HostBindings`、`Approver`、inbox、projection、journal 回放、fork、login 都是现成的，约七成。要补的是 JSON-RPC 层（用 `agent-client-protocol` 2.0.0，或手写约 300 行）、事件到 ACP 的映射、一个 Approver、注册表的 `agent.json`，以及三平台发布。
- **为什么有价值。** 这是让 Radian **进入 Zed、JetBrains、Neovim、Emacs 用户视野的唯一低成本入口**，注册表一键安装（§3）。opencode 和 omp 已经证明翻译层只要 3–4 千行 TS；Radian 的事件和会话模型更规整，Rust 实现估计 1 500–2 500 行，含测试。
- **成本。** 1.5–2.5 周。风险有两项：一是注册表的 binary 分发要求有 Windows 构建，Radian 的 Windows 支持没有核实，控制口也只在 `cfg(unix)` 下有，得先确认 `radian acp` 不依赖 Unix socket；二是 SDK crate 的 MSRV 和依赖树未核实，如果超过 1.88 或依赖太重，就手写 JSON-RPC。
- **ADR。** 要**写一个新 ADR**，内容包括：第三个宿主（ADR 0023 U5 只列了进程内和只读观察者两种实现，新增前端在边界之内，但要写清楚）；编辑器输入算 `User` 来源（ADR 0012 规则 5）；ACP 会话在未信任的仓库里怎么处理信任门（ADR 0031：没有 TUI 弹窗，建议未信任时只加载内建资源，并在第一条 update 里说明）；ADR 0032 的子命令清单加 `acp`；ADR 0024 Q5 是否扩展到 ACP。不改 `agent-core`。
- **验证。** 用一个脚本化的 ACP 客户端做集成测试，覆盖 initialize、new、prompt、权限往返、cancel 和 load 之后的历史一致性。每个映射做变异，例如把 `allow_always` 当成 `allow_once`，“本会话授权”的测试应当变红。手测项：在 Zed 和 JetBrains 里各跑一个真实任务，写进 `radian-tui/README.md` 的手测清单。

### ③ 控制口成为有文档的本机协议，加客户端子命令，补齐 herdr（收益中，成本低）

- **做什么。**
  1. 新增 `radian send <session> "text" [--steer] [--wait]` 和 `radian sessions --live`（列出 `run/*.sock` 和各自的会话 id）。按会话 id 找 socket，需要在 `run/` 里多写一个 `<session>.id`，或者让 `list_sessions` 能跨进程枚举。语义对应 `codex queue`。
  2. 在 herdr 的 `report_metadata` 里报出控制口路径，或者让插件动作调用 `radian send`。这样 herdr 和 p1 不必再靠 `send-text`，也不必等 herdr 的 `--kind` PR。
  3. `subscribe` 加一个可选的 `include_partial`，用来转发文本 delta，让仓库外的适配器和看板能看到流式输出。
  4. 审批动词**仍然不做**；远期的 owner 通道（ADR 0043 远期一节）再说。
- **复用多少。** 协议和服务端都已存在（864 行），约九成。要补的是客户端子命令（约 200 行）、会话 id 到 socket 的索引、herdr 插件脚本的改动。
- **为什么有价值。** 把已经写好、测好的 socket 变成用户和脚本真正能用的入口；解决 p1 实测发丢消息的问题；也给第三方在仓库外做 Radian 的 ACP、看板或 Slack 桥留了一个稳定的接口，pi 的 RPC 就是因此有了 pi-acp。
- **成本。** 2–4 天。
- **ADR。** ADR 0032 的子命令清单要改；控制口的规则（`CONTROL.md`、ADR 0043 第 1 条“control.rs 留”）不变；herdr 一侧属于 ADR 0034 的范围，要补一条修订（决定 3 暂缓的替代路径）。`send` 进来的输入是 `Scheduler` 来源，按 ADR 0012 规则 5 会降低回合的有效来源。要在文档里写明这是有意的：脚本不是人。
- **验证。** 两个进程的集成测试：`send --wait` 的退出码和回合结局一致；按会话 id 找 socket，遇到陈旧的 sock 时清理；变异：把来源改成 `User`，“控制口拒绝 user”的测试应当变红。herdr 手测：插件动作发出的消息出现在 journal 里，`agent list` 的状态在 1 秒内变成 `working`。

### 先不做，以及原因

| 入口 | 为什么不做 / 什么时候再看 |
|---|---|
| 自建 app-server 那种 JSON-RPC 协议 | Codex 为它付出了全量协议层和 daemon（分报告 4）。ACP 已经是编辑器侧的事实标准，Radian 没有自己的桌面或网页客户端，用不上。等将来有 `radian orca` 或跨机器需求再说（ADR 0043 远期） |
| 远程和手机控制 | 要么自建中继（新服务，碰 ADR 0013 的不出网缺省），要么依赖厂商云。可以先写文档：用 SSH 转发控制 socket，再加 `radian attach`（ADR 0043 远期“跨机器”）。opencode 的 `pair` 和 omp 的 collab 都要常驻 server |
| Slack 等聊天平台 | 对手的做法都是开云端会话（§3），Radian 没有云。等 ① 落地后，Slack bot 可以作为仓库外的示例，用“bot 跑 `radian exec --output jsonl`”实现，不进核心 |
| Python/TS SDK 包 | ① 落地后就是一层很薄的子进程封装（Codex TS SDK 的做法），可以交给 pi 或社区；先在 README 给一个 30 行的示例 |
| Rust 嵌入 API（发布 `radian-host` 或 provider） | 发布就等于冻结 `open_session` 那几十个字段的 `SessionSetup`，会限制维护。先让 ① 和 ② 跑通再评估，到时候可能需要新 ADR，定公共 API 的稳定性承诺 |
| 原生 VS Code 或 JetBrains 插件 | JetBrains 已经内置 ACP；VS Code 也被列为 ACP 客户端（[zed.dev/acp](https://zed.dev/acp)，具体由哪个扩展实现未核实）。自己做插件是多余的工作 |
| 用 MCP server 模式（`radian mcp`，已保留）作为编辑器入口 | OpenAI 自述 MCP 很难表达 IDE 需要的交互（§3）。`radian mcp` 适合“别的 agent 把 Radian 当工具用”，优先级低于 ACP |

### 顺序和依赖

① 先做：成本最低，它的 result 汇总和 Approver 拒绝汇总 ② 也要用，并且直接解锁 CI、SDK 和评测。② 第二个做，收益最大，用 ①留下的 golden 测试框架验证映射。③ 可以和 ① 并行，只碰宿主和 CLI，适合交给 pi；按 AGENTS.md 的规则，任务要能用命令验证，并且和时序无关。三项都**不改 `agent-core`**。
