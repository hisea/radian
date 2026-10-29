# hooks：`PermissionRequest` 让脚本回答 ask

- 状态：decided（可以派发；排在 `hooks-stop-and-feedback.md` 之后，见那一条的顺序说明）
- 涉及 ADR：0045（§1 事件子集、§2 `allow` 拒收的措辞、§4 hook 排在权限之后）、0012（P1 hook 不扩权、P2/P3 决定顺序）、0043（1a.7 子进程的 ask 按拒绝处理）、0048（arc worker 是 headless `radian exec`）、0024（结构化用户交互）、0017（§4 hook 错误失败关闭）、0046（import-claude）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页 `PermissionRequest` 一节；`subagent-ask-forwarding.md` 与 `arc-serve.md` 第 4 条（通知钩子）的同一需求

## 背景

headless 的 `radian exec` 没人回答 ask：0043 1a.7 规定子进程只在父是 auto 时拿 `--yes`，否则每个 ask 按拒绝处理；arc 的 worker 同样。来源不是人的回合（`Stop` 续跑、后台通知唤醒的回合，`InputSource::System`）在 TUI 里也一律拒绝：`radian-permissions` 的 `PermissionHooks::decide` 在调 approver 之前就拒了。

Claude Code 的 `PermissionRequest` hook 在**即将弹权限提示**时触发，脚本返回 `decision: {behavior: "allow" | "deny", …}`；非交互会话里没有 hook 作决定就拒绝，这正是 radian 现在的行为。

这个事件和 0045 §4 "hook 只收窄"不冲突：它只在权限层已经判为 `ask` 之后出现，回答的是本来要问人的问题，不把 `deny` 变成 `allow`。

## 决定（已定）

字段名沿用 Claude Code（owner 2026-10-29）。

### 1. 触发点

- 权限层对一次调用判出 `ask`，且本会话的 session grant 没有覆盖它之后、问人之前（approver 之前，也在"非人来源直接拒绝"之前）。`deny` 规则命中的调用不触发（权限层先拒，0045 §4）；`allow` 与 auto 审批下变成 allow 的调用不触发。
- hook 给出决定：按决定办，不弹提示。没给决定：走今天的路（人的回合问 approver：TUI 弹窗、`exec` 的终端提示；非人来源或无终端：拒绝）。
- 所有来源都触发，这正是它的用途：`exec -p`、arc worker、`Stop` 续跑回合里的 ask 都可以由脚本回答。
- `matcher` 与 `PreToolUse` 相同：Radian 工具名，锚定整名。

### 2. stdin

`hook_event_name: "PermissionRequest"`、`tool_name`、`tool_input`、`cwd`、`session_id`。radian 没有文本形式的权限规则语法（规则是 `radian-permissions` 里的类型），所以**没有** Claude 的 `permission_suggestions` 字段，手册写明。

### 3. 返回

- exit 0 加 JSON `hookSpecificOutput.decision`：
  - `behavior`：`"allow"` 或 `"deny"`，其他值或缺失按"没给决定"处理并警告一行。
  - `message`（字符串，可选）：`deny` 时写进工具的错误结果，原因码 `denied`；`allow` 时忽略。
  - `updatedPermissions`（数组，可选）：只认一种项 `{ "type": "addRules", "destination": "session", "behavior": "allow" }`，效果与 TUI 审批的"本会话允许"相同：按今天 `AllowForSession` 的规则给这次调用记一个 session grant（精确键或 shell 前缀），本会话内同类调用不再 ask，会话结束不留痕。项里的 `rules` 不解析（没有文本规则语法）。其他 `type`、`destination` 或 `behavior` 忽略，警告一行。
  - `updatedInput`、`interrupt`：第一版不接，忽略并警告一行。
- exit 0 且 stdout 为空或没有 `decision`：没给决定。
- exit 2：`deny`，stderr 为原因（与 `PreToolUse` 的 exit 2 同口径）。
- 其他退出码、超时（默认 60 s）、非 JSON：按 0017 §4 失败关闭，等于 `deny`（原因码 `hook_error`），不再回落去问人。

### 4. 两层

项目层先、用户层后（0045 §1）。任一层 `deny` 立即拒绝，后面的 hook 不再跑（拒绝只收窄）。没有 `deny` 且至少一层 `allow`：允许。都没给决定：走 §1 的今天的路。同一条命令两层都写只跑一次（0045 §1 第 3 条）。`updatedPermissions` 只取最终给出 `allow` 的那些层的项。

### 5. ADR 措辞

0045 §2 的"`allow`/`ask` 永远拒收"改成"`PreToolUse` 上的 `allow`/`ask` 拒收"，并写明 `PermissionRequest` 的 `allow` 只回答权限层已经判为 `ask` 的调用。0045 §1 的事件表加上 `PermissionRequest`；§4 写明它的位置在权限层的 ask 分支里。

### 6. 不改的

`subagent-ask-forwarding.md` 不受影响，仍是 idea：hook 命令可以自己去问父会话，那是脚本的事。`arc-serve.md` 第 4 条的手机审批也只是一条 hook 命令，radian 不内置。

## 验收

- `exec -p` 里一个 `shell` 调用触发 ask，配置的 hook 返回 `allow`，调用执行；返回 `deny` 时工具结果写明被拒与 `message`；hook 超时、非零非 2 退出、非 JSON 都按拒绝，且不弹提示。
- TUI 里 hook 返回决定时不弹提示；hook 无输出时照常弹；`Stop` 续跑回合（`System` 来源）里 hook 返回 `allow` 时调用执行（今天会被拒）。
- `deny` 规则命中的调用不触发该事件；`allow` 规则命中的调用不触发。
- 两层：项目层 `allow`、用户层 `deny` → 拒绝；项目层 `deny` 时用户层不运行；一层 `allow` 另一层无输出 → 允许。
- `addRules` 到 `session` 后同类调用本会话不再 ask，也不再跑这个 hook；其他 `destination` 警告一行且不记 grant。
- import-claude 把 Claude 配置里的 `PermissionRequest` 条目带过来，matcher 按 0046 翻译工具名。
- `docs/user/hooks.md` 新增事件的 stdin、返回字段、每个取值、默认行为与两层规则；ADR 0045 §1、§2、§4 在同分支的 docs-only commit 里改成新快照。
