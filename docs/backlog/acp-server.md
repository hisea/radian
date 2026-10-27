# ACP 服务端 `radian acp`（第二阶段：编辑器）

- 状态：idea（第二阶段：编辑器）；第一阶段已落地，见 ADR 0049 与手册 `radian acp` 页
- 涉及 ADR：0049（`radian acp` 的全部决定）、0024（审批与提问映射到 `request_permission`）、0031（信任门）、0023（宿主面分层）
- 来源：`research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.6、§6 ⑥；owner 2026-10-27："arc 可以用 ACP 调用 radian"
- 关系：arc 当 ACP 客户端的部分已落地（ADR 0048 第 7 节；手册 `docs/user/arc/files.md` 的 `flows.<name>.acp` 行与 `docs/user/arc/worker.md` 的「An ACP agent as the worker」），它是第一阶段（给 arc 用的服务端）的第一个用户，arc 的 radian worker 也已经切到本条（`radian acp`）。本条只剩编辑器场景。

## 第一阶段（已落地）

`radian acp` 子命令、与 `radian exec` 共用的参数、一个进程一个 session、`session/new` 与 `session/load`（按 id 打开同一个 journal 并回放 transcript，之后继续同一个 journal）、`session/prompt` 与 `stopReason`、live 事件到 `session/update`、`_meta.radian.*`、审批与 `ask_user` 的 `request_permission` 映射、非交互信任门、stdio `mcpServers`，以及不调 `fs/*`/`terminal/*`。决定与验收见 ADR 0049，用法见手册 `radian acp` 页；本条不再重复。

## 背景

- ACP（Agent Client Protocol，agentclientprotocol.com）：客户端把 agent 当 stdio 子进程起，JSON-RPC。`session/new` 带 `cwd` 与一组 `mcpServers`；`session/load` 按 id 打开已有会话并用 `session/update` 回放历史；一个连接可开多个 session；回合内只能 `session/cancel`；审批是 agent 调客户端的 `session/request_permission`；客户端可选暴露 `fs/read_text_file`、`fs/write_text_file`、`terminal/*`；协议没有美元成本字段，但允许 `_meta` 扩展字段。（2026-10-20 与 2026-10-27 对协议页核实。）
- 列表上 opencode 原生实现了服务端；Claude Code、Codex、Gemini CLI 各有适配器。radian 不在列表里。
- 编辑器用户不是 radian 现在的人群，所以第二阶段没有排期；第一阶段的动力是 arc。

## 待决（第二阶段：编辑器，idea）

- 多 session：Zed 一个连接开多个线程时是每线程起一个 `radian acp` 进程，还是宿主内多会话。先查 Zed 的行为。
- 信任门在编辑器里怎么问：经 `request_permission` 还是保持非交互。
- 要不要调客户端的 `fs/*` 以显示编辑器未保存的缓冲区内容。
- 上 agentclientprotocol.com 的列表。
