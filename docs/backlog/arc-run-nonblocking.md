# arc run：一轮只调度，不等 worker，也不等 gate

- 状态：decided（可以派发；排在 review stage 落地之后，冲突少）
- 涉及 ADR：0048（§7 worker、§9 失败与 stall、§10 gate、§12 auto、§13 调度循环）
- 来源：arc 第二个 plan 的复盘

## 背景

`radian arc run` 的一轮按固定顺序走：reconcile → 到期的 retry → verify → 领取并运行 ready 的 task → merge。有两处同步等待：

- **第 4 步等 worker**：在一个线程作用域里启动每个 attempt 的 `radian arc work` 包装进程，并且**等所有包装进程结束**才往下走；等待期间由 arc 进程执行 wall time、stall、cost 上限。
- **第 3 步等 gate**：按顺序同步跑每个 verifying task 的 gate（含远端完整 Verify，一次数分钟），跑完才进入第 4 步。merge 时的 integration 同样同步。

后果：一轮的长度等于最慢的 worker 加上排队的 gate。gate 等同一轮里别的 worker，worker 名额又在 gate 跑的时候空着。实测一个 task 在会话里等了 137 分钟，大部分是在等同一轮里另外两个 worker（分别跑了 48 和 32 分钟）和它们之后的 gate。

已经有的基础：包装进程是独立进程，自己写 `AttemptFinished`；下一轮的 reconcile 能接上已经结束的 attempt，也能把死掉的 attempt 按 retry 规则处理；`--watch` 让 `run` 成为常驻循环。

## 决定（已定）

一轮只做调度：启动、收结果、推进状态，几秒内结束；耗时的活都在一轮之外跑，下一轮读它们留下的记录。

1. **worker 启动即返回。** 第 4 步启动包装进程、把 task 置为 `doing.running` 后就返回。wall time、stall、cost 上限由**包装进程自己**执行：超限时它按 ADR 0048 的 cancel 流程（SIGTERM、宽限、SIGKILL）停掉 `radian exec`，写下带失败类别的结束记录。这样一次性 `run` 退出后 worker 仍受约束。
2. **一次性 `run`（不带 `--watch`）** 启动 worker 后不等它们：做完当下能做的转换就退出，下次 `run` 再接结果。要持续推进用 `--watch` 或开着的 arc TUI。手册写明这一点。
3. **心跳判活。** 包装进程定期（例如每 15 s）写心跳文件：pid、进程启动时间、最后心跳时间。reconcile 用心跳的新旧判断 attempt 是否还活着：超过 stall 时间没更新就当作死了，按 retry 规则处理。不用 `ps`（workspace 沙箱里会被拒）。
4. **gate 在后台跑。** 一轮看到 `review.verifying` 的 task，就在后台（线程或子进程）启动它的 gate 并记下"正在验证"，然后返回；之后的轮次读 verdict 文件推进状态。merge 时的 integration 同样在后台跑。同一阶段的 gate 仍按现有锁排队（每个阶段一份检出目录、远端 Verify 一把锁），但不再挡 worker 的领取与合并。
5. **并发名额只按 worker 算。** `[limits] concurrency` 只计 `doing` 状态的 attempt；在跑的 gate 不占名额。
6. **review 阶段不排在本地 commands 的锁后面。** 它只是一个只读模型会话，不编译、不占远端 Verify，可以和另一个 task 的 commands 同时跑（对同一个 task 仍然在 commands 通过之后）。
7. **停止。** `--watch` 或 TUI 收到 SIGINT/SIGTERM：不再启动新 worker 和新 gate；在跑的 worker 继续跑（由下一个循环接）；在跑的 gate 给一个短宽限，没跑完的由下一个循环当作未验证重跑。

## 验收

- 并发 2：一个 worker 跑很久时，另一个 task 在它结束前完成 verify 和 merge（集成测试用假 worker、假 gate 命令控制时长）。
- 一个 task 的 gate 在跑时，空出的名额立刻领取下一个 ready task。
- wall time、stall、cost 上限在包装进程里照样触发 cancel，现有对应测试全过；`run` 进程被杀掉后，worker 仍按上限停下。
- 心跳停止超过 stall 时间的 attempt 被 reconcile 判为死亡并按 retry 规则处理；沙箱里不调用 `ps`。
- 一次性 `run` 启动 worker 后立即退出，下一次 `run` 接上它的结果。
- 手册 `docs/user/arc.md` 的一轮顺序、`--watch`、reconcile、一次性 `run` 段落跟着改。
