# `radian import claude`：一次性翻译 Claude Code 的私有格式

- 状态：idea
- 涉及 ADR：新 ADR（外部 agent 配置导入）；0029（§5 "一次性翻译，不是运行时兼容层"的同一原则）、0008（只存 secret 引用；不写回）、0025（commands 转 skill、`extra_paths`）、0031（导入不等于信任）
- 来源：`docs/research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §6 ④；总报告 README 第 31 条
- 依赖：`mcp-servers-json.md`（目标 MCP 结构）、`command-hooks-claude-subset.md`（目标 hook 子集）、`project-skills.md`（`$ARGUMENTS`）先落地，否则对应类别只能在报告里列为"未导入"

## 背景

- Radian 已有两个一次性翻译的先例：`radian config import-pi`（绝不读 `auth.json`、不复制任何密钥值、不执行命令、不覆盖已有文件、写入前先用正式解析器校验）和 `radian theme import`（固定映射、19 份 golden）。Codex 用同一条路线做了 10 类 Claude/Cursor 迁移（Config、Skills、AgentsMd、Plugins、McpServerConfig、Subagents、Hooks、Commands、Memory、Sessions），翻译确定性地用代码做，不交给模型。
- omp 走的是运行时读 `.claude/` 整棵树；报告判断这条路对 Radian 既不安全也不极简，ADR 0025 S1 已经定了 `~/.claude/skills` 只有显式列出才读。所以 Claude 的私有格式只做导入，运行时只认 Radian 自己的格式。

## 方向（研究报告建议，owner 未定）

只做能保真的几类，照 `import-pi` 的骨架：

- `.claude/commands/*.md` 与 `~/.claude/commands/*.md` → 用户级 skill（`~/.agents/skills/<name>/SKILL.md`）：frontmatter 补上 `name`，正文里的 `$ARGUMENTS` 原样保留（前提是 `project-skills.md` 的 `$ARGUMENTS` 已支持）。
- `.claude/skills`、`~/.claude/skills` → 只在配置里写一条 `[skills] extra_paths` 指过去，不复制文件。
- `.mcp.json`、`~/.claude.json` 里的 `mcpServers` → Radian 的 MCP 配置（`mcp-servers-json.md` 落地后其实是同一结构，导入只剩搬位置与过滤 http/sse/ws）。
- `settings.json` 里的 `hooks` → `command-hooks-claude-subset.md` 的子集；工具名按该条目的 (a) 方案翻译，并提示脚本里的字段名要改。
- 每一条被丢弃的内容（`if`、`async`、http hook、`context: fork`、agents、plugins、LSP、`.claude/rules`、`CLAUDE.local.md`……）都在报告里单独列一行。

边界与安全：

- 读源文件，写 Radian 的**新**文件；不覆盖已有文件（`--force` 除外），不改源文件，不导入会话（各家 JSONL 语义差别大，翻译结果过不了 journal-selfcheck 的重放，收益只在迁移那一天）。
- 不读凭据文件（`.credentials.json`、`auth.json`），不复制密钥字面值（改成 ADR 0008 §3 的 secret 引用），不执行任何命令。
- 导入结果落在用户级配置。项目级产物（例如项目里的 hook）仍要经 ADR 0031 的信任门：导入不等于信任。
- 翻译逻辑只存在于这个命令里，运行时不认 Claude 的格式（ADR 0029 §5 的同一原则）。

## 待决问题

- 命令名：`radian import claude`（`cli.rs` 已有 `Import` 子命令的位置）还是并入 `radian config import-claude`，与 `import-pi` 对称。
- 项目级来源（`.claude/commands`、`.mcp.json`）导入后放用户级还是项目级：放用户级最简单，但"项目的 hook 变成用户的 hook"要让用户看见。
- Codex 另有的 Config、Memory 两类要不要做：`~/.claude/CLAUDE.md` 可以直接当 `~/.radian/AGENTS.md`，Claude 的 memory 格式与 ADR 0021 不对应。

## 验收

- 准备一个 Claude 配置的 fixture 目录（commands、skills、`.mcp.json`、`settings.json` 含 hooks），生成的 golden 草稿要能被正式读取器接受（配置解析器、skill 发现、MCP 加载、hook 解析各跑一遍）。
- 丢弃清单完整：fixture 里每一条不可移植的内容都出现在报告里。
- 读取路径的日志里永远不出现 `.credentials.json` 或 `auth.json`；fixture 里放一个明文密钥，输出里必须是引用。
- 已有目标文件时不覆盖；`--force` 才覆盖。
- 落地时同分支写新 ADR（docs-only commit）并删掉本文件。
