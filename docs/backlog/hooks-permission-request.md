# hooks：`PermissionRequest` 让脚本回答 ask

- 状态：idea
- 涉及 ADR：0045（§1 事件子集、§4 hook 排在权限之后）、0012（P1 hook 不扩权、P2/P3 决定顺序）、0043（1a.7 子进程的 ask 按拒绝处理）、0048（arc worker 是 headless `radian exec`）、0024（结构化用户交互）、0011（`System` 来源）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页 `PermissionRequest` 一节；`subagent-ask-forwarding.md` 与 `arc-serve.md` 第 4 条（通知钩子）的同一需求

## 背景

headless 的 `radian exec` 没人回答 ask：0043 1a.7 规定子进程只在父是 auto 时拿 `--yes`，否则每个 ask 按拒绝处理；arc 的 worker 同样。`subagent-ask-forwarding.md` 设想的方案是 durable `PermissionRequested` 事件加 control 口收审批，成本中高。

Claude Code 的 `PermissionRequest` hook 在**即将弹权限提示**时触发，脚本返回 `decision: {behavior: "allow" | "deny", updatedInput?, updatedPermissions?, message?, interrupt?}`；非交互会话里没有 hook 作决定就拒绝，这正是 radian 现在的行为。`updatedPermissions`（`addRules` / `replaceRules` / `removeRules` / `setMode`，`destination` 为 `session` / `localSettings` / `projectSettings` / `userSettings`）顺带解决"记住这次选择"。

这个事件和 0045 §4 "hook 只收窄"不冲突：它只在权限层已经判为 `ask` 之后出现，回答的是本来要问人的问题，不把 `deny` 变成 `allow`。但 0045 §2 "`allow` 永远拒收"的措辞要改成"`PreToolUse` 上的 `allow` 拒收"，并明写 `PermissionRequest` 这一例外。

## 待决问题

- 触发点：权限层返回 `Ask` 之后、交互式提示之前；TUI 里有 hook 回答就不弹，无回答才弹；`exec -p` 里无回答按今天的拒绝处理。
- 返回形状：沿用 Claude 的 `decision` 对象（import-claude 直通），`updatedPermissions` 第一版只接 `addRules` 到 `session`，其他 `destination` 要写配置文件（0008 的 `edit_preserving` 路径），是否纳入。
- stdin：工具名、`tool_input`、权限层给出的建议规则（Claude 的 `permission_suggestions`）；radian 的权限规则语法（`permissions.md`）直接作为建议值。
- 两层（项目、用户）都配了时的顺序：项目先、用户后与 0045 §1 一致；第一个给出 `decision` 的生效，还是 `deny` 优先。
- 与 `subagent-ask-forwarding.md` 的关系：hook 命令可以是"向父会话的 control 口发问并等答复"的脚本，那条 backlog 的 durable 事件方案是否因此降级为 idea。
- 与 `arc-serve.md` 第 4 条的关系：同一个 hook 命令可以推到手机再等回答，超时按拒绝。
- hook 超时与非零退出：按 0017 §4 失败关闭等于拒绝，与今天一致；是否要和 Claude 一样"exit 2 且无 `decision` 对象时流程不变（继续弹提示）"。

## 验收

- `exec -p` 里一个 `shell` 调用触发 ask，配置的 hook 返回 `allow`，调用执行；返回 `deny` 时工具结果写明被拒与 `message`；hook 超时按拒绝。
- TUI 里 hook 返回决定时不弹提示；hook 无输出时照常弹。
- `deny` 规则命中的调用不触发该事件（权限层先拒，hook 看不到，0045 §4）。
- `addRules` 到 `session` 后同类调用本会话不再 ask；会话结束不留痕。
- import-claude 把 Claude 配置里的 `PermissionRequest` 条目带过来；`docs/user/hooks.md` 新增事件的 stdin、返回字段、每个取值、默认行为；ADR 0045 §1、§2、§4 在同分支的 docs-only commit 里改成新快照。
