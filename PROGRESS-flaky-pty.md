# flaky-pty — fork (b) 事实核查: 有缺口, 按 owner 要求停下报告

## RunFinished 路径 (唯一发出点: agent-core/src/turn.rs:886 的 finish; 所有退出路径都经 finish)
| 路径 | RunFinished | 写 TurnFinished | 发 runtime 事件给 TUI |
|---|---|---|---|
| 正常结束 | 是 | 是 (driver.rs:502) | **否** |
| 取消 | 是 | 是 | 否 |
| 模型错误 (非致命) | 是 | 是 + TurnFailed (driver.rs:483-497) | TurnFailed 是, TurnFinished 否 |
| hook 错误 (非致命) | 是 | 是 + TurnFailed | 同上 |
| 预算/压缩失败 (RunError::Compaction) | 是 | 是 + TurnFailed | 同上 |
| 致命错误 (Journal/Poisoned/Session) | 是 | **否** (driver.rs:456 `is_fatal` 直接 return Err) | 否 |
| driver 停止 (closed / task drop) | 视情况 | 否 | 否 |

## 两个缺口
1. 致命错误路径 (journal/poisoned/session) 不写 TurnFinished (也不发 TurnFailed).
2. TurnFinished 只写 journal, **没有** 通过 runtime sink 发给 TUI (只有 TurnFailed/ConfigChanged
   等显式 on_runtime_event). 所以 TUI 现在根本收不到 TurnFinished.

## 结论
option (b) 需要额外改: driver 把 TurnFinished 发给 sink; 致命路径也要写+发 TurnFinished;
TUI 的 projection Idle 才跟得上. 按 owner "若有一条不写, 停下来报告, 不要改", 先停下.
