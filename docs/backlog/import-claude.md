# `radian import claude`：一次性翻译 Claude Code 的私有格式

- 状态：decided＼�；已落地（pi/import-claude，ADR 0046）：用户级 commands/skills 引用/`CLAUDE.md`/用户 hooks、项目级 commands/skills 复制；MCP 与项目 hooks 等两个 slice B（见末节）。本文件保留，不删除。
- 顺序：生态兼容四件事的第 4/4＼�按 `context-at-imports` → `mcp-servers-json` → `command-hooks-claude-subset` → `import-claude` 做；ACP 暂定不做）
- 涉及 ADR：新 ADR（外部 agent 配置导入）；0029（§5 "一次性翻译，不是运行时兼容层"的同一原则）、0008（只存 secret 引用；不写回）、0025（commands 转 skill、`extra_paths`）、0031（导入不等于信任）
- 来源：`docs/research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §6 ④；总报告 README 第 31 条
- 依赖：`mcp-servers-json.md`（目标 MCP 结构）、`command-hooks-claude-subset.md`（目标 hook 子集）、`project-skills.md`（`$ARGUMENTS`）先落地，否则对应类别只能在报告里列为"未导入"

## 背景

- Radian 已有两个一次性翻译的先例：`radian config import-pi`（绝不读 `auth.json`、不复制任何密钥值、不执行命令、不覆盖已有文件、写入前先用正式解析器校验）和 `radian theme import`（固定映射、19 份 golden）。Codex 用同一条路线做了 10 类 Claude/Cursor 迁移（Config、Skills、AgentsMd、Plugins、McpServerConfig、Subagents、Hooks、Commands、Memory、Sessions），翻译确定性地用代码做，不交给模型。
- omp 走的是运行时读 `.claude/` 整棵树；报告判断这条路对 Radian 既不安全也不极简，ADR 0025 S1 已经定了 `~/.claude/skills` 只有显式列出才读。所以 Claude 的私有格式只做导入，运行时只认 Radian 自己的格式。

## 决定（已定）

命令是 `radian config import-claude`，与 `import-pi` 对称，不新开顶层子命令。只做能保真的几类，照 `import-pi` 的骨架。**来源在哪一级，产物就落在哪一级**＼�：用户级来源写用户级文件，项目级来源写项目级文件。

用户级（`~/.claude/`）：

- `~/.claude/commands/*.md` → `~/.agents/skills/<name>/SKILL.md`：frontmatter 补上 `name`，正文里的 `$ARGUMENTS` 原样保留（前提是 `project-skills.md` 的 `$ARGUMENTS` 已支持）。
- `~/.claude/skills` → 只在 `config.toml` 写一条 `[skills] extra_paths` 指过去，不复制文件。
- `~/.claude.json` 里的 `mcpServers` → `config.toml` 的 `[mcp]` 节（`mcp-servers-json.md` 落地后是同一结构，只剩搬位置与过滤 http/sse/ws）。
- `~/.claude/settings.json` 里的 `hooks` → `[hooks]` 节，`command-hooks-claude-subset.md` 的子集；工具名翻译并提示脚本里的字段名要改。
- `~/.claude/CLAUDE.md` → `~/.radian/AGENTS.md`（Codex 的 Config 类）。Claude 的 memory 不导入：格式与 ADR 0021 不对应。

项目级（`<project>/.claude/`、`<project>/.mcp.json`）：

- `.claude/commands/*.md` → `<project>/.agents/skills/<name>/SKILL.md`。
- `.claude/skills/<name>` → 复制到 `<project>/.agents/skills/<name>`（项目级配置没有 `extra_paths`，ADR 0031 T1 只读 `permissions.profile`）；同名已存在则跳过并报告。
- `.mcp.json` → 不翻译：`mcp-servers-json.md` 落地后运行时直接读它；导入只报告"将由信任门处理"。
- `.claude/settings.json` 里的 `hooks` → `<project>/.radian/hooks.json`。

通用：每一条被丢弃的内容（`if`、`async`、http hook、`context: fork`、agents、plugins、LSP、`.claude/rules`、`CLAUDE.local.md`、memory……）都在报告里单独列一行。

边界与安全：

- 读源文件，写 Radian 的**新**文件；不覆盖已有文件（`--force` 除外），不改源文件，不导入会话（各家 JSONL 语义差别大，翻译结果过不了 journal-selfcheck 的重放，收益只在迁移那一天）。
- 不读凭据文件（`.credentials.json`、`auth.json`），不复制密钥字面值（改成 ADR 0008 §3 的 secret 引用），不执行任何命令。
- 项目级产物（`.agents/skills`、`.radian/hooks.json`）本身就是 ADR 0031 T1 的触发资源，下次启动照常进信任门：导入不等于信任。导入命令在项目里写文件前说明这一点。
- 翻译逻辑只存在于这个命令里，运行时不认 Claude 的格式（ADR 0029 §5 的同一原则）。

## 验收

- 准备用户级与项目级两个 Claude 配置的 fixture 目录（commands、skills、`CLAUDE.md`、`.claude.json`/`.mcp.json`、`settings.json` 含 hooks），生成的 golden 草稿要能被正式读取器接受（配置解析器、skill 发现、MCP 加载、hook 解析各跑一遍）；项目级产物在下次启动触发信任门。
- 丢弃清单完整：fixture 里每一条不可移植的内容都出现在报告里。
- 读取路径的日志里永远不出现 `.credentials.json` 或 `auth.json`；fixture 里放一个明文密钥，输出里必须是引用。
- 已有目标文件时不覆盖；`--force` 才覆盖。
- 落地时同分支写新 ADR（docs-only commit）并删掉本文件。

## 落地记（pi/import-claude；本文件保留）

- 落地：`radian config import-claude --claude-home <dir> [--project <dir>] [--write]`（ADR 0046）。干跑打印配置草稿 + skill/`AGENTS.md` 节、报告走 stderr；`--write` 写盘；写前项目级提示信任门。测试 `crates/radian-host/tests/claude_migrate.rs` 8 项，四个变异点名测试。
- fix1＼�永不覆盖，只合并）：`--force` 整个去掉；配置经 `edit_preserving` 格式保留合并（只追加缺失的 `extra_paths` 与 hook 条目，冲突逐键报告）；`AGENTS.md`/`SKILL.md` 已存在就跳过并逐行报告，其余照写；只读配置整个写入报错且什么都不写。
- 未落地（等两个 slice B）：用户 `.claude.json` 的 `mcpServers`（等 `[mcp]` 节）与项目 `.claude/settings.json` 的 `hooks`（等项目级 `.radian/hooks.json` 加载）只报告为未导入；项目 `.mcp.json` 永不翻译（运行时切片落地后直接读）。本文件保留到两片都落地再删。
