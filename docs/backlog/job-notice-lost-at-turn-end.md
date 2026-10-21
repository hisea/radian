# job 完成通知在回合收尾时丢失唤醒（bug）

- 状态：decided＼�按 lead 的修法做，优先）
- 涉及 ADR：0011（inject 不唤醒；本条加一个例外）、0037 决定 5（完成通知唤醒模型、唤醒速率闸门）
- 来源：2026-10-21 03:49 goal-mode dogfood 会话 `18e06cb8a10837b0-127e1` 卡住 3 分钟

## 背景（2026-10-21 在 4bc41044 上核实）

- `crates/radian-host/src/job_notify.rs` ~213：job 结束时，通知器读 `turn_running`；为真就 `inbox.inject(...)`（`next_step` 车道，`wake: false`，留给当前回合的下一步），为假就 `inbox.followup(...)`（唤醒空闲 driver）。这是先查后做，中间没有锁。
- 回合的最后一次模型回复没有工具调用时，没有「下一步」。driver（`crates/radian-runtime/src/driver.rs` `run_loop` ~446）在回合结束后 `claim_next_turn()`：只取 `next_turn`，或 `next_step` 里 `wake: true` 的条目；`inject` 永不开回合（`inbox.rs` ~515 的文档）。于是在「最后一步之后、`turn_running` 清掉之前」入队的通知，躺在 `next_step` 里，直到别的输入唤醒 driver。
- 复现（journal seq）：2325 最后一条 assistant（无工具调用）03:49:30.726 → 2327 `InboxEnqueued` lane `next_step` wake false 03:49:31.012 → 2329 `TurnFinished` 03:49:31.024 → 之后无 `InputClaimed`；3 分钟后一条用户消息开了 t-3，同时把这条通知一起认领。
- 与 `job-notice-lost-on-write-failure.md` 不同：那条是写入失败；这条是写入成功但没人读。
- driver 已有测试钩子 `commit_delay`（`driver.rs` ~139、~378）：在回合结束与 journal 的 `TurnFinished` 之间插入可控间隔，可以把入队稳定放进这个窗口。

## 决定（已定）

- 新的入队方式：job 完成通知在回合进行中入队时，`next_step` 条目带一个标记「回合结束时未被吸收就开下一回合」。回合中若被某一步吸收（`claim_next_step`），行为与今天的 inject 相同。
- 回合结束后，`claim_next_turn` 的取法：先 `next_turn`，再 `wake: true` 的 `next_step`，再带新标记的最旧 `next_step` 条目。普通 `inject` 仍然永不开回合。
- 唤醒速率闸门（ADR 0037 决定 5）：这样开出的回合**算一次唤醒**，被吸收的不算。闸门在 host，driver 在 runtime；host 用什么信号得知「这条通知开了回合」由实现定，但不能按入队次数预扣（那会把被吸收的也算进去）。闸门已满时，通知照今天的 `inject_degraded` 走，不带新标记。
- ADR 0011 写明这个例外，ADR 0037 决定 5 写明回合收尾时的通知也会唤醒。

## 验收

- driver 测试：用 `commit_delay` 让通知在最后一步之后、`TurnFinished` 之前入队，带新标记 → 回合结束后立刻开下一回合并认领它；普通 `inject` 同样时机 → 不开回合（今天的行为不变）。
- 回合中被吸收的带标记条目 → 回合结束后不再开回合（不重复）。
- job_notify：忙时用新入队方式；闲时仍 `followup`；闸门满时 `inject_degraded`。
- 闸门计数：被吸收 → 不计；开了回合 → 计一次。
- 变异：claim 不看新标记 → 第一条测试失败；被吸收的条目仍开回合 → 第二条失败；闸门把所有忙时通知都计数 → 计数测试失败。
- 落地时同分支改 ADR 0011、0037（docs-only commit），删掉本文件。
