# 轮末「只说不做」：先改提示词与通知文案，之后再定内建续跑

- 状态：B idea；A item 4、item 7 open（1、2、3、5、6 已落地）
- 涉及 ADR：0036（角色规则清单；完成通知作为消息追加）、0037（§2 自动转后台删除、前台缺省超时；决定 5 完成通知的形状、唤醒速率闸门）、0008（`[shell] auto_background_secs` 从 schema 删除）、0045 §3（`Stop` 续跑路径）、0011（`followup` lane、L2 来源、L5 空闲唤醒）、0021（memory 注入范围）、0034（herdr `blocked`）、0005（宿主消息 meta）、0017（只有 `Completed` 续跑）
- 来源：`docs/research/2026-10-21-intent-stop.md`（§3.4 提示词对比、§4 频率、§5 选项、§6 建议）

## 背景

模型有时用一句「下一步是 X」结束回合，没有工具调用，任务没完：`Now the docs-only ADR updates.`、`Writing the report.`、`Confirming the details.`。`agent-core` 把纯文本的 `EndTurn` 回复当作回合完成（`RunStatus::Completed`），之后只等人。本机 journal 里 muse-spark-1.3-contributor 的 42 个会话出现 9 次，有人看着时等 1–16 分钟，没人看着时等了 2 小时到一天；同一任务、同一模型、同一 brief 下 pi 与 opencode 的 ab6 运行 0 次。

radian 独有、且与停顿共现的，是「回合中途结束、等通知唤醒」这条路（ADR 0037）：角色规则 `End your turn only when the task is done, when you wait for a background job, or when you need the user`、`shell`/`job` 描述里的「完成会通知你」、完成通知以一条 user 角色的消息开启新回合。pi 与 opencode 的系统提示词对回合一字不提。那句角色规则是专为这个问题加的（`9aa6fff5`），ab6 带着它照样停了 3 次，更早的长版规则在 10-17 也没挡住。

## A. 提示词与通知文案（第 1、2、3、5、6 项已落地；第 4、7 项 open）

已落地的改动随本分支的代码 commit 进 `main`：`default.md` 删掉 `End your turn only when …`、工作区外可见加任务文本例外、完成通知以 `Resume the task using this result. Do not reply to this notice.` 收尾并修少换行、`shell` 显式转后台的返回文本按研究笔记 §3.5 改。ADR 0036 的角色规则清单已同步。

4. 查 memory 注入范围：lead 会话写入的记忆（lead 角色指令）注入了同一 project 的 worker 会话。决定是按角色还是按会话隔离，或者只是不把 lead 指令写成 project scope。

7. **取消自动转后台**（ADR 0037 §2 整节删除；该 ADR 改为「后台只有显式 `background: true` 一条路」）。七家里只有 radian 缺省把超过 60 s 的前台命令转后台，别家都要模型显式选 background；这是把模型推进「结束回合等通知」模式的 radian 独有推力（研究笔记 §3.5）。`shell` 描述里删掉「Without `timeout_secs`, a command that still runs after the threshold moves to the background …」整段，只留 `background: true` 的说明；`[shell] auto_background_secs` 配置项删除（ADR 0008 schema 与 config fixture 同步，旧配置里出现时警告一行并忽略）；`radian-coding` 里自动转后台的测试删除。
   - 连带待决：前台缺省超时今天也是 60 s（ADR 0037 §2 把两者绑在一起）。没有自动转后台后，没写 `timeout_secs` 的 `cargo test` 会在 60 s 被杀。建议缺省提到 300 s（Claude Code 缺省 120 s、上限 600 s；ab6 的四 crate 测试 274 s，pi 同步等了 288 s），上限 600 s 不变。


备选（不在 A 里做）：把等待写成固定句式 `To wait for a background job, end your reply with exactly: waiting for job <id>.`，只在 B 需要可识别的等待标记时再加。

### A 的验收（剩余）

- `shell` 单测：前台命令超过旧阈值不再转后台，到前台缺省超时按超时结束；`background: true` 照旧；`auto_background_secs` 出现在配置里时警告并忽略；config fixture 通过。
- 同一任务集 A/B/C 各 3 次：人工 `Continue` 次数（ab6 metrics 已有这列）与「纯文本中途结束」次数，改前改后对比。

## B. 内建续跑（idea，A 量过之后再定）

只在 A 之后仍有停顿时做：在 ADR 0045 §3 的 `Stop` 续跑路径上内建一条保守的轮末检测，命中后发一条 `System` `followup`，措辞让模型自己判断是继续还是一行说明已完成；每回合最多一次，连续两次仍停则不再续、herdr 报 `blocked`、TUI 一行；计入唤醒闸门，到顶时按现有方式降级并可见；`/loop`、`/goal` 开着时让它们接管；只对 `RunStatus::Completed` 的回合。

待决：规则直接续跑还是交模型判断（研究建议后者：误报代价是一次短调用加一行 transcript，不会做计划外的事）；是否给 `Stop` hook 的 stdin 加 `last_assistant_message`，让用户能用自己的检测器换掉内建规则；是否先加上面的备选等待句式，让检测器只认「无标记、无问句的纯文本结束」。

### B 的验收（草案）

- mock provider：一回合以 `Now the docs-only ADR updates.` 纯文本结束 → 恰好一条 `source = System`、meta 标 `continuation = "intent-stop"` 的 `followup` 入队并开启下一轮；以 `Done.`、以问句、以等待标记结束 → 不入队。
- 连续停顿两次后第三次不续，herdr 纯函数输出 `blocked`，TUI 收到一行通知。
- 唤醒闸门计数包含这类续跑；到顶时降级为 `inject` 并标 `degraded`。
- 变异：去掉检测器、去掉上限、让所有终态都续跑，各有一条点名的测试失败。
