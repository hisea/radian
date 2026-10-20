# 外部命令 hook：与 Claude Code / Codex 同构的子集

- 状态：decided＼�
- 顺序：生态兼容四件事的第 3/4＼�按 `context-at-imports` → `mcp-servers-json` → `command-hooks-claude-subset` → `import-claude` 做；ACP 暂定不做）
- 涉及 ADR：新 ADR（命令 hook）；0012（hook 不能放宽权限）、0031（项目级 hook 进信任门）、0011（`stop` 钩子续跑走 inbox）、0017（hook 出错结束回合）、0016（`workspace` 档下 hook 进沙箱）
- 来源：总报告 `docs/research/2026-10-18-area-deep-dive/README.md` 第 17 条（两个外部命令 hook）；`11-ecosystem-compat.md` §5.5、§6 ③（字段对齐）

## 背景

今天的行为（2026-10-20 对 `main` 核实）：

- `agent-core` 有进程内的 `Hooks` trait（`BeforeModelCall`、`AfterModelResponse`、`BeforeToolCall`、`AfterToolCall`），能拒绝、改参数、直接给结果、改写结果，出错 fail closed（ADR 0012 P1/P2、0017）。没有用户层的配置：用户和项目都没法挂一个外部命令。
- Claude Code 的 hook JSON 已成事实标准（事件名、`matcher`、`hooks[{type:"command",command,timeout}]`、exit 2、`hookSpecificOutput`）。Codex 原样采用了 12 个事件；它的导入器只收一个能保真的子集：`type` 必须是 `command`，不带 `if`/`async`/`asyncRewake`/`shell`/`once`，只保留 `command`/`timeout`/`statusMessage`；`permissionDecision` 只收 `deny`，`allow`/`ask` 一律拒绝。
- Radian 的工具名与参数名和 Claude 不同：`shell`/`write`/`patch`/`read` 对应 `Bash`/`Write`/`Edit`/`Read`；`patch` 的参数是 `path`/`old`/`new`，对应 `file_path`/`old_string`/`new_string`。社区的 hook 脚本（`jq .tool_input.file_path`）按 Claude 的名字写，拿过来要改。
- 总报告第 17 条实测外部进程的开销是 3–24 ms。

## 决定（已定）

- 先只做三个事件：`PreToolUse`、`PostToolUse`、`Stop`。`PostToolUse` 用于编辑后跑 lint 一类；`Stop` 用于轮末验证，不通过时自动开下一轮（上限 3 次，经 inbox 的唤醒条目，ADR 0011 L5）。不做 `async`、`if`、`http`、`prompt`、`agent` 这类 handler，不做另外 30 个事件。
- 配置直接写 Claude/Codex 的 JSON 子集：事件名、`matcher`、`hooks[{type:"command",command,timeout}]`，正是 Codex 导入器筛出的那个子集。位置：用户级在 `config.toml` 的 `[hooks]` 节（TOML 写同一结构，ADR 0008 加节）；项目级在 `<workspace>/.radian/hooks.json`，加进 ADR 0031 T1 的触发清单。
- stdin 用 Claude 的字段名：`hook_event_name`、`tool_name`、`tool_input`、`tool_response`、`cwd`、`session_id`。exit 0、exit 2、其他退出码，以及 `hookSpecificOutput.permissionDecision:"deny"` 和 `updatedInput`，含义与 Claude 相同。
- **`allow` 与 `ask` 拒收**（与 Codex 一致，符合 ADR 0012 P1"不能放宽"）；hook 出错或超时按 ADR 0017 fail closed。
- `matcher` 只认 Radian 的工具名。工具名和参数名**不做运行时别名**：翻译只在 `import-claude.md` 的导入命令里做（`Edit|Write` → `patch|write`、`Bash` → `shell`），并提示脚本里的 `.tool_input.file_path` 要改成 `.path`；不给 `tool_input_claude` 视图。
- 安全：用户级 hook 直接生效；项目级 hook 进 ADR 0031 信任门，并按内容哈希确认（Codex 的做法：文件变了重新问）；`workspace` 档下 hook 在沙箱里跑；**永远不读 `.claude/settings.json` 里的 hook**，别家的项目 hook 只能经 `import-claude.md` 的一次性导入由用户自己执行。
- 实现落在宿主（`radian-host`）的 `Hooks` 实现里，不改 `agent-core` 的 trait；重复调用检测之类的宿主 hook 也走这条路。
- `Stop` 钩子一旦配置就启用续跑，不另设开关；上限 3 次固定，不加配置项。
- `PreToolUse` 的 `updatedInput` 走 ADR 0012 的 `Rewrite`：参数再校验、不重跑 hook；新 ADR 写明。

## 验收

- 用一份"Claude 文档示例 hook 配置"的 fixture，确认子集能被解析；带 `if`/`async`/`http` 的条目被跳过并各报一行。
- `allow` 输出被拒收并给出警告；exit 2 时调用被拒绝，原因进入工具结果。
- `Stop` 钩子返回不通过时自动开下一轮，第 4 次不再续。
- 未受信任的项目：项目级 hook 不运行；hook 文件内容改了会重新询问。
- `workspace` 档：hook 子进程在沙箱内，写工作区外失败。
- 变异检查：放开 `allow`，"allow 被拒收"的测试必须失败；去掉续跑上限，"第 4 次不续"的测试必须失败。
- 落地时同分支写新 ADR、改 0012/0031（docs-only commit）并删掉本文件。
