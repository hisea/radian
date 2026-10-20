# `radian exec -p` 等后台 job 结束再退出

- 状态：decided＼�选 (a)，闸门见 `wake-cap-unattended.md`）
- 涉及 ADR：0037（完成通知与唤醒）、0044（exec 的退出码与 `Result` 行）、0032 A3
- 来源：lead 2026-10-20 夜间 LXC worker（`radian exec -p`）的一次运行

## 背景

LXC 上一个 `radian exec -p` worker 做完代码后，把完整 Verify 用 `background: true` 放进后台，回复里写"等 verify 跑完再补报告"，然后结束了这一轮。`-p` 只跑一轮：这一轮一结束 exec 就关 driver、退出，后台 job 随会话一起被杀，Verify 只跑到第一步，报告没写完。交互会话里同样的写法没问题：job 结束时 idle 会话收到 `followup`，被唤醒一轮（ADR 0037）。

`shell` 的描述和后台结果都在教模型"后台跑、会通知你、没事就结束回复"，这在 `-p` 下恰好是错的。无人值守的 worker、Terminal-Bench 适配器都走 `-p`。

## 决定＼�

选 (a)：`-p` 在本轮结束后，只要还有运行中的后台 job 就继续等，job 结束按 ADR 0037 的 `followup` 唤醒下一轮，直到没有运行中的 job；闸门是 `wake-cap-unattended.md` 的速率闸门，到顶时结束会话、退出码 3、`Result` 行写明未处理的通知数。退出码与 `Result` 取最后一轮。两条一起落地。

## 原候选

- (a) `-p` 在本轮结束后，若还有运行中的后台 job，就继续等：job 结束按 ADR 0037 的 `followup` 唤醒下一轮，直到没有运行中的 job、且最后一轮没有再启动新 job；`maxConsecutiveWakes` 照旧是闸门。退出码与 `Result` 取最后一轮。
- (b) `-p` 下不允许后台：`background: true` 与自动转后台都关掉，命令前台跑到超时；描述里相应说明。
- (c) 维持现状，只在 `-p` 的工具描述/系统提示词里说"本次只有一轮，不要把要等的事放进后台"。

(a) 最符合"无人值守 worker"的定位，但 exec 的退出时机从"一轮"变成"一轮加它唤醒的链"；超时与 SIGTERM 的行为要一起定。

## 验收

按选定方案补。
