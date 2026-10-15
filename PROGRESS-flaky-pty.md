# flaky-pty — fork 完整版 (b) 完成

HEAD a7f444b (pi/flaky-pty)
提交: 133ad6e/39dc34f/b577279 (前四条测试), 52265c1 (fork (b)), a7f444b (MUTATIONS)。

## 交付
- Idle 仍跟 RunFinished (审批取消/连续发送不变; 两个既有 pty 测试照旧过)。
- driver 正常/非致命路径 append TurnFinished 后 on_runtime_event 给宿主; driver_stopped 带 DriverEnd。
- TUI: session_ended (状态栏写原因, 离开 Running, /fork 拒, 不退出); turn_committed (TurnStarted 假,
  TurnFinished 真); /fork 未落盘时挂起 (5s), 收到 TurnFinished 执行, 超时提示不建空子会话;
  循环把 fork deadline 纳入 recv_timeout; 测试钩子 RADIAN_TURN_COMMIT_DELAY_MS。
- 测试: 间隙 1.5s 完整历史; 间隙 8s 超时无子会话; driver panic 进 session_ended 且 /fork 拒。

## Verify (a7f444b)
fmt/clippy ws/workspace 2056/0/jsc 221/0/两套 fixture/合约 15+17/MSRV 1.88/macOS escape/
pty 130 x3 — 全绿。变异 FC-M1/M2 全抓住。
