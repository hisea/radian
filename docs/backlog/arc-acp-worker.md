# arc 的 ACP worker：用 Agent Client Protocol 起第三方 agent

- 状态：idea（方向已定，三个待决问题见下）
- 涉及 ADR：0048（§7 worker 的两种起法、§3 `start.json` 的 `worker` 记录、§9 取消、§12 成本）、0044（`Result` 回传契约）、0043（子进程边界）、0008（配置 schema）
- 来源：owner 2026-10-27（tkrs 仓库的 image flow 用一个 40 行脚本包 `codex exec`，"有 ACP 就简单了"）；`acp-server.md` 的协议事实；agentclientprotocol.com 协议页与 `agentclientprotocol/codex-acp` README（2026-10-27 核对）
- 与 `acp-server.md` 的关系：那条是 radian **当 ACP 服务端**给编辑器用；这条是 arc **当 ACP 客户端**起别人的 agent 当 worker。两条独立，这条小得多，先做这条不影响那条。

## 背景

arc 今天有两种 worker（0048 §7）：`radian exec`，或 flow 的 `command`。`command` 是一个黑盒程序：brief 进 stdin，JSON 行出 stdout，最后一行 `Result` 定胜负。拿它接 Codex 要写一个脚本做五件事：存 brief、在 brief 前加"不要 commit"（Codex 的 `workspace-write` 沙箱写不了 `.git`）、跑 `codex exec --json`、脚本自己在沙箱外 commit、拼 `Result` 行。换成 Claude Code 或 opencode 再写一个。

ACP（Agent Client Protocol）把 agent 当 stdio 子进程，JSON-RPC。客户端做的事：`initialize`，`session/new`（带 `cwd` 与一组 `mcpServers`），`session/prompt`，收 `session/update` 流，答 `session/request_permission`，`session/cancel` 取消。`session/prompt` 的回应带 `stopReason`：`end_turn`、`max_tokens`、`max_turn_requests`、`refusal`、`cancelled`。`session/update` 的种类有 `agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update`、`plan`、`usage_update` 等。客户端必须实现的只有 `session/request_permission`，`fs/*`、`terminal/*` 都是可选能力。协议本身没有美元成本字段。

适配器现状（2026-10-27）：Codex 用 `npx -y @agentclientprotocol/codex-acp`（原 `@zed-industries/codex-acp` 已迁移；新版起 Codex App Server 再翻译），README 列了 approval 与 sandbox mode 配置、`INITIAL_AGENT_MODE`（`read-only` / `workspace-write` / `agent` / `agent-full-access`）、token usage 事件、客户端给的 MCP server；Claude Code、Gemini CLI 各有适配器，opencode 原生。Rust 有官方 crate `agent-client-protocol`（v1 稳定；3.x 带 v2 草案，落地时钉版本）。

## 方向（已定的部分）

- 第三种 worker 起法，与 `Radian`、`Command` 并列：flow 配一个 ACP agent 的命令行，wrapper 自己当 ACP 客户端起它。不是新的 flow 概念（flow 仍只是参数捆绑，见 `arc-flows.md`），也不进 `radian-core`：只改 `radian-arc` 的 wrapper。
- 一次 attempt = 一个 agent 子进程 + 一个 `session/new`（`cwd` 为 worktree）+ 一次 `session/prompt`（内容就是今天 stdin 的那份 brief）。回合结束即 attempt 结束，与 0043 单会话、0048 "worker 没有交互输入"一致。
- 映射到现有记录，不加新文件：`session/update` 逐条写进 `result.jsonl`（TUI 的 conversation tab 因此能显示真正的工具调用行，而不是 Codex 的 stdout 原样）；`stopReason` 为 `end_turn` 记 `Result.outcome = completed`，其余按名字记为失败 outcome，走 0048 §9 的重试表；最后一段 `agent_message_chunk` 拼成 `Result.text`，供下一次 attempt 的 hand-off 读。
- 取消：SIGTERM 之前先发 `session/cancel`，把挂起的 `request_permission` 以 `cancelled` 答掉，再按 `cancel_grace_secs` 处理进程。
- 交付检查、git 控制文件检查、push、记录与 `command` worker 完全一样（0048 §8）。arc 仍只查结果不查过程。
- `start.json` 的 `worker` 记录加第三种 kind，shape 与现有两种同构。
- `mcpServers`：v0 给空数组。arc 的 MCP server 是 lead 用的，worker 不读 backlog（0048 §5）。

## 做了也不会消失的两件事

- **提交。** Codex 的 `workspace-write` 写不了 `.git`，这是 Codex 的事，不是协议的事。脚本的"沙箱外 commit"只能由 arc 接手，否则脚本还得留一半。这是独立的通用小功能，见待决第 3 条。
- **成本。** 协议有 `usage_update`（token 数），没有美元。arc 只有 token，没有价目表就算不出钱；attempt 的成本在 0048 §12 的意义上仍是 unknown，预算仍不生效，`cost_unknown` 照旧。要给钱，得 flow 配单价或复用 radian 的模型目录（0018），另议。

## 待决问题

1. **配置键的名字与形状。** `flows.<name>.acp = ["npx", "-y", "@agentclientprotocol/codex-acp"]`（与 `command` 互斥，同设报错）是最直接的；要不要再给 agent 传环境变量（`INITIAL_AGENT_MODE`、`CODEX_PATH` 之类）用一个 `acp_env` 表，还是让用户自己包一层脚本。
2. **审批策略的缺省值。** `request_permission` 的选项是 `allow_once` / `allow_always` / `reject_once` / `reject_always`。候选缺省：worktree 内的读写与执行一律 `allow_once`，worktree 外一律 `reject_once`，并把每次询问与答复记进 `result.jsonl`。要不要让 flow 覆盖（`acp_permissions = "allow-all" | "worktree" | "deny-all"`），以及 `ARC_DENY_READ` 那些路径怎么表达（协议里客户端看不到 agent 读了什么，除非 agent 走 `fs/read_text_file`）。
3. **"脏则 arc 提交"。** worker 结束后 worktree 有未提交改动时，由 wrapper `git add -A && git commit`，commit message 取 `Result.text` 第一段。这对三种 worker 都有用，但改变了 0048 §8 "脏 worktree 即失败"的规则：是作为 flow 的显式开关（缺省关），还是只对 ACP worker 开。
4. **会话复用。** `[worker] rework_session = "resume"` 对应 ACP 的 `session/load`，但它是可选能力，codex-acp README 没写清楚是否支持。v0 可以只支持 `fresh`，`resume` 配 ACP flow 时报配置错误。
5. **适配器的安装。** `npx -y` 第一次要联网下载，worker 没有网络时失败；是文档里写清"先全局安装"，还是 arc 在 `start` 前做一次可执行性检查（与 `command` 的检查同款）。

## 验收

- tkrs 的 image flow 配置从脚本换成一行 `acp = [...]`，同一个 task 能跑完：brief 送达、Codex 调用 image_gen、改动落到 worktree、attempt 通过交付检查。
- arc TUI 的 conversation tab 显示 ACP 的 `tool_call` / `tool_call_update` 行，与 `radian exec` worker 的显示同一种形状。
- `radian arc cancel` 让 agent 以 `cancelled` 结束，attempt 记录为取消，而不是 `terminated`。
- 同一配置把命令换成 Claude Code 的 ACP 适配器也能跑完一个 task（有一次手工核对即可）。
- 落地时同分支改 0048 §3、§7、§9（docs-only commit），手册 `docs/user/arc/worker.md` 与 `files.md` 加每个新键，并删掉本文件。
