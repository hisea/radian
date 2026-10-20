# 唤醒上限让无人值守的 worker 停住：换一种闸门，并且到顶时不再沉默

- 状态：idea
- 涉及 ADR：0037（决定 5 的 `maxConsecutiveWakes`）、0011（`inject` 不开轮）、0034（herdr 只看到 `idle`）、0032/0044（`radian exec -p` 没人能推）
- 来源：的一次 dogfood 观察

## 背景

一个 dogfood worker 把测试放进后台、结束回合、等完成通知，这样连做了三件事。第四个 job 结束时，ADR 0037 决定 5 的上限（默认 3）让通知只注入、不唤醒：会话空闲，`inject` 进的是 `next_step`，而空闲会话没有下一步（ADR 0011），通知就一直躺在收件箱里。worker 停在那里，没有任何提示；owner 手动发了一条消息才把它推醒。

代码里的事实（`crates/radian-host/src/job_notify.rs`）：

- 计数器数的是「上一条用户消息以来被唤醒的轮数」，每次 `followup` 成功就 +1，只有用户消息（`SendFollowup`/`Steer`/`Inject`）清零。被唤醒的那一轮有没有编辑文件、有没有启动新 job，都不影响计数。
- 所以它实际的语义是「每条用户消息最多换 3 次唤醒」，不是 ADR 写的「防『唤醒的一轮又启动一个后台命令』的自激」。一个正常推进的 worker（跑测试 → 看结果 → 改代码 → 再跑测试）每一步都是这个形状，第四步必停。
- 到顶之后没有任何可见信号：TUI 不显示，herdr 按 ADR 0034 的纯函数只看到 `idle`（它会显示成 done），journal 里只有一条普通的 `InboxEnqueued { wake: false }`。
- `crates/radian-host/src/hooks.rs` 的 `STOP_MAX_CONTINUATIONS`（Stop hook 的连续续跑上限，也是 3）是同一个形状的闸门，到顶后同样沉默。
- `radian exec -p` 今天一轮就退（`exec-waits-for-jobs.md`）；那条的候选 (a) 写了「`maxConsecutiveWakes` 照旧是闸门」，但无人值守时到顶没人能推，闸门就等于把 worker 杀在半路，两条要一起定。

ADR 0037 担心的自激是「job 一结束就唤醒、唤醒的一轮立刻又起一个 job、循环不止」。这个循环的特征是**快**（job 本身没做事）；正常 worker 的链每一环都有一个要跑几分钟的命令。按次数数不开这两者，按速率能。

## 待决问题

1. **闸门换成什么。** 候选：
   - (a) 按速率：滑动窗口内最多 N 次唤醒（例如 10 分钟 10 次），超过才退化成注入；窗口过去自动恢复，不需要用户消息。正常 worker 碰不到；空转循环几十秒就到顶。
   - (b) 只数短 job：被通知的 job 运行时间低于阈值（例如 10 s）才计入上限；长 job 的唤醒永远放行。更贴近 ADR 的本意，但一个 `sleep 30` 的循环能绕开（代价是每圈 30 s，成本自己兜底）。
   - (c) 只把默认值调大并开配置项（`[shell] max_consecutive_wakes`）。最小改动，但无人值守的 worker 只是晚一点停。
   - 建议 (a)，默认值用 dogfood 的 journal 量一下再定；`STOP_MAX_CONTINUATIONS` 同样处理。
2. **到顶时怎么不沉默。** 不论选哪种闸门，退化成注入时都要：TUI 显示一条通知（「唤醒预算用完，N 条 job 通知在等，发任何消息继续」）；herdr 不能是 `idle`——是在 ADR 0034 的 `state_labels` 里加一个值，还是报 `blocked`，要改 0034 才能定；journal 里写明这条通知是被闸门降级的，retro 才看得出来。
3. **无人值守的会话没人能推。** `radian exec -p`（含 `subagent` 的子进程、herdr 里的 worker）到顶时应该：要么不受闸门约束（但自激的成本也没人看），要么走到顶就结束会话，`Result` 行写明「唤醒预算用完，还有 N 个 job 的通知未处理」，退出码非零。和 `exec-waits-for-jobs.md` 的候选 (a) 一起定。

## 验收

按选定方案补。至少要有：一个 worker 连续做 ≥ 4 轮「后台跑命令 → 被唤醒 → 处理结果」不被卡住的测试；一个空转循环（job 立即结束、唤醒的一轮只起下一个 job）在有限次数内被降级的测试；降级时 TUI 通知与 herdr 状态的测试；`exec -p` 的结束路径按第 3 条的决定测。
