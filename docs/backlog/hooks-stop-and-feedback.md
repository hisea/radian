# hooks：`Stop` 的输入与两种反馈口吻

- 状态：decided（可以派发；本组四条 hooks 条目按 `hooks-stop-and-feedback.md` → `hooks-permission-request.md` → `hooks-async.md` → `hooks-lifecycle-events.md` 的顺序串行落地，四条都改 `crates/radian-host/src/hooks.rs`、`docs/user/hooks.md` 与 ADR 0045）
- 涉及 ADR：0045（§1 事件子集、§2 stdin 与退出码、§3 `Stop` 续跑、§5 输出进上下文的位置）、0037（决定 5 唤醒闸门）、0017（§4 hook 错误失败关闭）、0012（P1 不扩权）、0046（import-claude 的子集）、0001（journal 记最终结果）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页（`Stop input`、`Stop decision control`、`PostToolUse decision control`、`Debug hooks` 四节）与 radian `docs/user/hooks.md` 的对照

## 背景

radian 的 `Stop` hook 只收四个字段（事件名、`cwd`、`session_id`、`last_assistant_message`），反馈只有一条路：exit 2 续跑。`PostToolUse` 同样只有 exit 2，而且 exit 2 必定把工具结果标成错误。`PostToolUse` 与 `Stop` 今天不读 stdout（`crates/radian-host/src/hooks.rs` 的 `parsed_output` 只给 `PreToolUse` 用）。

三个实际缺口：

1. **脚本分不清"真完成"和"等后台"。** 内建 intent-stop validator 自己知道"有后台 job 就不触发"（0045 §3），用户脚本拿不到这个信息。
2. **没有非错误的反馈通道。** 格式化 hook 想告诉模型"我重排了这个文件"，`Stop` hook 想说"收尾前跑一下测试"而不算 hook 错误，现在都做不到。
3. **续跑上限不区分"续跑后干了活"。** hook exit 2 的续跑只受 0037 决定 5 的滑窗（10 次 / 10 分钟）约束。

顺带两个小项：hook 命令只能 `/bin/sh -c`；每次 hook 运行没有耗时记录，找慢 hook 靠猜。

## 决定（已定）

字段名一律沿用 Claude Code（owner 2026-10-29），脚本两边通用，import-claude 不翻译。

### 1. `Stop` stdin 加三个字段

- `stop_hook_active`（bool）：本回合是否由 `Stop` 续跑打开（hook exit 2、§2 的 `decision: "block"` 或 `additionalContext`、内建 validator，任一种）。`CommandHooks` 发出续跑 followup 时置位，这个续跑回合结束跑完 `Stop` 后清掉；用户消息到达（`followup`/`steer`/`inject` 任一）同样清掉，与 `reset_wakes` 同一处。
- `background_tasks`（数组）：`Stop` 运行时正在跑的后台 job，取自 `Jobs::list()`。每项 `{ "id", "type": "shell", "status": "running", "description", "command" }`；`description` 是命令的第一行，`command` 是完整命令。无 job 时为 `[]`，字段总在。`hooks-async.md` 落地后，正在跑的 async hook 也列在这里，`type` 为 `"hook"`。
- `session_crons`（数组）：会自己再开回合的会话级调度。radian 没有 cron，按两种来源填：`/loop` 开着时一项 `{ "id": "loop", "type": "loop" }`；`/goal` 处于 Active 或 Paused 时一项 `{ "id": "goal", "type": "goal", "status": "active" | "paused" }`。都没有时为 `[]`。`radian exec` 没有 `/loop`，只可能有 goal 一项。

### 2. exit 0 的 JSON：`PostToolUse` 与 `Stop` 开始读 stdout

两者沿用 `PreToolUse` 的规则：空 stdout 什么都不做，非 JSON 警告一行后忽略。认这些字段：

- `hookSpecificOutput.additionalContext`（字符串）：非错误反馈。
  - `PostToolUse`：在该工具结果后附一段 `[PostToolUse hook] <文本>`，**不**置 `is_error`。与 exit 2 的反馈同一处写入（0045 §5 的同步位置不变）。
  - `Stop`：打开一个续跑回合，与 exit 2 走同一条链：同一扇唤醒闸门、与其他层的 exit 2 和 validator 合并为一条消息、只开一个回合、计一次 wake。它不是 hook 错误，不出警告行。
- `decision: "block"` 加 `reason`：与 exit 2 加 stderr 等价（Claude 的 JSON 写法）。`PostToolUse` 上把结果标为错误，`Stop` 上续跑。同时有 `decision: "block"` 与 `additionalContext` 时两段都进消息，block 在前。
- `hookSpecificOutput.updatedToolOutput`（只 `PostToolUse`，字符串）：替换模型看到的结果正文，结果变成一段文本，`is_error` 与 `error_code` 不变。项目层先替换，用户层看到的是替换后的结果（0045 §1 第 2 条的层序）。替换发生在 `ToolExecutionSettled` 之前，journal 与 transcript 记替换后的结果（`ToolExecutionSettled.result` 本来就是 `after_tool_call` 之后的最终结果），不改 0001 与 0006。字符串之外的值失败关闭，按 hook 错误处理。
- 截断：`additionalContext`、`reason`、`updatedToolOutput` 都按今天 stderr 的 2000 字符上限截断（`REASON_CHAR_LIMIT`）。
- `Stop` 上的 `permissionDecision`、`updatedInput`，`PostToolUse` 上的 `permissionDecision`：忽略，警告一行。

### 3. 续跑计数：调了工具就清零

- 新增一个"连续空转续跑"计数，只算 hook 打开的续跑（exit 2、`decision: "block"`、`additionalContext`）：续跑回合以 `Completed` 结束且没有调用任何工具（`RunOutcome.model_calls == 1`）时加一；调了工具则清零。
- 上限 8（Claude Code 的数）。到上限时本次续跑不开新回合，按 0045 §3 validator 两连封顶的同一套处理：转 `inject`（`degraded`，journal 可见）、TUI 一行提示、herdr `blocked`（类别 `hook stop`，不带正文）。用户消息或下一回合开始解除。
- 滑窗 10 次 / 10 分钟留作硬顶，不变。validator 自己的"连续两次纯文本"规则不变，与这个计数并存，不合并。
- 非 `Completed` 回合清零，与 validator 计数同一处。

### 4. `args`：exec 形式，不经 shell

- hook 表新增 `args`（字符串数组，可选）。有 `args` 时 `command` 是可执行文件（路径或 `PATH` 里的名字），直接以 `args` 为参数启动，不经 `/bin/sh -c`。没有 `args` 时行为不变。
- `workspace` 档：`sandbox.command_str(command, args)`，env 规则与 `/bin/sh -c` 路径相同；`full` 档直接 `Command::new(command).args(args)`。`setsid` 与超时杀进程组不变。
- 去重（0045 §1 第 3 条）的键改为 `command` 加 `args` 整体，两层写同一条才算同一条。
- `args` 不是字符串数组：该 hook 跳过，警告一行（同今天"子集外条目"的规则）。
- import-claude：Claude 配置里的 `args` 原样带过来（今天被静默丢掉）。

### 5. 每次运行一条记录

- 每次 hook 运行结束记一条：`<事件>:<tool 名> [<命令首行>] <结束方式> (<毫秒>ms)`，`Stop` 没有 tool 名。结束方式是闭集：`status <退出码>`、`timed out`、`cancelled`、`failed to start`，parser、`/hooks` 页与 drift 测试共读一张表。
- 去处：会话内存里最近 50 条，`/hooks` 页新增"最近运行"一节显示；同时一条 `tracing` info 日志。不进 journal（0001 只记 transcript 相关事件）。`radian exec` 只有日志。

## 验收

- `Stop` 脚本能从 stdin 判断有后台 job 在跑并选择不续跑；测试：有 job 时 `background_tasks` 非空、无 job 时为 `[]`；`/goal` Active 时 `session_crons` 有 goal 一项；续跑回合里 `stop_hook_active` 为 `true`，用户消息之后的回合为 `false`。
- exit 0 加 `additionalContext` 的 `PostToolUse` hook，模型看到反馈，结果不标错误；exit 2 行为不变；`decision: "block"` 与 exit 2 结果相同。
- `updatedToolOutput` 替换后，transcript 与 journal 里的工具结果是替换后的文本；两层都替换时用户层看到项目层的结果；非字符串失败关闭。
- `Stop` 上 `additionalContext` 与另一层 exit 2 同回合只开一个续跑回合、计一次 wake、原因合并为一条消息。
- 续跑后调了工具的回合把计数清零；连续 8 次不调工具的续跑在第 8 次后转 `inject` 并出三处可见信号；滑窗硬顶仍生效。
- `args` 形式的 hook 在 `workspace` 档沙箱内跑，参数含 shell 元字符（`;`、`$(...)`、空格）时原样到达；`args` 非数组时跳过并警告。
- 四种结束方式各出现一次时 `/hooks` 页的最近运行表各有一行且毫秒数非零（超时一例的毫秒数不小于超时值）。
- import-claude（0046）把 Claude 配置里的 `args` 原样带过来。
- `docs/user/hooks.md` 每个新字段有名字、类型、默认值、何时生效，结束方式全部列出；ADR 0045 §2、§3、§5 在同分支的 docs-only commit 里改成新快照。
