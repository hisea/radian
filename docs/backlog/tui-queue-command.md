# `/queue <消息>`：回合进行中把一条消息排到下一回合，并让队列可见

- 状态：decided＼�第一档）
- 涉及 ADR：0011（inbox 四个操作与 lane）、0027（footer 组件）、0023（Action → Effect）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §2.3、§3 第一档

## 背景

回合进行中按 Enter 是 `steer`（安全点插入当前回合）。inbox 早就有 `followup` lane，但 TUI 没有任何入口在回合进行中投一条 `followup`，用户想「等这轮做完再说下一件事」只能干等。opencode（缺省 steer、可切 queue 并有面板）、omp（`/queue`）、Claude Code（Enter 即排队，Ctrl-Enter 立刻送）、Codex（斜杠命令按 Tab 排队）都有这个入口。

## 决定（已定）

- `/queue <text>`：无论回合是否进行中，都作为 `Inbox::followup`（`source = user`）入队；空闲时等同于直接发送。Enter 在回合进行中仍是 `steer`，不改。
- 队列可见：footer 增加一个 `queued` 贡献项，显示 TUI 自己投出、runtime 还没 admit 的 followup 条数；为 0 时不显示。被 admit 的消息按普通用户消息进 transcript，不另画「待发送」行。
- 本轮**不做**取回/编辑/重排：那需要 inbox 多一个「删除指定条目」操作，是 ADR 0011 L1 的变更，留给第二档讨论。
- `/queue` 没有参数是用法提示；`//queue …` 仍是普通消息（现有转义规则不变）。

## 验收

- 回合进行中 `/queue a` 再 `/queue b`：当前回合不受影响，结束后 a、b 各开一轮，顺序不变（ADR 0011 L4 FIFO）。
- 空闲时 `/queue a` 立刻开一轮。
- footer 在两条待发时显示 2，admit 一条后显示 1，清零后消失。
- 现有「Enter 进行中 = steer」的 pty 测试仍绿；`/help` 列出 `/queue`。
