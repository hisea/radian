# `/copy`：复制最近一条 assistant 消息

- 状态：decided＼�第一档）
- 涉及 ADR：0027（`clipboard` 配置与 osc52）、0028（transcript 内容）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第一档

## 背景

Codex、omp、pi 都有 `/copy`。radian 只能用鼠标选区复制，长回复要滚动选很多屏。TUI 已经有 `clipboard = osc52` 的复制路径。

## 决定（已定）

- `/copy`：复制最近一条**已提交**的 assistant 消息的 markdown 源文本（不是渲染结果，不含 reasoning），走配置的剪贴板后端；流式进行中复制的是上一条已提交的。
- `/copy code`：只复制该消息里最后一个围栏代码块的内容（不含围栏行）。
- 成功后通知「copied N chars」；`clipboard = none` 或平台不支持时通知说明原因。
- 没有 assistant 消息时通知「nothing to copy」。

## 验收

- 单测：选中的是最近一条已提交 assistant 消息；`code` 取最后一个代码块、去围栏；无代码块时通知说明。
- 流式进行中 `/copy` 不取在飞文本。
