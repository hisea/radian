# arc run：一轮不再等正在跑的 worker

- 状态：idea
- 涉及 ADR：0048（`run` 的一轮、attempt 生命周期、cancel 与 reconcile、预算与 stall）
- 来源：arc 第二个 plan 的复盘

## 背景

`radian arc run` 的一轮按固定顺序走：reconcile → 到期的 retry → verify → 领取并运行 ready 的 task → merge。第 4 步在一个线程作用域里启动每个 attempt 的 `radian arc work` 包装进程，并且**等所有包装进程结束**才往下走。等待期间由 arc 进程监督每个 attempt：wall time、stall、cost 上限都在这里执行。

后果：一轮的长度等于最慢的那个 worker。别的 task 这时就算已经 `review.verifying`、已经可以 merge、或者正等着 lead 给返工，也要等。实测一个 task 在会话里等了 137 分钟，大部分就是在等同一轮里的另外两个 worker（分别跑了 48 和 32 分钟）和它们之后的 gate。concurrency 越高，这个等待越长。

已经有的基础：包装进程是独立进程，自己写 `AttemptFinished`；下一轮的 reconcile 能接上已经结束的 attempt，也能把死掉的 attempt 按 retry 规则处理。`--watch` 已经让 `run` 成为常驻循环。

## 建议

第 4 步只启动包装进程、把 task 置为 `doing.running` 就返回，不等。之后每一轮的 reconcile 负责：

- 接上已写 `AttemptFinished` 的 attempt（今天已有）；
- 对仍在跑的 attempt 执行 wall time、stall、cost 检查，超限就按 ADR 0048 的 cancel 流程处理（今天这部分在等待循环里）；
- concurrency 按 `doing` 状态的 attempt 数算，而不是按本轮启动的个数算。

## 待决问题

1. 监督放在哪里：留在 arc 的 reconcile（每轮查一次，精度等于 `--interval`），还是挪进 `radian arc work` 包装进程自己执行上限（不依赖 `run` 是否在跑）？后者让一次性 `run` 退出后 worker 仍受约束。
2. 不带 `--watch` 的一次性 `run`：启动 worker 后直接退出（worker 继续跑，下次 `run` 再收），还是保留今天「等到安静」的语义？
3. `runner.lock` 释放后 worker 还在跑：第二个 `run` 进来时，怎么确认它看到的 `doing` attempt 是活的（今天 reconcile 已有进程存活检查，`ps` 在 workspace 沙箱里被拒）。

## 验收

- concurrency 2：一个 worker 跑很久时，另一个 task 在它结束前就能完成 verify 和 merge（集成测试用假 worker 控制时长）。
- wall time、stall、cost 上限在新位置照样触发 cancel，现有对应测试全过。
- 手册 `docs/user/arc.md` 的一轮顺序和 `--watch` 段落跟着改。
