# subagent 的 ask 转回父会话

- 状态：idea
- 涉及 ADR：0043（1a.7、§7）、0010、0024
- 来源：`research/2026-10-18-area-deep-dive/4-architecture.md` §6 ②；待评估

## 背景

子进程只在父是 auto 时拿到 `--yes`，否则它的每个 `ask` 都按拒绝处理（0043 1a.7）。缺省 `full + OnAsk` 下，`general` 子会话碰到 shell 就被拒，一次模型调用白费。Claude Code 的后台 subagent 把权限请求转到主会话里问。

4-architecture 只写了"修订 1a.7"，实际缺的更多：

- 没有 durable 的 `PermissionRequested` 事件（0043 §7 明说 `attach` 因此没有"等审批"行）；
- `control` 拒绝审批答复（0043 §8）；
- 转发要跨 0010 的 durable/live 划分和 0024 的结构化交互。

## 依赖

先做 `sandbox-auto-allow-shell.md`：沙箱内放行之后，子会话剩下要问的只有越界和 MCP，转发量才小到值得做。

## 待决问题

- 子会话的 ask 走什么通道回父：控制 socket 加一种审批请求，还是子会话 journal 里的 durable 事件由父 tail。
- 父在 TUI 里怎么显示"子会话 X 请求 Y"，拒绝后子会话的结论怎么写。
- 顺带测两件没测过的：每次 subagent 调用的启动时间，`general` 子会话重起父的 MCP server 的开销（`explore` 已不带 `--mcp-config`）。

## 成本

中高。

## 验收

workspace 档、非 auto 模式下，`general` 子会话里的越界请求出现在父会话的审批里，父同意后子会话继续；父拒绝时子会话结论写明被拒了什么；`attach` 能看到"等审批"行。
