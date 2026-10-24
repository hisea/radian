# hooks：`Stop` 的输入与两种反馈口吻

- 状态：idea
- 涉及 ADR：0045（§1 事件子集、§2 stdin 与退出码、§3 `Stop` 续跑、§5 输出进上下文的位置）、0037（决定 5 唤醒闸门）、0017（§4 hook 错误失败关闭）、0012（P1 不扩权）、0046（import-claude 的子集）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页（`Stop input`、`Stop decision control`、`PostToolUse decision control`、`Debug hooks` 四节）与 radian `docs/user/hooks.md` 的对照

## 背景

radian 的 `Stop` hook 只收四个字段（事件名、`cwd`、`session_id`、`last_assistant_message`），反馈只有一条路：exit 2 续跑。`PostToolUse` 同样只有 exit 2，而且 exit 2 必定把工具结果标成错误。

三个实际缺口：

1. **脚本分不清"真完成"和"等后台"。** 内建 intent-stop validator 自己知道"有后台 job 就不触发"（0045 §3），用户脚本拿不到这个信息。Claude Code 给 `Stop` 脚本 `background_tasks`（在跑的后台任务）、`session_crons`（定时唤醒）和 `stop_hook_active`（本回合是否已由 hook 续跑而来）。radian 的 job 注册表、`/loop`、`/goal` 状态都在，只是没传给子进程。
2. **没有非错误的反馈通道。** 一个格式化 hook 想告诉模型"我重排了这个文件"，一个 `Stop` hook 想说"收尾前跑一下测试"而不算 hook 错误，现在都做不到。Claude Code 区分 `decision: block`（算错误、弹通知）和 `hookSpecificOutput.additionalContext`（正常反馈，transcript 标为 hook feedback）；`PostToolUse` 另有 `updatedToolOutput` 改模型看到的结果。
3. **续跑上限不区分"续跑后干了活"。** radian 的 hook 续跑按 0037 决定 5 的滑窗计数（10 次 / 10 分钟），validator 另有"连续两次纯文本"的硬顶。Claude Code 的做法是连续 8 次封顶，但模型一调工具就清零：续跑之后真做了事的不算。validator 的两次规则已经是这个精神，hook exit 2 的计数没有。

顺带两个小项：hook 命令只能 `/bin/sh -c`，Claude Code 的 `args` 数组走 exec 形式不经 shell，占位符注入不了；每次 hook 运行留一行带耗时的记录（`PostToolUse:write [cmd] status 0 (31ms)`），今天 `/hooks` 页只有配置与启动行，找慢 hook 靠猜。

## 待决问题

- `Stop` stdin 加哪些字段：`stop_hook_active`、`background_tasks`（每项 `id`、`type`、`status`、`description`，`shell` 项带 `command`）、`session_crons` 是否改名为 radian 的说法（`loop`、`goal`），还是沿用 Claude 字段名以便脚本可移植（0045 §2 现在沿用 Claude 字段名）。
- `additionalContext` 进上下文的形状：`Stop` 上走 `followup` lane 的 `System` 消息、与 exit 2 的续跑合并计一次 wake（0045 §3 已有合并规则），还是独立一类。`PostToolUse` 上附在该工具结果之后、不标错误（0045 §5 的同步写入位置不变）。
- `updatedToolOutput` 是否接受：模型看到的结果被替换，journal 记原始结果还是替换后的（0001、0006 的 transcript 一致性）。
- 续跑计数：工具调用清零只加在 hook exit 2 的计数上，滑窗 10/10 分钟留作硬顶；还是 validator 与 hook 共用一套"连续 N 次纯文本"计数。
- `args` 数组：有 `args` 时 `command` 是可执行文件路径，不经 shell；沙箱内（0045 §4）用同一条 `command_str` 路径。
- 耗时行写到哪：journal 的非 critical 事件、`/hooks` 页的最近运行表，或只在 debug 日志。

## 验收

- `Stop` 脚本能从 stdin 判断有后台 job 在跑并选择不续跑；测试：有 job 时字段非空、无 job 时为空数组。
- exit 0 加 `additionalContext` 的 `PostToolUse` hook，模型看到反馈，结果不标错误；exit 2 行为不变。
- `Stop` 上 `additionalContext` 与 exit 2 同回合只开一个续跑回合、计一次 wake。
- 续跑后调了工具的回合不计入连续计数；连续纯文本仍在原上限停下。
- `args` 形式的 hook 在 `workspace` 档沙箱内跑，占位符含 shell 元字符时不被解释。
- import-claude（0046）把 Claude 配置里的 `args` 原样带过来，不再按子集外丢弃。
- `docs/user/hooks.md` 每个新字段有名字、类型、默认值、何时生效；ADR 0045 §2、§3、§5 在同分支的 docs-only commit 里改成新快照。
