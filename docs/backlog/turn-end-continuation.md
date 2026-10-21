# 轮末「只说不做」：提示词与通知文案（A）+ 内建续跑（B）

- 状态：decided（A、B 都可派发，两个独立切片；B 不等 A 的度量）
- 涉及 ADR：0036（角色规则清单；完成通知作为消息追加）、0037（§2 自动转后台删除、前台缺省超时；决定 5 完成通知的形状、唤醒速率闸门）、0008（`[shell] auto_background_secs` 从 schema 删除）、0045 §3（`Stop` 续跑路径）、0011（`followup` lane、L2 来源、L5 空闲唤醒）、0021（memory 注入范围）、0034（herdr `blocked`）、0005（宿主消息 meta）、0017（只有 `Completed` 续跑）
- 来源：`docs/research/2026-10-21-intent-stop.md`（§3.4 提示词对比、§4 频率、§5 选项、§6 建议）

## 背景

模型有时用一句「下一步是 X」结束回合，没有工具调用，任务没完：`Now the docs-only ADR updates.`、`Writing the report.`、`Confirming the details.`。`agent-core` 把纯文本的 `EndTurn` 回复当作回合完成（`RunStatus::Completed`），之后只等人。本机 journal 里 muse-spark-1.3-contributor 的 42 个会话出现 9 次，有人看着时等 1–16 分钟，没人看着时等了 2 小时到一天；同一任务、同一模型、同一 brief 下 pi 与 opencode 的 ab6 运行 0 次。

radian 独有、且与停顿共现的，是「回合中途结束、等通知唤醒」这条路（ADR 0037）：角色规则 `End your turn only when the task is done, when you wait for a background job, or when you need the user`、`shell`/`job` 描述里的「完成会通知你」、完成通知以一条 user 角色的消息开启新回合。pi 与 opencode 的系统提示词对回合一字不提。那句角色规则是专为这个问题加的（`9aa6fff5`），ab6 带着它照样停了 3 次，更早的长版规则在 10-17 也没挡住。

## A. 提示词与通知文案（decided）

1. **删掉**角色里的 `End your turn only when the task is done, when you wait for a background job, or when you need the user.`。等 job 的许可已由 `shell`、`job` 描述给出（`You will be notified when the command finishes`、`Prefer the notice over wait`），角色层不重复。ADR 0036 的角色规则清单同步去掉这一条（docs-only commit）。
2. 「不可撤销或工作区外可见的动作先问用户」加例外：任务文本里明确要求的通知、提交等不用再问。两次停顿正好停在「发 herdr 通知」之前。
3. 完成通知（`job_notify.rs` 的 `completion_message`）：末尾加 `Continue the task; do not end the turn only to report this result.`；修掉尾部为空时 `output: available through job` 与 `Read its output with job.` 之间少一个换行的 bug。
4. 查 memory 注入范围：lead 会话写入的记忆（lead 角色指令）注入了同一 project 的 worker 会话。决定是按角色还是按会话隔离，或者只是不把 lead 指令写成 project scope。
5. `shell` 转后台时的返回文本与通知文本按研究笔记 §3.5 改：返回文本说「先做不依赖结果的事；实在没事再结束回复，完成后会被自动唤醒」（opencode 的形状，只在真的进入等待时才看到）；通知以 `Resume the task using this result. Do not reply to this notice.` 收尾（omp 的形状）。
6. 不改 `Do not use sleep or polling to wait`，不动模型参数。

7. **后台的使用数据**：本机 journal 里 shell 调用 6 690 次前台、570 次显式 `background: true`、49 次被自动转后台（muse：1 955 / 227 / 15）。9 次停顿前面的 job 全是显式后台，没有一个是被自动转的。所以模型已经偏好后台，偏好来自 `shell` 描述的 `You will be notified when the command finishes` 与 `job` 描述的 `Prefer the notice over wait`；取消自动转后台（下一条）与停顿没有直接关系，是为了与别家一致、少一条路径。描述改为中性：`job` 里去掉 `Prefer the notice over wait`，改成 `A completion notice arrives when the job ends.`；`background` 参数说明它适合什么（服务、监视器、预计超过前台上限的命令），不再说「会通知你」以外的话。
8. **取消自动转后台**（ADR 0037 §2 整节删除；该 ADR 改为「后台只有显式 `background: true` 一条路」）。七家里只有 radian 缺省把超过 60 s 的前台命令转后台，别家都要模型显式选 background；这是把模型推进「结束回合等通知」模式的 radian 独有推力（研究笔记 §3.5）。`shell` 描述里删掉「Without `timeout_secs`, a command that still runs after the threshold moves to the background …」整段，只留 `background: true` 的说明；`[shell] auto_background_secs` 配置项删除（ADR 0008 schema 与 config fixture 同步，旧配置里出现时警告一行并忽略）；`radian-coding` 里自动转后台的测试删除。
   - 连带待决：前台缺省超时今天也是 60 s（ADR 0037 §2 把两者绑在一起）。没有自动转后台后，没写 `timeout_secs` 的 `cargo test` 会在 60 s 被杀。建议缺省提到 300 s（Claude Code 缺省 120 s、上限 600 s；ab6 的四 crate 测试 274 s，pi 同步等了 288 s），上限 600 s 不变。


备选（不在 A 里做）：把等待写成固定句式 `To wait for a background job, end your reply with exactly: waiting for job <id>.`，只在 B 需要可识别的等待标记时再加。

### A 的验收

- `prompt.rs` 的角色关键句测试更新；ADR 0036 清单与 `default.md` 一致。
- `shell` 单测：前台命令超过旧阈值不再转后台，到前台缺省超时按超时结束；`background: true` 照旧；`auto_background_secs` 出现在配置里时警告并忽略；config fixture 通过。
- `job_notify` 单测：通知末尾是那句「继续任务」；尾部为空时三行之间都有换行。
- 同一任务集 A/B/C 各 3 次：人工 `Continue` 次数（ab6 metrics 已有这列）与「纯文本中途结束」次数，改前改后对比。

## B. 内建续跑（decided）

在 ADR 0045 §3 的 `Stop` 续跑路径上内建一条验证器，和用户的 `Stop` hook 同一条链、同一闸门、同一降级可见：

1. **触发**：回合以 `RunStatus::Completed` 结束，且最后一条助手消息命中保守规则——纯文本、无工具调用；全文不超过约 200 字符；句末无问号，也没有「要不要 / 说一声 / 告诉我 / 需要我」一类问人的句式；最后一句以动名词、`Now`/`Next`/`Then`/`Let me`/`I'll`/`I will`/`Time to`，或「接下来 / 现在 / 然后 / 下一步」开头；动作主体不是 `you`/`p1`/`lead`/「你」；`Nothing` 不算动名词；结束时没有后台 job 在跑。规则只做粗筛（本机数据：真停顿 12/12 命中，误报集中在「Next: 由别人决定」「Nothing further」「现在重启你的 radian 就能试」三类，加上这些过滤后剩 1–2 条）。
2. **动作**：发一条 `System` `followup`（ADR 0011 L2，`source = System`），meta `continuation = "intent-stop"`（ADR 0005），正文固定：`Your last message announced a next step but made no tool call. If the task is not done, continue now. If it is done, or you need the user, say so in one line.` 判断权交给模型：误报的代价是一次短调用加一行 transcript，不会做计划外的事。
3. **上限**：每回合最多一次；连续两次续跑后仍以命中的纯文本结束，第三次不再续，herdr 报 `blocked`（ADR 0034 H2 新增一类 `blocked` 消息，只给类别不给正文），TUI 一行通知，journal 里能看出。用户消息到来即清零（与唤醒闸门同款）。
4. **闸门与可见**：计入 ADR 0037 决定 5 的唤醒速率闸门；到顶时按现有方式降级为 `inject` 并标 `degraded`，三路可见不变。
5. **不触发**：用户 Esc 取消的回合（非 `Completed`，已由 ADR 0045 §3 保证）；`/loop` 或 `/goal` 开着（它们自己轮末重投）；`exec -p` 与 TUI 行为相同。
6. **可替换**：`Stop` hook 的 stdin 多带 `last_assistant_message`（Claude 的字段名），用户能用自己的脚本做检测；配置项 `[hooks] intent_stop = true` 关掉内建规则（名字待定，ADR 0008 schema 与 fixture 同步）。
7. 涉及 ADR：0045（内建验证器与 stdin 字段）、0011、0037 决定 5、0034、0005、0008。

### B 的验收

- mock provider：一回合以 `Now the docs-only ADR updates.` 纯文本结束 → 恰好一条 `source = System`、meta `continuation = "intent-stop"` 的 `followup` 入队并开启下一轮；以 `Done.`、以问句、以 `Waiting on the completion notice.`（且有 job 在跑）、以 `Next: the reviewer decides.` 结束 → 不入队。
- 规则的单元测试用一份固定样本：本机普查里标出的 23 条与 20 条未标出的最后一句，断言标签。
- 连续停顿两次后第三次不续，herdr 纯函数输出 `blocked`，TUI 收到一行通知；用户消息后计数清零。
- 唤醒闸门计数包含这类续跑；到顶时降级为 `inject` 并标 `degraded`。
- `Stop` hook stdin 含 `last_assistant_message`；配置项关掉后不续跑。
- 变异：去掉规则、去掉每回合一次、去掉连续两次上限、让所有终态都续跑、去掉闸门计数，各有一条点名的测试失败。
- 线上指标：A/B/C 同任务重跑，人工 `Continue` 次数归零；误续跑次数（续跑后模型一行说已完成）单独计数。
