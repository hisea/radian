# ACP 服务端 `radian acp`

- 状态：in-progress（第一阶段除 `session/load` 已落地，见 ADR 0049；`session/load` 待补）；idea（第二阶段：编辑器）
- 涉及 ADR：新 ADR；0032（一个二进制、保留的子命令）、0009（全屏 TUI 之外的宿主面）、0024（审批与提问映射到 `request_permission`）、0011（`session/prompt` 走 inbox）、0010（`session/update` 只发 live 事件）、0043（单会话、控制口）、0031（信任门）、0007（usage 与 cost）、0022（不 fork 会话）
- 来源：`research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.6、§6 ⑥；owner 2026-10-27："arc 可以用 ACP 调用 radian"
- 关系：`arc-acp-worker.md`（arc 当 ACP 客户端）是本条的第一个用户，先做；`arc-radian-worker-over-acp.md` 在两条之后，把 arc 的 radian worker 切到本条。

## 落地状态

第一阶段除 `session/load` 外已落地为 ADR 0049：`radian acp` 子命令、与 `radian exec` 共用的参数、一个进程一个 session、`session/prompt` 与 `stopReason`、live 事件到 `session/update`、`_meta.radian.*`、审批与 `ask_user` 的 `request_permission` 映射、非交互信任门、stdio `mcpServers`，以及不调 `fs/*`/`terminal/*`。`initialize` 目前回 `loadSession: false`，`session/load` 被拒绝并说明不支持；按 id 打开已有会话并用 `session/update` 回放 journal 的部分仍留在本条。

## 背景

- ACP（Agent Client Protocol，agentclientprotocol.com）：客户端把 agent 当 stdio 子进程起，JSON-RPC。`session/new` 带 `cwd` 与一组 `mcpServers`，agent 必须连上客户端给的 stdio MCP server；一个连接可开多个 session；回合内只能 `session/cancel`，不能再发 prompt；审批是 agent 调客户端的 `session/request_permission`，选项 `allow_once` / `allow_always` / `reject_once` / `reject_always`，取消时客户端用 `cancelled` 答掉挂起的审批；`session/prompt` 的回应带 `stopReason`（`end_turn`、`max_tokens`、`max_turn_requests`、`refusal`、`cancelled`）；`session/update` 的种类有 `agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update`、`plan`、`usage_update`；客户端可选暴露 `fs/read_text_file`、`fs/write_text_file`、`terminal/*`；`session/load` 可选，要用 `session/update` 回放历史；协议没有美元成本字段，但允许 `_meta` 扩展字段。（2026-10-20 与 2026-10-27 对协议页核实。）
- 列表上 opencode 原生实现了服务端；Claude Code、Codex、Gemini CLI 各有适配器。radian 不在列表里。
- `radian serve` 与 `radian mcp` 两个子命令按 ADR 0032 预留；`radian arc mcp` 已占用 `mcp` 的 arc 子树。ADR 0043 保留了控制口但没有 server 模式。
- 原先暂定不做，理由是编辑器用户不是 radian 现在的人群。2026-10-27 owner 定了：arc 自己用 ACP 起 radian worker，本条因此有了第一个用户，范围按 arc 的需要定。

## 决定（第一阶段，已定）

- **内建子命令 `radian acp`**，不做外部适配器：外部适配器要把审批接进控制口，而控制口不收审批答复（0043），内建直接用宿主的 Prompter（0024 Q1）反而便宜。它是 `radian-host` 之上与 TUI、`radian exec` 并列的第三个宿主面，不改 `radian-core`（0023 的分层本来就是为了加前端）。ADR 0032 的子命令表加 `acp`。
- **flag 与 `radian exec` 同一套**：`--profile`、`--workspace`、`--session-id`、`--resume`、`--mcp-config`、`--model`、`--provider`、`--system-file` 等照收，语义不变，arc 与 `subagent` 工具设的隐藏内部 flag（`--tools`、`--deny-read`）也照收；没有 `-p`、`--output`、`--yes`（输入来自 `session/prompt`，输出走协议，审批走协议）。arc 的 wrapper 把今天拼给 `radian exec` 的那串原样拼给 `radian acp`。
- **一个进程一个 session**（0043）：第一个 `session/new` 建会话，cwd 必须等于 `--workspace`（或在未给 `--workspace` 时成为它）；第二个 `session/new` 以错误拒绝。`session/load` 待补：按 id 打开已有会话并用 `session/update` 回放 journal 里的 transcript（0006、0022），等价于 `--resume`；在补上之前 `loadSession: false`，`session/load` 返回“不支持”。
- **映射**：`session/prompt` 的内容进 inbox 成为用户消息（0011），回合跑完以 `stopReason` 回应：正常结束 `end_turn`，被 `session/cancel` 中断 `cancelled`，模型以 `max_tokens` 停 `max_tokens`，模型或钩子错误结束回合（0017）按 `refusal` 回应并在 `_meta` 里带错误。live 事件（0010）逐条翻成 `session/update`：文本块 `agent_message_chunk`，推理内容（0014）`agent_thought_chunk`，工具调用 `tool_call` 与 `tool_call_update`（标题、kind、状态、位置、原始输入输出），usage（0007）`usage_update`。durable 事件只进 journal，与 TUI 会话可互相回放。
- **成本走 `_meta`**：`session/prompt` 的回应在 `_meta.radian.cost` 带今天 `radian exec` 的 `Result.cost`（`micros`、`incomplete`、`estimated`），`_meta.radian.text` 带最后一条助手消息全文。读不懂 `_meta` 的客户端忽略它；arc 读到就记成本。
- **审批全走 `request_permission`**：0024 的权限询问与 `ask_user` 都映射到它；`ask_user` 的选项成为 `request_permission` 的选项（0024 Q5 在这个宿主上就是这样注册）。`allow_always` 成为 0012 的 `AllowForSession` 授权：只活在这个进程里，不进 profile、不落盘，`session/load` 之后没有。客户端答 `cancelled` 时工具结果带原因，回合按中断处理（0003）。
- **信任门（0031）按非交互处理**：`radian acp` 与 `radian exec` 一样不弹启动期 modal，未信任目录按不信任加载。编辑器里怎么问留到第二阶段。
- **`mcpServers`**：`session/new` 带来的 stdio MCP server 当作本次会话的 MCP 配置合并进来，与 `--mcp-config` 同级；HTTP 形式的 server 第一阶段拒绝并在错误里说明。
- **不实现的客户端能力**：不调 `fs/*` 与 `terminal/*`，radian 自己读写文件、自己起 shell，沙箱照旧（0016、0020）。

## 待决（第二阶段：编辑器，idea）

- 多 session：Zed 一个连接开多个线程时是每线程起一个 `radian acp` 进程，还是宿主内多会话。先查 Zed 的行为。
- 信任门在编辑器里怎么问：经 `request_permission` 还是保持非交互。
- 要不要调客户端的 `fs/*` 以显示编辑器未保存的缓冲区内容。
- 上 agentclientprotocol.com 的列表。

## 验收

- arc 用 `acp = ["radian", "acp", ...]` 起一个 worker 跑完一个 task：brief 经 `session/prompt` 送达，`session/update` 进 `result.jsonl`，`_meta.radian.cost` 被记为 attempt 成本，`radian arc attach` 仍按 session id 找到 journal。
- `--resume` 等价：`session/load` 打开的会话继续同一个 journal，`journal-selfcheck` 对它的回放通过。
- `radian arc cancel` 经 `session/cancel` 结束回合，回应是 `cancelled`，journal 里是中断前缀（0003）。
- 一个需要审批的工具调用经 `request_permission` 问到客户端，拒绝后工具结果里有原因。
- 第二个 `session/new` 被拒绝，错误说明一个进程一个 session。
- 落地时同分支写新 ADR、改 0032 与 0024（docs-only commit），手册加 `radian acp` 一页（每个 flag 指向 `radian exec` 的同名条目），并删掉本文件。
