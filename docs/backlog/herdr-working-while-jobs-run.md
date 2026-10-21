# herdr：后台 job 在跑时报 `working`

- 状态：decided＼�后台 job 在跑时报 `working`，标签 `working · jobs`）
- 涉及 ADR：0034（H2 状态函数、`state_labels`）、0037（后台 job 与完成通知）
- 来源：��herdr 显示空闲，其实还有 job 在跑

## 背景（2026-10-21 在 4bc41044 上核实）

- 状态函数是 `crates/radian-host/src/herdr.rs` ~122 的 `state(turn, pending, trust_modal, wake_waiting)`：审批/信任 → `blocked`；回合在跑 → `working`；唤醒预算用完 → `blocked`；否则 `idle`。不看后台 job。调用点是 `crates/radian-tui/src/cli/herdr.rs` ~32。
- 模型发起后台命令（如 `cargo test`）后结束回合等完成通知是 ADR 0037 要的行为；此时会话没干完，job 结束会叫醒它继续，但 herdr 报 `idle`。
- TUI 已经知道本会话在跑的 job：footer 的 `jobs` 组件读的是 `JobNotifier` 经 live 事件转发的 start/settle 对（`crates/radian-tui/src/footer/components/jobs.rs`）。
- `state_labels` 今天只给 `blocked` 一个更准的字（ADR 0034 第 33 行附近）。

## 决定（已定）

- 状态函数多一个输入：本会话正在跑的后台 job 数。没有回合在跑、但 job 数 > 0 → `working`，`state_labels` 给它 `jobs`（herdr 显示 `working · jobs`）。回合在跑时仍是普通的 `working`，不带标签。
- 优先级不变：审批/信任的 `blocked` 最先；唤醒预算用完时，有 job 在跑也报 `working · jobs`（job 结束的通知会被闸门降级，这一点照 ADR 0037 决定 5 的到顶可见处理，本条不改）。
- 最后一个 job 结束且没有回合时回到 `idle`（或通知开了新回合就是 `working`）。
- 只发 H2/H3 已有字段，`jobs` 只是 `state_labels` 的一个值，不报 job 数、命令或时长。

## 验收

- 纯函数单测：`Idle + jobs>0 → Working(jobs)`；`Running + jobs>0 → Working`（无标签）；`pending>0 + jobs>0 → Blocked`；`jobs=0` 时与今天一致。
- pty（假 herdr socket）：一轮里模型起一个后台命令后结束回合 → 收到 `working` 且标签 `jobs`；命令结束后收到 `working`（通知开的回合）再 `idle`。
- 变异：状态函数忽略 job 数 → 第一条失败；回合在跑时也带 `jobs` 标签 → 第二条失败。
- 落地时同分支改 ADR 0034（docs-only commit），删掉本文件。
