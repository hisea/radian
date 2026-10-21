# ACP 服务端 `radian acp`

- 状态：idea＼�暂定，不排期；生态兼容的另外四件先做）
- 涉及 ADR：新 ADR；0032（一个二进制、保留的子命令）、0009（全屏 TUI 之外的宿主面）、0024（审批与提问映射到 `request_permission`）、0011（`session/prompt` 走 inbox）、0010（`session/update` 只发 live 事件）、0043（控制口）
- 来源：`research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.6、§6 ⑥；总报告 README 第 32 条（入口顺序：先 `exec --output jsonl`，再控制口客户端，最后 `radian acp`）
- 依赖：`exec-jsonl-output.md` 先落地；控制口客户端（总报告第 32 条，尚无 backlog 文件）

## 背景

- ACP（Agent Client Protocol，agentclientprotocol.com）列了 43 个 agent、13 个编辑器客户端。opencode 原生实现了服务端；Claude Code、Codex、pi 各通过一个适配器（`claude-agent-acp`、`codex-acp`、`pi-acp`）接入。Radian 不在列表里：编辑器用户是 Radian 现在完全触达不到的人群。
- `radian serve` 与 `radian mcp` 两个子命令已按 ADR 0032 预留、尚未实现（`cli.rs` 的 `reserved`）；ADR 0043 保留了控制口但没有 server 模式。

## 方向（研究报告建议，owner 未定）

2026-10-20 对协议页核实的事实：客户端把 agent 当子进程起；`session/new` 带 `cwd` 与一组 `mcpServers`，agent 必须连上客户端给的 stdio MCP server；一个连接可开多个 session；回合内只能 `session/cancel`，不能再发 prompt；审批是 agent 调客户端的 `session/request_permission`，选项 `allow_once`/`allow_always`/`reject_once`/`reject_always`，取消时客户端用 `cancelled` 答掉挂起的审批；客户端可选暴露 `fs/read_text_file`、`fs/write_text_file`、`terminal/*`；`loadSession` 可选，要用 `session/update` 回放历史。

分析（未定）：外部适配器要把 `request_permission` 接通，只能经控制口或 `exec --output jsonl`，两者今天都不收审批答复（ADR 0043），等于要在控制口开口子；内建子命令直接用宿主的 Prompter（ADR 0024 Q1），成本反而更低。多 session 与 ADR 0043 单会话冲突，先查 Zed 是否每个线程起一个新进程。信任门可以用 `request_permission` 的四个选项表达，`allow_always` 落 `trust.json`；`ask_user` 在 ACP 宿主不注册（ADR 0024 Q5）。

- 这不是格式兼容，是一套新的宿主面：审批（ADR 0024 的 `ask_user` 与权限询问）映射到 ACP 的 `request_permission`；用户输入与 steer（ADR 0011）映射到 `session/prompt`；live 事件（ADR 0010）映射到 `session/update`；durable 事件照旧只进 journal。
- 报告建议**先像 pi 那样做一个外部适配器**验证需求，不进主二进制；需求坐实后再决定是否成为 `radian acp`（或占用预留的 `serve`）。
- 不改 `agent-core`：ACP 服务端是 `radian-host` 之上的又一个宿主，与 TUI、`exec` 并列（ADR 0023 的分层本来就是为了加前端）。

## 待决问题

- 外部适配器还是内建子命令；若内建，用预留的 `serve` 还是新名 `acp`（ADR 0032 要改）。
- 多会话：ACP 客户端会在一个进程里开多个 session，而 ADR 0043 是单会话 radian；是每个 session 起一个 radian 进程（适配器方案天然如此），还是宿主内多会话。
- 沙箱与信任门在编辑器里怎么问：ADR 0031 的启动期 modal 没有 TUI 可用，要经 `request_permission` 还是按非交互处理（缺省不信任）。
- 收益大小取决于用户群，定优先级；报告估计 1.5–2.5 周。

## 验收

- Zed（或任一 ACP 客户端）能开一个 radian 会话、发 prompt、看到流式输出与工具调用。
- 需要审批的工具调用在编辑器里弹 `request_permission`，拒绝后工具结果里有原因。
- 中途输入经 inbox 进入下一轮，journal 与 TUI 会话可回放一致。
- 落地时同分支写新 ADR、改 0032（docs-only commit）并删掉本文件。
