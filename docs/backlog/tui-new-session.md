# `/new`：原地开一个新会话

- 状态：decided＼�第一档）
- 涉及 ADR：0022（fork 不切换）、0023（全局 chrome 与 `SessionView` 的边界）、0043 §3（沙箱按项目保存）、0026（模型切换状态）、0037（后台 job）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第一档

## 背景

opencode、Codex、omp、pi 都有 `/new`。radian 今天 `/sessions` 和 `/fork` 都不原地切换，想开新会话只能退出再启动。TUI 的架构已经把全局 chrome（主题、键位）和单会话状态（`SessionView`）分开（ADR 0023），缺的是「结束当前会话、起一个新的、换掉 `SessionView`」这条路径。

## 决定（已定）

- `/new` 只在空闲时可用；回合进行中拒绝（和 `/fork` 一样）。有运行中的后台 job 时拒绝，通知指向 `/jobs kill`。
- 干净结束当前会话（释放写锁、停 driver），在**同一 workspace** 起一个新会话；旧会话仍可 `--resume`，通知里给出旧 id。
- 新会话沿用**当前生效**的模型/provider、thinking、access 档、auto 开关（即用户在本会话里切过的值，不是配置文件里的缺省），沙箱档按 ADR 0043 §3 的项目保存值。
- transcript、footer 计数、`/btw` 的进程内历史全部重置；主题、键位、窗口状态不变。
- 不加 `/clear` 别名：omp 的 `/clear` 是「原地清上下文、保留会话」，和 Codex/opencode 的别名语义冲突，先不碰。

## 验收

- pty 测试：发一轮、`/new`、再发一轮：两个会话文件，第二个的 `SessionCreated` 沿用切换后的模型与档位；`/sessions` 列出两者。
- 回合进行中与有后台 job 时 `/new` 被拒绝，会话不变。
- `journal-selfcheck` 对两个会话都绿。
