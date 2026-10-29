# hooks：其余三个生命周期事件，按用例逐个加

- 状态：idea（`StopFailure` 与 `Notification` 已拆到 `hooks-stopfailure-notification.md`，decided）
- 涉及 ADR：0045（§1 "no more events exist"）、0011（inbox 认领点、L2 来源、L5 空闲）、0036（基础提示词与上下文文件）、0018（自动压缩）、0021（memory）、0034（向 herdr 上报状态）、0017（模型错误结束回合）、0013（默认无遥测）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页各事件一节；radian 三事件子集的对照

## 背景

Claude Code 有 35 个事件，radian 有意只做三个（0045 §1）。多数差距是我们不要的（子 agent、任务表、worktree、配置变化、文件 watcher、UI 层事件：radian 的对应物在 arc 或根本不存在）。剩下五个有 radian 内的真实用例；`StopFailure` 与 `Notification` 已决定（见 `hooks-stopfailure-notification.md`），其余三个还没人要，记在这里，谁先有用例谁先做。任何一个落地都要先改 0045 §1 那句话。

| 事件 | 用例 | 接入点 | 顾虑 |
|---|---|---|---|
| `UserPromptSubmit` | Claude Code 最常用的 hook 模式：按策略拦下提示词（exit 2）、注入 git 状态与日期（`additionalContext`） | 0011 的 inbox 认领处，只对 `User` 来源 | 每回合变动的注入必须放在 user message 末尾，不动 system prompt，否则破缓存前缀（`cache-efficiency.md`）；不允许改写提示词（Claude 也不允许） |
| `SessionStart` | 每会话一次的动态上下文（分支、环境变量、上次留的状态），与 AGENTS.md 的静态内容互补；matcher 分 `startup` / `resume` / `fork` | 会话建立后、第一回合前 | 和 0036 的上下文文件重叠；arc worker 的 setup 由 wrapper 做，所以不急 |
| `PreCompact` / `PostCompact` | 压缩前让项目脚本落盘状态（exit 2 可阻止一次压缩）；压缩后把摘要（`compact_summary`）交给脚本写进 memory 或外部 | 0018 的压缩路径两端 | 阻止由上下文溢出触发的压缩会让本次请求失败（Claude 的说法一致）；与 `compaction-threshold.md`、`overflow-three-stage.md` 同一片区域，一起考虑 |

## 待决问题

- `UserPromptSubmit` 的 stdin 是否带 `/loop`、`/goal` 续跑消息（Claude 对定时任务与子 agent 报告也触发），还是只对人打的提示词。
- `SessionStart` 的 `additionalContext` 进哪里：第一条 user message 之前的 `System` 消息（0011 L2），还是追加到上下文文件那一段。
- `PostCompact` 的摘要交给脚本后是否允许脚本改摘要（Claude 不允许）。
- 每个事件的 matcher 闭集、stdin 字段、超时默认值。

## 验收

- 每个落地的事件：`docs/user/hooks.md` 有一节写 stdin 字段、matcher 全部取值、能否阻止、超时默认；配置 drift 测试读到 matcher 闭集；ADR 0045 §1 在同分支的 docs-only commit 里列出新事件。
- import-claude（0046）把对应事件的 Claude 条目带过来，其余仍按子集外丢弃并报告。
- 未落地的事件名在配置里出现仍按今天的规则跳过并警告一行。
