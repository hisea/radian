# hooks：`async` 后台 hook 接到后台 job 上

- 状态：idea
- 涉及 ADR：0045（§1 明文排除 `async`、§5 `PostToolUse` 同步写入）、0037（后台命令、完成通知、决定 5 唤醒闸门）、0011（`inject` 与 `followup` lane、L2 来源）、0046（import-claude 现在把 `async` 条目丢弃）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页 `Run hooks in the background` 一节；radian dogfood 里 post-edit lint 同步跑拖慢回合的观察

## 背景

0045 §5 规定 `PostToolUse` 同步跑、结果写在该工具结果之后再返回。post-edit lint 和测试这类 hook 因此每次编辑都阻塞回合；重一点的检查（整包测试）没人敢配。

Claude Code 的 `async: true` 让 command hook 在后台跑，模型不等它；进程退出后 stdout JSON 里的 `additionalContext` 在下一回合送达；`asyncRewake: true` 的 hook 退出 2 时立即唤醒空闲会话。async hook 不能阻止或改写任何东西（它要控制的动作已经发生）。

radian 已经有完全对应的机器，不需要新概念：

| Claude Code | radian 对应物 |
|---|---|
| 后台起进程，模型不等 | 0037 的后台 job（`background: true` 那条路） |
| 退出后结果下一回合送达 | 完成通知走 `inject` lane（不唤醒） |
| `asyncRewake` 退出 2 立即唤醒 | 完成通知走 `followup` lane，计一次 wake，过 0037 决定 5 的闸门 |
| 完成通知在会话 idle 时等下一次交互 | 0011 L5 空闲唤醒的既有语义 |

## 待决问题

- 字段形状：沿用 Claude 的 `async: true` 与 `asyncRewake: true` 两个布尔，还是 radian 一个 `async = "inject" | "wake"`。沿用 Claude 字段 import-claude 不用翻译。
- async hook 的 stdout：只读 JSON 里的 `additionalContext`（Claude 的做法），还是像前台 job 一样把 stdout 尾部当通知正文。
- async hook 是否出现在 `/jobs` 与 `job {id,op}` 工具里（模型能 `wait`、`kill` 它吗），还是独立一类后台进程，只在 `/hooks` 页可见。
- 会话结束时还在跑的 async hook：杀掉（Claude `-p` 模式的做法），还是像后台 job 一样随会话退出。
- 哪些事件允许 `async`：只 `PostToolUse` 与 `Stop`，还是 `PreToolUse` 也允许（那时它只能观察，不能否决）。
- 超时：async hook 不受 `timeout` 约束（Claude 的做法）还是沿用 job 的上限。

## 验收

- 配了 `async: true` 的 `PostToolUse` hook，工具结果立即返回，hook 退出后其 `additionalContext` 在下一回合进上下文；同回合连续编辑起多个 hook，各自一条。
- `asyncRewake` 退出 2 在会话空闲时唤醒一个回合，计入 0037 决定 5 的闸门；超闸门时退化为 `inject`，与后台通知同样的三处可见信号。
- async hook 返回的 `permissionDecision`、`updatedInput` 被忽略并警告一行。
- `workspace` 档下 async hook 子进程在同一沙箱内。
- import-claude 不再丢弃 `async` 条目，报告行相应减少；`docs/user/hooks.md` 写明字段、默认值、哪些事件接受；ADR 0045 §1 与 §5 在同分支的 docs-only commit 里改成新快照。
