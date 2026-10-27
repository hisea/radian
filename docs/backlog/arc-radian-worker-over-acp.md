# arc 的 radian worker 切到 ACP

- 状态：decided（owner 2026-10-27）；排在 `arc-acp-worker.md` 与 `acp-server.md` 第一阶段之后
- 涉及 ADR：0048（§3 `start.json` 的 `worker` 记录、§7 worker 的起法、§9 取消、§12 成本）、0043
- 来源：owner 2026-10-27："arc 可以用 ACP 调用 radian"
- 依赖：`arc-acp-worker.md`（ACP 客户端）、`acp-server.md` 第一阶段（`radian acp`）

## 背景

两条落地后 arc 有三种 worker 起法：`radian exec`（wrapper 拼 flag、读 jsonl stdout）、ACP（任意 agent）、`command`（黑盒）。前两种对 radian 做的是同一件事，只是协议不同；留两条路就要维护两种 `result.jsonl` 形状、两种审批（`--yes` 对 `request_permission`）、两种取消。

## 决定（已定）

- `Radian` 起法改为经 ACP 起 `radian acp`：wrapper 把今天拼给 `radian exec` 的那串 flag（`--profile workspace`、`--workspace`、`--session-id` / `--resume`、`--deny-read ...`、flow 的 `exec_args`）原样拼给 `radian acp`，worktree 走 `session/new` 的 cwd；`-p -`、`--output jsonl`、`--yes` 不再传。
- 配置不变：`flows.<name>.exec_args` 与 `worker.exec_args` 仍是"给 radian 的额外参数"，用户不用改 `config.toml`；`flows.<name>.acp` 仍是"起别的 agent"。`start.json` 的 `worker` 记录 `{"kind": "radian", ...}` 不变，`kind` 说的是起谁，不是协议。
- 成本从 `_meta.radian.cost` 读，与今天读 `Result.cost` 等价；`Result.text` 从 `_meta.radian.text` 读。radian worker 的预算行为不变。
- `rework_session = "resume"` 对 radian worker 走 `session/load`；`arc-acp-worker.md` 第 4 条"v0 只 fresh"只对第三方 `acp` flow 成立。
- 审批对 radian worker 也走 `request_permission`，用 `arc-acp-worker.md` 第 2 条的缺省（worktree 内允许、外拒绝）；`--yes` 在 worker 里不再使用。
- gate 的 review session 与 `radian exec -p` 的 headless 用法不动（0048 §10、0032）。
- 落地后 wrapper 里的 `radian exec` 起法删除，ADR 0048 §7 只剩两种：ACP 与 `command`。

## 验收

- 与今天 `radian exec` worker 的每一条验收等价：交付检查、push、记录、`attach`、成本、预算、`resume`、`cancel`。
- `.radian/arc/config.toml` 不改一行，现有仓库的 task 照常跑完。
- 落地时同分支改 0048 §3、§7、§9（docs-only commit），手册 `docs/user/arc/worker.md` 改"worker 的起法"一节，并删掉本文件。
