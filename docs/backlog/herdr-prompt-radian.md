# `herdr agent prompt` 拒认运行中的 radian

- 状态：decided（我们这边不改代码；跟踪上游 herdr #4732）
- 涉及 ADR：0034

## 背景

`herdr agent prompt <pane>` 和 `herdr agent send-keys <pane>` 对运行中的 radian 返回 `agent_not_ready … is not an active named agent`，而 `herdr agent list` 同时显示 `agent: radian`、状态正确。herdr 0.9.5 上复现；只用 `pane report-agent` 自报的最小脚本（不涉及 radian）也复现。

## 原因（herdr 源码核对）

- `agent prompt` / `agent send-keys` 要求 pane 上是 herdr 编译进去的已知 agent：`src/app/api/agents.rs` 里 `effective_known_agent()` 为 `None` 就返回 `agent_not_ready`。`effective_known_agent()` 只认 herdr 的 `Agent` 枚举（`src/detect/mod.rs`）里的名字；自报的名字不算。之后还要过一道前台进程识别。
- 自报来源（不以 `herdr:` 开头）的 `agent_session_id` 也会被 herdr 丢掉（`agent_resume.rs` 的 `session_ref_from_report`），所以 `agent list` 里 radian 没有 `agent_session`。不影响恢复：radian 上报的 `resume_argv` 照常保存、照常在重启后执行。
- herdr 文档（Add Herdr support to your agent）让 agent 自己上报，不需要给 herdr 提 PR；radian 已经这样做（ADR 0034）。给 herdr 加一个 radian 的 `integration` 资产会和 radian 自己的上报重复、冲突，不做。

## 决定

- 上游：herdr 只接受 `.github/APPROVED_CONTRIBUTORS` 名单里的人提 PR，功能提议走 Discussions，同一问题已有 issue #4732（triaged、p2、maintainer-needed），#5438 已被当作它的重复关掉。我们在 #4732 下补充 radian 的复现，不另开 issue、不提 PR。
- 我们这边：给运行中的 radian 发消息，用 `herdr pane send-text <pane> "<text>"` 加 `herdr pane send-keys <pane> enter`（绕过 agent 种类检查），不用 `herdr agent prompt`。

## 验收

- herdr 修了 #4732（或加了 radian 的已知 agent 条目）之后：`herdr agent prompt` 能对运行中的 radian 提交一行，删掉本文件。
