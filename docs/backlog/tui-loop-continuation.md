# `/loop`：续跑引擎第一、二步（轮末重投 + 可选间隔）

- 状态：decided＼�三维引擎分三步，续跑轮次用 `User` 来源加 meta 标记）
- 涉及 ADR：0011（`followup` lane、L5 空闲唤醒、L2 来源）、0005（宿主消息 meta）、0037（唤醒速率闸门不计这里）、0027（footer 贡献项）、0023（Action → Effect）、0017（模型错误结束回合）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §2.2；本条与 `goal-mode.md` 共用一条路径，后者是第三步

## 背景

三家的 `/loop` 语义不同：omp 是「每轮结束原样重投」的开关，pi-loop 与 Claude Code 是定时器，Codex/omp 的 `/goal` 是持久目标。radian 不做三套，而是 driver 轮末一个决策点、三个独立维度：**投什么**（重投用户 prompt，或宿主生成的续跑句）、**何时投**（轮末立刻，或等间隔）、**何时停**（次数、时长、显式完成）。本条做前两步，`goal-mode.md` 在同一路径上加第三种输入与 `goal` 工具。

## 决定（已定）

**语法**：`/loop [--every <dur>] [--times N] [--for <dur>] [prompt]`，flag 在 prompt 之前；时长单位 `s`/`m`/`h`。不带任何参数的 `/loop` 是开关：开着就关掉。限次和间隔都是时长，所以不接受裸数字或裸时长，必须带 flag，避免 omp（限时）与 pi-loop（间隔）的歧义。

**投什么**：开启后下一条用户 prompt（或命令行里给的 prompt）成为循环 prompt；每次迭代原样重投为 `Inbox::followup`，`source = User`，消息 meta 带 `continuation = "loop"` 与 `iteration = n`（ADR 0005）。用 `User` 而不是 `Scheduler` 的原因已核实：非 `User` 回合不认本会话的审批授权、不做记忆注入，workspace 档下每轮都会重新弹审批。审计信息在 meta 里，journal 能看出哪些轮是机器投的。

**何时投**：没有 `--every` 时本轮一结束就投（走 0011 L5 的空闲唤醒路径）。有 `--every D` 时，要同时满足「本轮已结束」和「距上一次迭代**开始**已过 D」；回合进行中到点不打断，顺延到轮末；多次到点合并成一次。等待期间 footer 显示 `loop · next in 3m`，herdr 状态按 ADR 0034 现有标签，不新增。

**何时停**：`--times N` 或 `--for D` 任一到即停。两者都没给时强制缺省上限：20 次或 1 小时先到者为准（常量写进 ADR，落地后按 dogfood 调）。到顶时 TUI 通知、loop 关闭。

**Esc**：循环发起的回合进行中按 Esc 取消该回合并**暂停**循环（丢掉循环 prompt，模式仍开着，omp 形状）；用户下一条 prompt 成为新的循环 prompt 并恢复迭代。`/loop` 再按一次彻底关闭。

**模型错误**（ADR 0017 `turn failed`）：暂停循环并通知，不自动重试，避免对着故障的 provider 连发；用户用 `/retry` 或新 prompt 恢复。

**边界**：不计入 0037 的唤醒速率闸门，靠自己的上限兜底；状态只在进程内，退出 TUI 即结束，不写会话；只有 TUI 有，`radian exec` 没有；回合进行中 `/loop … prompt` 允许，prompt 按 `/queue` 的方式排到轮末。footer 贡献项 `loop 3/20`、`loop every 5m · 3/20`。

## 验收

- driver 测试：开启后同一 prompt 连投三轮，每轮 `InboxEnqueued.source = User` 且 meta 的 `iteration` 递增；`--times 2` 恰好两轮后停并有通知。
- `--every 5m`：回合 1 分钟结束时不投，到 5 分钟投；回合跑了 7 分钟时轮末立刻投（测试用可注入时钟，不 sleep）。
- 缺省上限：无 flag 时第 21 次不投。
- Esc 后循环暂停，下一条 prompt 成为新循环 prompt；模型错误后暂停且不重投。
- 唤醒速率闸门的测试不受循环影响（循环 20 轮后闸门计数仍为 0）。
- pty：footer 显示与 `/loop` 开关；`/help` 列出。
