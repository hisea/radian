# 后台 job 的完成通知写入失败时，唤醒静默丢失

- 状态：idea（bug，lead 2026-10-20 发现；定修法）
- 涉及 ADR：0037（决定 5：完成通知与唤醒）、0001（journal 写入失败的语义）、0011（inbox）、0034（herdr 状态）
- 来源：2026-10-20 A/B/C 测试 ab3，radian 会话 `18e054a6b1ec15a8-cd59`

## 背景（已核实）

- `crates/radian-host/src/job_notify.rs` 处理 `JobEvent::Settled`：先写 `BackgroundJobSettled`（`let _ =`，失败忽略），再按会话是否空闲选 `inject` / `followup` / `inject_degraded`。空闲时是 `if self.inbox.followup(text, source).await.is_ok() { … }`：`followup` 返回 `Err` 时**什么都不做**，没有重试、没有可见提示、herdr 仍报 `idle`。
- ab3 里 radian 把 clippy 放进后台后按设计结束了这一轮，等完成通知唤醒。journal 里 20:09:09Z 有 `BackgroundJobSettled { state: "exited 1" }`，之后再没有 `InboxEnqueued` / `TurnStarted`；会话一直空闲，直到 lead 7 分钟后手动发消息。
- 同一时刻机器磁盘写满（ENOSPC，pi 和 lead 的进程都报了），所以 `followup` 的 durable 写入失败**很可能**是原因；`followup` 返回的具体错误没有被记录，未直接核实。

## 待决方向

- (a) `followup` 失败时退回 `inject`（不唤醒，但通知不丢）并发一条与唤醒上限同形的可见提示（TUI 一行、herdr `blocked`）。
- (b) 有界重试几次再走 (a)。
- (c) 至少把失败原因写进 stderr / 诊断日志，让下次能直接看到错误。
- 与 `wake-cap-unattended` 落地的"到顶可见"同一原则：等待中的会话不能静默停住。

## 验收（定了方向后）

- 一个让 `followup` 返回 `Err` 的测试 inbox：settle 之后会话不再静默空闲（按所选方向：可见提示 / 重试后唤醒）。
- 变异：恢复成"失败就什么都不做"，测试必须失败。
