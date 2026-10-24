# Claude Code hooks 对照 radian：启发、小项与不采纳项

来源：`https://code.claude.com/docs/en/hooks`（整页，约 28 万字符）对照 ADR 0045、0046、0037、0011、0012、0017 与 `docs/user/hooks.md`。只记事实与结论，派发用的 brief 另写。

## 1. 现状对照

| | Claude Code | radian（ADR 0045） |
|---|---|---|
| 事件 | 35 个 | 3 个：`PreToolUse`、`PostToolUse`、`Stop` |
| handler 类型 | `command`、`http`、`mcp_tool`、`prompt`、`agent` | `command` |
| 配置层 | 7 层合并（用户、项目、项目本地、托管、插件、skill frontmatter、subagent frontmatter） | 2 层（`config.toml` `[hooks]`、`.radian/hooks.json`） |
| 同事件内执行 | 并行 | 项目先、用户后，顺序 |
| `PreToolUse` 超时 | 失败开放（`onFailure: "block"` 才关闭） | 失败关闭（0017 §4） |
| `PreToolUse` 返回 `allow` | 接受（deny/ask 规则仍生效） | 拒收并警告（0012 P1） |
| matcher | 非锚定 JS regex | 锚定整名 regex |
| 输出上限 | 10,000 字符，溢出存文件留 2,000 预览 | stderr 截 2,000 |
| `Stop` 续跑上限 | 连续 8 次，工具调用清零 | 10 次 / 10 分钟滑窗；内建 validator 连续 2 次 |
| `Stop` 何时不跑 | 用户中断不跑，API 错误走 `StopFailure` | 只在 `Completed` 跑 |

## 2. 五点启发（已写成 backlog）

1. **`Stop` stdin 补字段**：`stop_hook_active`、`background_tasks`、`session_crons`，让脚本分清"真完成"和"等后台"。radian 的内建 validator 自己知道有没有后台 job，用户脚本不知道。→ `docs/backlog/hooks-stop-and-feedback.md`
2. **非错误反馈通道**：`hookSpecificOutput.additionalContext`（`Stop` 与 `PostToolUse`）与 `updatedToolOutput`（`PostToolUse`）。radian 只有 exit 2，且 `PostToolUse` 的 exit 2 必定把结果标成错误。→ 同上
3. **`async` 后台 hook 接到 0037 的后台 job 上**：exit 0 的 `additionalContext` 走 `inject`，`asyncRewake` 的 exit 2 走 `followup` 计一次 wake。零新概念。→ `docs/backlog/hooks-async.md`
4. **`PermissionRequest` 让脚本回答 ask**：headless worker 的 ask 今天一律按拒绝（0043 1a.7）。只回答 ask，不扩权。`updatedPermissions` 顺带解决"记住这次选择"。→ `docs/backlog/hooks-permission-request.md`
5. **续跑计数在工具调用后清零**：区分"续跑后干了活"和"续跑后又只说不做"。validator 的两次规则已是这个精神，hook exit 2 的计数没有。滑窗留作硬顶。→ `docs/backlog/hooks-stop-and-feedback.md`

## 3. 其他推荐（小项，随最近的 backlog 一起做）

- **`args` 数组**：有 `args` 时 `command` 是可执行文件路径，exec 形式不经 shell，占位符注入不了。radian 只有 `/bin/sh -c`。→ `hooks-stop-and-feedback.md`
- **每次运行一行耗时**：`PostToolUse:write [cmd] status 0 (31ms)`，结束方式还有 `timed out`、`cancelled`、`failed to start`。`/hooks` 页今天只有配置与启动行。→ `hooks-stop-and-feedback.md`
- **`if` 字段**：一条权限规则表达式作为 hook 的第二层过滤，如只对 `shell(git *)` 跑；只在工具事件上求值；单条规则，无 `&&`/`||`。radian 复用 `permissions.md` 的规则语法即可，脚本不必自己 parse stdin。未写 backlog，等用例。
- **输出上限与溢出**：10,000 字符上限、溢出存文件留 2,000 预览，与 radian shell 工具的 clamp 同一模式；hook stderr 的 2,000 截断可以统一到同一套。未写 backlog。
- **`prompt` 型 `Stop` hook 与 `impossible: true`**：Claude Code 把 `/goal` 定义为"会话级 prompt 型 Stop hook"；prompt hook 的返回 `{ok, reason, impossible}`，`impossible: true` 让模型声明条件永远满足不了，Claude Code 随即放行结束，避免死循环。radian 的 `/goal`（0047）与 intent-stop validator 是同一东西的内建版；intent-stop 研究推荐的"model judges"版本，将来的实现形式就是 prompt 型 `Stop` hook，`impossible` 这个出口值得照搬。未写 backlog，依附 `turn-end-continuation.md`。
- **注入文案写成陈述句**：Claude Code 明说 `additionalContext` 要写事实陈述，命令式、系统口吻的句子会触发模型的注入防御。radian 的完成通知结尾是命令式（`Resume the task using this result. Do not reply to this notice.`）。接 Anthropic 路由时观察一下这条有没有被当成注入。未写 backlog。
- **`statusMessage`**：hook 运行时的 spinner 文案。TUI 小项，顺手。
- **五个生命周期事件按用例逐个加**：`UserPromptSubmit`、`SessionStart`、`Notification`（只 `permission_prompt` 与 `idle_prompt`）、`PreCompact`/`PostCompact`、`StopFailure`。→ `docs/backlog/hooks-lifecycle-events.md`

## 4. 有意不采纳

| 项 | 原因 |
|---|---|
| `PreToolUse` 超时失败开放 | 0017 §4 失败关闭更稳；Claude 自己的安全提示也是"加 `onFailure: "block"`" |
| 非锚定 matcher | 0045 §1 锚定，`sh` 不匹配 `shell`；import-claude 已翻译 |
| `PreToolUse` 返回 `allow`/`ask` | 0012 P1 hook 不扩权；`PermissionRequest` 是回答 ask，不是这条 |
| `http`、`mcp_tool`、`agent` handler | arc 的 `on_transition` 命令里 curl 就够；不在 radian-host 引入 HTTP 客户端；agent 型是实验性 |
| `continue: false` 全局停机 | 没有用例；`Stop` 的 exit 2 与 `PreToolUse` 的 deny 够用 |
| `SubagentStart`/`SubagentStop` | radian 的 subagent 是子 `radian exec` 进程，`Stop` hook 已在子进程里跑 |
| `TaskCreated`/`TaskCompleted`/`TeammateIdle` | Claude Code 的会话内任务表；radian 的对应物是 arc，arc 有自己的 `after_merge`/`after_fail`/`on_transition` |
| `WorktreeCreate`/`WorktreeRemove` | arc 拥有 worktree |
| `ConfigChange` | radian 配置下次启动生效（0004 热槽除外） |
| `FileChanged` | 文件系统 watcher，小众，新依赖 |
| `PostToolBatch`、`PermissionDenied`、`PostToolUseFailure` | `PostToolUse` 的 `tool_response.is_error` 已覆盖失败；批次与 auto 模式重试无用例 |
| `InstructionsLoaded`、`MessageDisplay`、`CwdChanged`、`DirectoryAdded`、`Setup` | 纯可观测或 Claude Code 特有 UI 层 |
| `PreModelSwitch`/`PostModelSwitch`、`Elicitation`/`ElicitationResult`、`UserPromptExpansion` | 无用例 |
| 7 层配置、skill/subagent frontmatter 带 hook、`once` | 2 层够；skill 带 hook 等 0025 有需求再说 |

## 5. 参考字段（供 import-claude 与将来的 drift 测试对照）

- 通用 stdin：`session_id`、`prompt_id`、`transcript_path`、`cwd`、`scratchpad_dir`、`permission_mode`、`effort`、`hook_event_name`；工具事件加 `tool_name`、`tool_input`、`tool_use_id`，MCP 工具加 `mcp_server {name, source}`。
- `Stop` stdin：`stop_hook_active`、`last_assistant_message`、`background_tasks[{id,type,status,description,command?,agent_type?,server?,tool?,name?}]`、`session_crons[{id,schedule,recurring,prompt}]`。
- `PermissionRequest` 返回：`decision {behavior: allow|deny, updatedInput?, updatedPermissions?[{type: addRules|replaceRules|removeRules|setMode|addDirectories|removeDirectories, destination: session|localSettings|projectSettings|userSettings}], message?, interrupt?}`；stdin 有 `permission_suggestions`，无 `tool_use_id`。
- `PreToolUse` 返回优先级：deny > defer > ask > allow。
- 通用返回：`continue`、`stopReason`、`suppressOutput`（无效果）、`systemMessage`、`terminalSequence`；`additionalContext`、`systemMessage`、`initialUserMessage`、纯 stdout 各限 10,000 字符。
- `command` 字段：`command`、`args`、`async`、`asyncRewake`、`shell: bash|powershell`、`onFailure: continue|block`；通用：`type`、`if`、`timeout`、`statusMessage`、`once`。
- 超时默认：`command`/`http`/`mcp_tool` 600 s，`prompt` 30 s，`agent` 60 s；`UserPromptSubmit` 等事件上降到 30 s，`MessageDisplay` 10 s，`SessionEnd` 共用 1.5 s 预算。
- `SessionEnd` `reason` 闭集：`clear`、`resume`、`logout`、`prompt_input_exit`、`other`。
- `Notification` matcher 闭集：`permission_prompt`、`idle_prompt`、`auth_success`、`elicitation_dialog`、`elicitation_url_dialog`、`elicitation_complete`、`elicitation_response`、`agent_needs_input`、`agent_completed`、`quota_auto_resume_fired`、`quota_auto_resume_stale`、`quota_auto_resume_disabled`、`plugin_notification`。
