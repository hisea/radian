# hooks：`StopFailure` 与 `Notification` 两个事件

- 状态：decided（可以派发；排在 `hooks-async.md` 之后，见 `hooks-stop-and-feedback.md` 的顺序说明）
- 涉及 ADR：0045（§1 "no more events exist"）、0017（模型错误结束回合）、0034（向 herdr 上报状态）、0024（结构化用户交互）、0013（默认无遥测）、0046（import-claude）
- 来源：`hooks-lifecycle-events.md` 的五个候选里先做的两个（owner 2026-10-29）；Claude Code hooks 参考页 `StopFailure` 与 `Notification` 两节

## 背景

- 回合因模型错误结束（rate limit、服务端错、上下文溢出）时，无人值守的 `radian exec` 和 arc worker 没人知道，只有事后翻 journal。`Stop` 只在 `Completed` 跑是对的（取消或失败的回合不能被 exit 2 续跑），所以要一个只观察的独立事件。
- 没有 herdr 的用户在 radian 等批准或回合完了无人输入时收不到任何提醒。herdr 用户已有 0034 的状态上报。

## 决定（已定）

字段名沿用 Claude Code（owner 2026-10-29）。两个事件都只观察：不阻止、不读 stdout、退出码与超时只影响 §3 的记录，不失败关闭。默认超时 60 s。

### 1. `StopFailure`

- 何时：回合以 `RunStatus::Failed` 结束，且原因是模型错误（driver 的 `on_model_error` 那一处，0017），每个失败回合一次。hook 错误、压缩错误、journal 错误结束的回合不触发（那些不是模型错误，另有 0017 的处理）。取消的回合不触发。
- `matcher`：错误类别，取值就是 `ModelErrorKind` 的闭集：`transport`、`protocol`、`unavailable`、`usage_limit`、`context_overflow`、`empty_response`。parser、手册与 drift 测试共读 `MODEL_ERROR_KINDS`。`*` 或空匹配全部。不在闭集里的 matcher 加载时警告一行，那个条目不跑（与 0045 §1 非法 pattern 同规则）。注意它不是 Claude 的类别名（`rate_limit`、`authentication_failed` 等），手册写明对应关系；import-claude 能一一对应的翻译，对应不上的丢弃并报告。
- stdin：`hook_event_name: "StopFailure"`、`cwd`、`session_id`、`error`（类别，上面的闭集之一）、`error_details`（错误消息，2000 字符截断）、`last_assistant_message`（同 `Stop`，空串表示没有）。
- 不等待：hook 在后台跑，不推迟回合结束、`exec` 的退出码与输出；`exec -p` 退出前最多等它 5 s，然后杀进程组。

### 2. `Notification`

- `matcher` 闭集两个值：
  - `permission_prompt`：radian 即将向人请求批准时，即 TUI 弹出审批框、`exec` 在终端上提示的那一刻（在 `hooks-permission-request.md` 的 hook 没给决定之后）。每次提示一次。
  - `idle_prompt`：回合结束（任何结束状态）后 60 s 内没有任何输入，触发一次；有输入后重新计时。只 TUI；`exec -p` 结束即退出，没有空闲。`/loop` 或 `/goal` 安排了下一回合时不触发（会话不在等人）。
- stdin：`hook_event_name: "Notification"`、`cwd`、`session_id`、`notification_type`（上面两个值之一）、`title`（`radian`）、`message`：`permission_prompt` 为 `radian needs your permission to use <tool 名>`，`idle_prompt` 为 `radian is waiting for your input`。消息只含工具名，不含参数与正文（0034 的隐私口径，也合 0013：radian 自己不往外发任何东西，hook 发什么由用户的命令决定）。
- 不等待：后台跑，不推迟审批框的出现。
- 60 s 是常量，手册写明，没有配置键。

### 3. 通用

- 两层规则、去重与沙箱同 0045 §1、§4。两个事件都写入 `HOOK_EVENTS`，记入 `hooks-stop-and-feedback.md` §5 的最近运行表。
- `async` 字段在这两个事件上无意义（本来就不等），写了不报错，手册写明 inert。
- `hooks-lifecycle-events.md` 留下其余三个事件（`UserPromptSubmit`、`SessionStart`、`PreCompact`/`PostCompact`），仍是 idea。

## 验收

- `exec -p` 下 provider 返回 429（`usage_limit`）：`StopFailure` hook 收到 `error: "usage_limit"` 与 `error_details`，`matcher = "usage_limit"` 的条目跑、`matcher = "transport"` 的不跑；`exec` 的退出码与不配 hook 时相同；hook `sleep 30` 时 `exec` 在约 5 s 后退出且 hook 进程组被杀。
- 用户取消的回合、hook 错误结束的回合不触发 `StopFailure`；`Completed` 回合不触发。
- 非法 matcher（`rate_limit`）加载时警告一行且不跑。
- TUI 里一次 ask 弹框时 `permission_prompt` 跑一次，`message` 含工具名不含参数；`PermissionRequest` hook 已回答的 ask 不触发。
- 回合结束后 60 s 无输入，`idle_prompt` 跑一次，再等 60 s 不再跑；中间有输入则重新计时；`/loop` 开着时不跑（测试用可注入的时钟，不真等 60 s）。
- import-claude 把两个事件的 Claude 条目带过来，`StopFailure` matcher 按对应表翻译，对应不上的丢弃并报告。
- `docs/user/hooks.md` 两个事件各一节：何时触发、stdin 字段、matcher 全部取值、不能阻止、超时默认与 `exec` 等待的 5 s；ADR 0045 §1 在同分支的 docs-only commit 里列出新事件。
