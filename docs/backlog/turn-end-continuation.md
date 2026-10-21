# 轮末「只说不做」：提示词与通知文案（A）

- 状态：decided（可派发）
- 涉及 ADR：0036（角色规则清单；完成通知作为消息追加）、0037（§2 自动转后台删除、前台缺省超时；决定 5 完成通知的形状、唤醒速率闸门）、0008（`[shell] auto_background_secs` 从 schema 删除）、0021（memory 注入范围）
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

7. **后台的使用数据**：本机 journal 里 shell 调用 6 690 次前台、570 次显式 `background: true`、49 次被自动转后台（muse：1 955 / 227 / 15）。9 次停顿前面的 job 全是显式后台，没有一个是被自动转的。所以模型已经偏好后台，偏好来自 `shell` 描述的 `You will be notified when the command finishes` 与 `job` 描述的 `Prefer the notice over wait`；取消自动转后台（下一条）与停顿没有直接关系，是为了与别家一致、少一条路径。描述改为：`job` 里去掉 `Prefer the notice over wait`，改成 `A completion notice arrives when the job ends.`；`background` 参数的说明改为 `Use for servers, watchers and commands expected to run longer than 600 s. Run tests and builds in the foreground with timeout_secs.`
8. **取消自动转后台**（ADR 0037 §2 整节删除；该 ADR 改为「后台只有显式 `background: true` 一条路」）。七家里只有 radian 缺省把超过 60 s 的前台命令转后台，别家都要模型显式选 background；这是把模型推进「结束回合等通知」模式的 radian 独有推力（研究笔记 §3.5）。`shell` 描述里删掉「Without `timeout_secs`, a command that still runs after the threshold moves to the background …」整段，只留 `background: true` 的说明；`[shell] auto_background_secs` 配置项删除（ADR 0008 schema 与 config fixture 同步，旧配置里出现时警告一行并忽略）；`radian-coding` 里自动转后台的测试删除。
   - **前台缺省超时 300 s，上限 600 s 不变**（今天缺省 60 s，与被删除的阈值绑在一起）。依据：Claude Code 缺省 120 s、上限 600 s；ab6 的四 crate 测试 274 s，pi 同步等了 288 s。超过上限的（整套 Verify）用 `background: true`。`timeout_secs` 的参数说明同步改成 `Default: 300. Maximum: 600.`；超时的返回文本写出两条路：`timed out after N s; the process was killed. Re-run with a longer timeout_secs (up to 600) or with background: true.`


备选（不在 A 里做）：把等待写成固定句式 `To wait for a background job, end your reply with exactly: waiting for job <id>.`，只在 B 需要可识别的等待标记时再加。

### A 的验收

- `prompt.rs` 的角色关键句测试更新；ADR 0036 清单与 `default.md` 一致。
- `shell` 单测：前台命令超过旧阈值不再转后台，到前台缺省超时（300 s）按超时结束，超时文本含两条路；`background: true` 照旧；`auto_background_secs` 出现在配置里时警告并忽略；config fixture 通过。
- `job_notify` 单测：通知末尾是那句「继续任务」；尾部为空时三行之间都有换行。
- 同一任务集 A/B/C 各 3 次：人工 `Continue` 次数（ab6 metrics 已有这列）与「纯文本中途结束」次数，改前改后对比。
