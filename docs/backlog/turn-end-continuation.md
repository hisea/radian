# 轮末「只说不做」的内建续跑：检测 + 一次 System 续跑 + 可见

- 状态：idea（研究建议，设计未定）
- 涉及 ADR：0011（`followup` lane、L2 来源、L5 空闲唤醒）、0037 决定 5（唤醒速率闸门与到顶可见）、0045 §3（`Stop` 续跑路径）、0036（角色规则与完成通知作为消息追加）、0034（herdr `blocked`）、0005（宿主消息 meta）、0017（只有 `Completed` 续跑）
- 来源：`docs/research/2026-10-21-intent-stop.md`（§4 频率、§5 选项、§6 建议）

## 背景

模型有时用一句「下一步是 X」结束回合，没有工具调用，任务没完：`Now the docs-only ADR updates.`、`Writing the report.`、`Confirming the details.`。`agent-core` 把纯文本的 `EndTurn` 回复当作回合完成（`RunStatus::Completed`），之后只等人。本机 journal 里 muse-spark-1.3-contributor 的 42 个会话出现 9 次，平均每次等人 1–16 分钟，没人看着时等了 2 小时到一整天；同一任务、同一模型、同一 brief 下 pi 和 opencode 的 ab6 运行 0 次（研究笔记 §3、§4）。角色提示词里已有「End your turn only when the task is done, when you wait for a background job, or when you need the user」，10-17 的早期案例跑的是更长的旧规则（`Work until the task is done … do not stop at a phase boundary`），两代提示词都没挡住。

radian 独有的是「回合中途结束、等通知唤醒」这条路（ADR 0037）：后台 job 的完成通知以一条 user 角色、`source = system` 的消息开启新回合，模型对它像对聊天消息一样回一句就停。9 次里 4 次发生在通知开启的回合里，2 次紧跟在一次合法等待之后，4 次卡在「写报告 / 发通知」这最后一步。

## 待决问题

1. **检测器**。研究笔记 §4 的规则（最后一句以动名词、Now/Next/Then/Let me/I'll、「接下来/现在/然后」开头；无问号；不以用户/lead 为动作主体；全文不超过约 160 字符）在本机 286 个完成回合里标出 23 条，其中 12 条是真停顿，误报集中在「Next: 由 lead 决定」「Nothing further」「现在重启你的 radian 就能试」三类。是用规则直接续跑，还是规则只做粗筛、把判断交给模型（续跑消息写明「上一条只宣布了下一步却没有调用工具；任务没完就继续，完了或要问用户就用一行说明」）？后者的误报代价是一次短调用加一行 transcript，不会做计划外的事。
2. **路径**。(a) 复用 ADR 0045 §3 的 `Stop` 路径：内建一条 `Stop` 验证器，命中即 `System` `followup`，受同一唤醒闸门、同一降级可见（journal `degraded`、TUI 一行、herdr `wake budget`）；这要求 `Stop` 的 stdin 多带上一条助手消息的文本（今天只有 `hook_event_name`、`cwd`、`session_id`），顺手让用户也能写自己的检测器。(b) 直接在 driver 轮末做，与 `/loop`、`/goal` 同一个决策点。
3. **上限**。每回合最多一次自动续跑；连续两次续跑后仍停顿则不再续，herdr 报 `blocked`，TUI 通知——ab6 里人工 `Continue.` 之后模型又停了一次，所以 1 次不够、无上限不行。
4. **范围**。只对 `RunStatus::Completed` 的回合（ADR 0045 §3 同款）；用户 Esc 取消的回合不续；`/loop`、`/goal` 开着时让它们接管，不叠加。
5. **通知文案**（独立于本条，可先做）。完成通知现在的结尾是 `output: available through job` 直接接 `Read its output with job.`（尾部为空时少一个换行，`job_notify.rs` 的 `completion_message`），而且没有一句「继续任务」；加一句「Continue the task; do not end the turn only to report this」直接针对 4/9 的情形。

## 验收（草案）

- mock provider：一回合以 `Now the docs-only ADR updates.` 纯文本结束 → 恰好一条 `source = System`、meta 标 `continuation = "intent-stop"` 的 `followup` 入队并开启下一轮；以 `Done.`、以问句、以 `Waiting on the completion notice.`（且有 job 在跑）结束 → 不入队。
- 连续停顿两次后第三次不续，herdr 纯函数输出 `blocked`，TUI 收到一行通知。
- 唤醒闸门计数包含这类续跑；闸门到顶时降级为 `inject` 并标 `degraded`。
- 变异：去掉检测器、去掉上限、让所有终态都续跑，各有一条点名的测试失败。
- 线上指标：A/B/C 同任务重跑，人工 `Continue` 次数（ab6 的 metrics 已记这一列）归零；误续跑次数（续跑后模型一行说「已完成」）单独计数。
