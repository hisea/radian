# 终端提醒：审批、提问、回合结束时发 BEL（缺省关）

- 状态：decided＼�第一档）
- 涉及 ADR：0027（footer 与 chrome）、0024（`ask_user`）、0008（配置 schema 与 fixture）、0037（后台 job 完成通知）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第一档

## 背景

opencode 有 `attention` 通知（缺省关），Claude Code 有终端 bell。herdr 里的 radian 通过 ADR 0034 上报状态，但裸终端里的用户看不到别处的窗口在等审批。

## 决定（已定）

- 配置 `tui.bell = "off" | "attention" | "all"`，缺省 `off`。
  - `attention`：审批提示打开、`ask_user` 问题打开、回合结束（正常或 `turn failed`）各发一个 BEL（`\x07`）。
  - `all`：另加后台 job 完成通知到达时。
- 每个事件最多一个 BEL，流式输出中不发，同一帧多个事件合并成一个。
- 走 ADR 0008 的配置 schema 与 fixture（`check-config-fixtures.sh`）；`NO_COLOR` 不影响它。

## 验收

- pty 测试：`attention` 下审批提示出现时输出里恰有一个 `\x07`；`off` 下没有；`all` 下 job 完成通知出现时有。
- 配置 fixture 更新并通过；未知值按 ADR 0008 的方式警告并回落 `off`。
