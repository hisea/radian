# 深挖 11：生态兼容（别家格式能读多少）

2026-10-18 · Radian `356df1e` · 对手：Codex `8c07694`（全部用 `git show HEAD:` 读，行号以 HEAD 为准）、opencode `63c1a27`（`packages/core`、`packages/cli`）、pi 1.3.0 npm 包（下文 `pi/` = `npm-_earendil-works_pi-coding-agent/package/`）、omp 18.12.1 npm 包（下文 `omp/` = `npm-_oh-my-pi_pi-coding-agent/package/src/`）；Claude Code 只用官方文档。
`rad/` = `<repo>/crates`。网页的抓取日期都是 2026-10-18。
本报告接着 8-extensibility.md 往下写。那份报告里已有的"外部命令 hook""stop 钩子""项目级 skill 接线"这里不再重复，只补兼容这一层：字段名、目录位置、子集边界和信任门。

## 1. 一句话结论

几家的格式正在收敛到四个厂商中立的标准：`AGENTS.md`、Agent Skills（`SKILL.md`，包括 `.agents/skills/` 目录）、MCP 客户端的 `mcpServers` JSON 结构、Agent Plugins 1.0（skills + MCP 打包）。这四个标准里，Radian 只完整读了 `AGENTS.md`。`SKILL.md` 的格式它认，却不读标准目录 `.agents/skills`；MCP 用的是 Radian 自己的 JSON；hook 根本没有。
**建议：** 厂商中立的标准在运行时直接读，并且走 ADR 0031 的信任门；厂商私有的格式（`.claude/` 下的 hooks、commands、settings）用一次性导入命令翻译成 Radian 格式，不在运行时兼容。Radian 已经用这个办法处理过 pi 的主题和配置（ADR 0029、`import-pi`），Codex 也用同样的办法做了 10 类 Claude/Cursor 迁移。omp 走的是相反的路：运行时读 9 家工具的目录，而且找不到项目信任门。对 Radian 来说这条路既不安全，也不够极简。

## 2. 兼容矩阵

"读"指运行时直接读入；"导入"指一次性转换成自家格式；"—"指不读。

| 维度 | Radian | pi 1.3.0 | Codex | Claude Code（文档） | omp | opencode |
|---|---|---|---|---|---|---|
| `AGENTS.md` | 读：`AGENTS.override.md` > `AGENTS.md` > `CLAUDE.md`，每个目录取第一个，往上读到仓库根（`rad/radian-host/src/context.rs:18-24`） | 读，候选名和顺序与 Radian 相同（`pi/docs/configuration.md:18,41-45`） | 读：只认 `AGENTS.override.md`/`AGENTS.md`，往上读到项目根，缺省 32 KiB（`codex-rs/core/src/agents_md.rs:1-18`，`config/src/config_toml.rs:75`） | 读：**没有 CLAUDE.md 时**读 AGENTS.md，需 v2.1.277 及以上（[memory](https://code.claude.com/docs/en/memory#agents-md)） | 读（`omp/discovery/agents-md.ts`） | 读：只认 `AGENTS.md`，包括全局和祖先目录（`core/src/config/plugin/instruction.ts:41-57`）；`read` 读了某个文件之后，再补读它附近的 AGENTS.md（`core/src/tool/plugin/read.ts:16,82-84`） |
| `CLAUDE.md` | 读（同一组候选名） | 读 | 缺省不读；可以用 `project_doc_fallback_filenames` 打开，缺省值是空列表（`config_toml.rs:85-87`） | 主格式；另有 `.claude/CLAUDE.md`、`CLAUDE.local.md`、`.claude/rules/*.md`（`paths:` 限定范围），子目录里的到用的时候才加载 | 读（`omp/discovery/claude-md.ts:21`） | **不读**："`.claude` is only read for skills"（`cli/src/server-process.ts:127`） |
| `@path` 导入 | **不展开**（`context.rs` 里没有导入逻辑） | 未核实 | 未核实 | 展开：相对于所在文件解析，最多 4 层，跳过代码块；导入仓库外的文件要先确认（[memory](https://code.claude.com/docs/en/memory#import-additional-files)） | 按 Claude 的语义展开（`omp/discovery/at-imports.ts:1-20`） | 未核实 |
| `.cursorrules`、`GEMINI.md`、copilot-instructions 等 | — | — | 只在导入时处理 Cursor | — | 读：cursor、gemini、cline、windsurf、github、vscode（`omp/discovery/*.ts` 各文件头） | 只在 `/init` 的提示词里提到（`plugin/command/initialize.txt:14`） |
| Agent Skills 格式 | 读：`name` 和 `description` 都必填；`name` 可以用 `[A-Za-z0-9_-]`（比规范宽），不检查和目录同名（`rad/radian-host/src/skills/mod.rs:290-320,477-484`）；遇到 `allowed-tools` 每个 skill 警告一次（`:323-330`） | 读，按规范，大多数问题只警告（`pi/docs/skills.md:7`） | 读 | 读，并做了扩展：**`name` 可省，缺省用目录名**，`description` 只是"推荐"（[skills](https://code.claude.com/docs/en/skills)） | 读 | 读（`skill-scan.ts:29`） |
| skill 目录 | `~/.radian/skills`、`[skills] extra_paths`、内建（`mod.rs:106-133`）；**不读 `.agents/skills`**，项目级还没接线 | `~/.agents/skills`，以及 cwd 到仓库根之间各级的 `.agents/skills`（`pi/docs/skills.md:61`） | `~/.agents/skills`（`ext/skills/src/host_roots.rs:105`）、仓库里的 `.agents/skills`（`:154`）、`.codex/skills`；`$CODEX_HOME/skills` 已弃用（`:96`） | `~/.claude/skills`、`.claude/skills`（往上到仓库根）、嵌套、插件；文档里**没有提到 `.agents/skills`**（未核实） | 自己的目录 + `.agents` + `.claude` + `.github/skills` 等 | `~/.claude`、`~/.agents`，以及祖先目录里的 `.claude/`、`.agents/` 下的 skills（`core/src/config/discovery.ts:69-85,124-141`） |
| MCP 配置结构 | **自有结构**：`{"servers":[{"id","command","args","env","inherit_env",…}]}`，未知字段直接报错（`rad/radian-host/src/mcp.rs:11-35`）；子进程环境只留 `PATH` 和显式列出的变量（`agent-extensions/src/process.rs:77-84`） | `mcpServers` 对象，"The format matches other MCP clients"（`pi/docs/mcp.md:33-60`）；文档给了从 Claude、Cursor、VS Code、Codex、opencode 手工转换的表（`:104-112`） | config.toml 里的 `[mcp_servers.x]`；`.mcp.json` 和 `.claude.json` **只在导入时读**（`external-agent-migration/src/mcp.rs:10-11,156`） | `.mcp.json` 里的 `mcpServers`，支持 `${VAR}` 和 `${VAR:-默认值}` 展开，有 stdio/http/sse/ws 四种（[mcp](https://code.claude.com/docs/en/mcp)） | 读 `.mcp.json`、`mcp.json` 和多家的格式（`omp/discovery/mcp-json.ts`） | 自有 `opencode.json` 的 `mcp` 键（`local`/`remote`） |
| MCP 配置位置 | 只有命令行 `--mcp-config`；配置文件里的 `[mcp]` 被跳过（`config/mod.rs:313-315`）；`import-pi` 不迁移 MCP（`config/migrate_pi.rs:128-131`） | `~/.pi/agent/mcp.json`；项目里的 `.pi/mcp.json` 要经过信任门 | 用户级或项目级 config.toml；项目层**未信任时会加载但不启用**（`config/src/loader/mod.rs:132-134`） | user/local 写在 `~/.claude.json`，project 写在 `.mcp.json`；交互模式下使用前要确认，**非交互模式不问就加载** | cwd 下的各家目录 | 全局或项目 |
| hook 配置结构 | 没有 | 不兼容（进程内 TS） | **和 Claude Code 同构**：12 个事件名，`matcher` + `hooks[{type:"command",command,timeout}]`（`hooks/src/lib.rs:24-38`）；exit 2 表示阻断或"继续干"（`events/pre_tool_use.rs:262`、`events/stop.rs:343`）；`hookSpecificOutput.permissionDecision` 只收 `deny`，`allow`/`ask` 一律拒绝（`engine/output_parser.rs:440-460`） | 33 个事件，有 command/http/mcp_tool/prompt/agent 几种 handler（[hooks](https://code.claude.com/docs/en/hooks)） | 读 `.claude/hooks/pre|post/*.sh`。这是 **omp 自己定的约定，不是 Claude 的 settings.json 格式**（`omp/discovery/claude.ts:375-420`） | 不兼容（进程内插件） |
| 自定义命令 | 没有（ADR 0043 删掉了 `.radian/prompts/`） | `prompts/*.md` | 插件里的 `commands/*.md` **安装时转成 skill**，正文里的 `CLAUDE.md` 改写成 `AGENTS.md`，超过 4000 字节截断（`core-plugins/src/command_migration/plugin.rs:17-23`，`render.rs:3-12`） | `.claude/commands/*.md`，已并入 skill（[skills](https://code.claude.com/docs/en/skills)） | 读 `.claude/commands` 和 opencode 的 commands | 读自己的 `command/` |
| agent 定义 | 不做（ADR 0043 1a.2） | — | 自有的 `agents/*.toml`；`.claude/agents` 在导入时转换（`external-agent-migration/src/subagents.rs`） | `.claude/agents/*.md` | 读 `.claude/agents` 和插件里的 agent，去掉 Claude 方言的 frontmatter（`omp/task/discovery.ts:14,123-142`） | 读自己的 `agent/` |
| 插件清单 | 没有 | pi 包（npm/git） | `.codex-plugin/`；**也读** `.claude-plugin/plugin.json`、`.cursor-plugin/`，以及 Agent Plugins 1.0 的根 `plugin.json`（`utils/plugins/src/plugin_namespace.rs:12-17`）；marketplace 认四个路径（`core-plugins/src/marketplace.rs:20-25`） | `.claude-plugin/plugin.json`，组件 13 类，包括 hooks、agents、LSP、monitors、`bin/` 等（[plugins-reference](https://code.claude.com/docs/en/plugins-reference)） | 读 `~/.claude/plugins/cache`（缺省不开），也认 Agent Plugins 1.0（`omp/discovery/agent-plugin-format.ts:1-12`） | npm 插件 |
| 会话导入/导出 | 导出 `/export md|json`（`rad/radian-tui/src/command.rs:29,1064`）；不能导入 | 导出 HTML/JSONL，`/share`（`pi/docs/sessions.md:58`） | 导入 Claude 和 Cursor 的会话（`external-agent-migration/src/sessions/`，`detect/sessions/cla.rs:26` 读的是 `projects/`） | 自有 JSONL | 导入 Claude 和 Codex 的会话（`omp/session/foreign-session-import.ts:8-11`） | 未核实 |
| 主题 | 自有 TOML；pi 主题用 `radian theme import` 一次性转换（ADR 0029，`rad/radian-tui/src/cli.rs:340`） | 自有 JSON | — | 插件里的 `themes/`（experimental） | 未核实 | 自有 |
| 编辑器协议 ACP | 没有；`radian serve`、`radian mcp` 两个子命令已预留、尚未实现（`cli.rs:374,377`） | 通过 `pi-acp` 适配器 | 通过 `codex-acp` 适配器 | 通过 Zed 的 `claude-agent-acp` 适配器 | 不在列表里 | 原生支持（`cli/src/acp/`） |
| 读别家目录的信任门 | 门已经实现，触发条件目前只有 `.radian/skills` 和 `.radian/config.toml`（`rad/radian-host/src/trust.rs:430-434`） | project trust 管 `.pi/*` 和祖先目录里的 `.agents/skills` | 项目 config 层按 trust_level 决定；hook 按内容哈希再审一次 | `.mcp.json` 要确认；导入仓库外文件也要确认 | 别家的**用户级**目录要手动打开（`omp/capability/index.ts:51-60,356-373`）；项目级直接加载，没找到门（两处写死 `isProjectTrusted: () => true`：`session/agent-session.ts:8457`、`extensibility/extensions/runner.ts:1433`，是否所有路径都这样未核实） | 没找到信任门（在 `core/src/config` 里 grep `trust` 没有结果） |

### 已经事实标准化的格式（给链接）

| 标准 | 管理方 / 规模 | 谁在用（本报告核实过的） |
|---|---|---|
| [AGENTS.md](https://agents.md/) | Linux Foundation 下属的 Agentic AI Foundation；"used by over 60k open-source projects" | Radian、pi、Codex、opencode、omp；Claude Code 自 v2.1.277 起在没有 CLAUDE.md 时读它 |
| [Agent Skills 规范](https://agentskills.io/specification) | 必填字段只有 `name`（小写、连字符、≤64、与目录同名）和 `description`（≤1024）；`allowed-tools` 标为 Experimental | 六家都读 `SKILL.md`。`.agents/skills` 目录：pi、Codex、opencode、omp 都读；Claude Code 文档里没写 |
| MCP 客户端的 `mcpServers` JSON | 没有正式规范，事实上源于 Claude Desktop（omp 的注释写"matches Claude Desktop's format"，`omp/discovery/mcp-json.ts`） | Claude Code、Cursor、pi、omp；Codex 只在导入时读 |
| [Agent Plugins 1.0.0](https://vercel.com/changelog/introducing-agent-plugins-1-0-0)（2026-08-06，Vercel 发起，schema 见 `agent-plugins.org`） | 只覆盖 "Agent Skills and MCP servers"；清单是根目录下的 `plugin.json` | 首发支持 Codex、Cursor、Copilot、Kiro、VS Code；omp 也读。Claude Code 用的仍是自己的 `.claude-plugin` |
| Claude Code 的 hook JSON（事实标准，没有规范） | 事件名、`matcher`、exit 2、`hookSpecificOutput` | Codex 原样采用这套结构（12 个事件） |
| [ACP（Agent Client Protocol）](https://agentclientprotocol.com/overview/agents) | 列了 43 个 agent | opencode 原生支持；Claude Code、Codex、pi 通过适配器 |
| [`npx skills`](https://vercel.com/changelog/introducing-skills-the-open-agent-skills-ecosystem)（2026-01-20） | skill 包安装器 | 列出 17 个 agent 用过 |

## 3. 每家的关键做法（只写和兼容有关的）

### Codex：标准格式运行时读，私有格式一次性导入
- **运行时**只读厂商中立的东西：`AGENTS.md`、`.agents/skills`、Agent Plugins 1.0 的 `plugin.json`。它也读 `.claude-plugin/plugin.json`，因为这是事实上的插件格式。
- **导入**放在单独一个 crate `external-agent-migration` 里，分 10 类：Config、Skills、AgentsMd、Plugins、McpServerConfig、Subagents、Hooks、Commands、Memory、Sessions（`src/model.rs:53-63`），来源是 Claude（代码里写作 `cla`）和 Cursor（`cur`）。
- hook 导入只收一个能保真的子集，其余直接跳过：`type` 必须是 `command`；不能带 `if`、`async`、`asyncRewake`、`shell`、`once`；只保留 `command`/`timeout`/`statusMessage` 三个字段；`disableAllHooks` 生效（`hooks_cla.rs:68-170`）。这是一份现成的"可移植 hook 子集"清单。
- 导入时会改写正文：`CLAUDE.md` 换成 `AGENTS.md`，产品名换成 Codex（`render.rs:3-12`）。翻译是确定性的，用代码做，不交给模型。
- hook 的输出不能放大权限：`permissionDecision:allow`（不带 `updatedInput`）和 `ask` 都会被当成无效输出（`output_parser.rs:450-459`）。

### omp：运行时什么都读
- `discovery/` 下约 30 个 provider 文件，覆盖 Claude、Cursor、Codex、Gemini、opencode、Cline、Windsurf、Copilot、VS Code、`.agents`、`mcp.json`……（`omp/discovery/`，共 9678 行）。按 priority 合并，自家的 `.omp` 优先。
- 别家的**用户级**目录默认关闭（`FOREIGN_USER_PROVIDERS`，`capability/index.ts:51-60`）。项目级目录无条件加载，这就意味着克隆下来的仓库里的 `.claude/hooks/pre/*.sh`、`.mcp.json` 会被直接装进来（推断，依据是上表里信任门那一行）。
- 它支持的"Claude hooks"其实是 `.claude/hooks/pre|post/<tool>.sh` 这个自定约定（`claude.ts:375-420`），并不读 Claude 真正的 `settings.json` 里的 `hooks`。可见"兼容"这个词容易名不副实：名字一样，格式不一样。

### opencode：只读别家的 skills
- `.claude` 和 `.agents` 只用来找 skills，环境变量 `OPENCODE_DISABLE_CLAUDE_CODE` 可以整体关掉（`server-process.ts:127-131`）。CLAUDE.md 不读，`.mcp.json` 也不读。范围最窄，也最好理解。
- 原生实现了 ACP 服务端（`cli/src/acp/`），能直接接进 Zed、JetBrains 这类编辑器。

### pi：读标准，其他的写转换说明
- 运行时读 `AGENTS.md`/`CLAUDE.md`、Agent Skills 和 `.agents/skills`，MCP 用 `mcpServers` 结构。跨工具的 MCP 配置**不自动读**，文档给一张手工转换表（`pi/docs/mcp.md:104-112`）。项目里的 `.pi/mcp.json` 和 `.agents/skills` 都要经过信任门。

### Claude Code（文档）
- 它是这些私有格式的源头。以前只读 CLAUDE.md，后来让步：v2.1.277 起，没有 CLAUDE.md 的仓库直接读 AGENTS.md，不需要再写一个 `@AGENTS.md` 的导入文件。
- skill 遵循 Agent Skills 规范，但扩展了大约 14 个字段（`context`、`hooks`、`paths`、`shell`……），并且 `name` 可以省略。文档自己也写了：这些扩展字段在 claude.ai 和 Skills API 里会直接报错，说明这些字段本身就不可移植。
- 项目 `.mcp.json` 在交互模式下使用前要确认，**非交互模式不问就加载**（[mcp](https://code.claude.com/docs/en/mcp)）。

## 4. Radian 的优势

1. **上下文文件已经和 pi、Codex 对齐，而且更安全。** 候选名和 pi 一致，只读到仓库根为止。单个文件 64 KiB、合计 128 KiB，比 Codex 的 32 KiB 宽松；超出时先丢外层的文件。内容加 `</project_instructions` 转义并标为不可信（ADR 0036 C2）。只要仓库有 AGENTS.md 或 CLAUDE.md，Radian 拿到的项目规则和 pi 一样。
2. **"一次性翻译"已经有两个先例。** `radian config import-pi`：绝不读 `auth.json`、不复制任何密钥值、不执行命令、不覆盖已有文件、写入前先用正式解析器校验（`config/migrate_pi.rs:1-20`）。`radian theme import`：映射确定，19 份 golden 文件（ADR 0029）。这正是 Codex 的路线，安全规则还写得更严。后面的 Claude 导入可以照抄这套骨架。
3. **信任门和权限是分开的。** 信任既不放宽也不收紧权限（ADR 0031 T1），非交互默认不信任（T7 片 B）。比较：Claude Code 在非交互模式下会直接加载 `.mcp.json`，omp 和 opencode 找不到门。将来读 `.agents/skills`、`.mcp.json` 时可以直接进这道门，不用新造机制。
4. **MCP 子进程的环境是白名单。** 只传 `PATH` 和显式列出的变量（`process.rs:77-84`），项目里的 MCP server 拿不到宿主的密钥。兼容 `mcpServers` 时要保住这一点（见 §6 ②）。
5. **skill 解析器很宽容。** 嵌套 map 和列表不会被误读成顶层键（`mod.rs:350,408`），名字字符集也比规范宽，所以别家的 skill 大多能直接读进来。

## 5. Radian 的劣势和缺口

1. **不读 `.agents/skills`。** 这是 pi、Codex、opencode、omp 共同认的跨工具目录，`npx skills` 这类安装器也往标准目录装（推断：装到哪个目录，Vercel 的公告没写）。仓库里自带的跨工具 skill，Radian 看不到。ADR 0025 裁决 2 只讨论了 `~/.claude/skills` 要不要缺省读，`.agents` 这个中立位置没有讨论过。
2. **Claude 风格的 skill 一部分会被拒收。** Radian 要求 `name` 必填（`mod.rs:290-301`），Claude Code 允许省略、缺省用目录名。用 `extra_paths = ["~/.claude/skills"]` 接 Claude 的 skill 时，没写 `name` 的会被跳过并警告。写了 `allowed-tools` 的，每个 skill 都单独刷一条警告（`:323-330`）。ADR 0025 预见到了警告刷屏的风险，但实现仍然是一个 skill 一条。
3. **MCP 的配置结构是 Radian 独有的。** 主流客户端都用 `mcpServers` 对象（键是名字），Radian 用的是 `servers` 数组（id 是字段），并且 `deny_unknown_fields`。一份现成的 `.mcp.json` 拿过来一个字都用不了，`${VAR}` 也不展开。8-extensibility 已经指出 MCP 只能走命令行；兼容层面的问题比那更早一步：连结构都对不上。
4. **`@path` 导入不展开。** 如果一个仓库只有 CLAUDE.md，正文是 `@docs/conventions.md`，Radian 给模型的就是这一行字面文本。模型也许会自己去读那个文件，但会多一轮，也可能不读。像本仓库这种 `CLAUDE.md` 只写 `@AGENTS.md` 的情况没有影响，因为 AGENTS.md 排在前面先被选中。
5. **没有 hook，也就谈不上 hook 兼容。** Codex 的 12 个事件已经和 Claude Code 同构，社区的 hook 脚本（`jq .tool_input.file_path`）是按 Claude 的工具名和参数名写的。Radian 的工具名不同（`shell`/`write`/`patch`/`read`，对应 `Bash`/`Write`/`Edit`/`Read`），参数名也不同（`path`/`old`/`new`，对应 `file_path`/`old_string`/`new_string`，`rad/radian-coding/src/patch.rs:30-47`，Claude 的定义见 `sdk-tools.d.ts:864-912`）。就算以后加了 hook，别人的脚本也要改。
6. **没有 ACP，也没有导入会话。** 编辑器生态里有 43 个 agent，Radian 不在其中。从 Claude 或 Codex 切过来的用户，正在做的会话带不过来（Codex 和 omp 都能导入）。

## 6. 改进方案（按"对 Radian 整体价值的收益 / 成本"排序）

每条都写四件事：兼容到什么程度、安全边界、和极简原则有没有冲突、碰到哪些 ADR。

### ① 读 `.agents/skills`，并放宽 `name` 和警告（收益高，成本低）
- **做什么。**
  - 发现顺序改为：用户级 `~/.radian/skills` → `~/.agents/skills` → `extra_paths` → 项目 `.radian/skills` → 从工作区到仓库根各级的 `.agents/skills` → 内建。
  - 和 ADR 0025"用户级优先"一致：项目级不能遮住用户级的同名 skill。
  - `name` 缺失时用目录名，再用现在的 `valid_name` 检查。
  - `allowed-tools` 等未实现字段的警告，合并成一条："N 个 skill 声明了 allowed-tools，已忽略"。
  - 和 8-extensibility ③（项目级接线）一起做，代码路径是同一条（`discover_with_extras`，`mod.rs:106-133`）。
- **兼容程度。** 只读。正文照样经 `skill` 工具读取，不认 Claude 的扩展字段（`context`、`hooks`、`` !`cmd` ``），读到时只警告，不执行。
- **安全。**
  - 项目里的 `.agents/skills` 加进 ADR 0031 T1 清单和 `has_project_resources`（`trust.rs:430-434`）：未信任时不加载。这和 pi 一致。
  - `~/.agents/skills` 是用户自己的目录，不进门。但沙箱要能读到它，因为 skill 附带的脚本要在沙箱里跑。这需要扩展 ADR 0020 的可读清单，做法和 ADR 0025 裁决 1 给 `~/.radian/skills` 开的口子一样。
  - skill 不能给自己授权，这条不变（裁决 4）。
- **和极简的冲突。** 很小：多两个发现根，不加新概念。`.agents` 是厂商中立的位置，不违背裁决 2"不默认读别家目录"的本意。这一点要在修订里写明。
- **ADR。** 0025 修订（发现位置、`name` 缺省值）、0031 T1 清单、0020 可读范围。
- **验证。**
  - 已信任时，`.agents/skills/x` 出现在 `/skills` 列表里；未信任时不出现，并在启动行写明原因。
  - 没有 `name` 的 skill 用目录名加载。
  - 10 个带 `allowed-tools` 的 skill 只产生 1 条警告。
  - 变异检查：去掉信任检查，"未信任不加载"的测试应该失败；去掉 `name` 的缺省值，用目录名加载的测试应该失败。

### ② MCP 配置接受 `mcpServers` 结构（收益高，成本中）
- **做什么。**
  - `--mcp-config` 和将来接上的 `[mcp]` 节（8-extensibility ⑤a）都接受标准的 `{"mcpServers":{"<name>":{command,args,env,cwd}}}`，现有的 `servers` 数组继续兼容。
  - `${VAR}` 和 `${VAR:-默认值}` 由宿主在加载时展开，语义照 Claude Code 文档。
  - `type` 是 `http`、`sse` 或 `ws`，或者带 `url` 的条目：报一行"Radian 只支持 stdio，已跳过"，不让整个会话失败。
  - 项目根的 `.mcp.json`：信任后才读，并且照 Codex 的 hook 做法按内容哈希确认，文件变了就重新问。
- **兼容程度。** 只读 stdio 子集。不写回 `~/.claude.json`，也不支持 OAuth。
- **安全。**
  - 保留"环境变量白名单"。`env` 里显式写出的变量（包括 `${VAR}` 展开后的值）照常传；其余宿主环境照旧不传。
  - **推断（未验证）**：`npx`/`uvx` 启动的 server 通常需要 `HOME`。这里给 `mcpServers` 条目一个缺省的 `inherit_env = ["HOME"]`，并写进 ADR，不要悄悄把整个环境都继承过去。
  - MCP 是可执行内容，ADR 0031 T1 早就写了"进门、更该门控"。
  - 照 Claude 文档的做法，`ANTHROPIC_API_KEY` 这类凭据变量在项目条目里展开为空。
- **和极简的冲突。** 中等：要多维护一种输入结构和一个展开器，但运行时只有一种内部表示。相比继续用自有结构，用户省下的是"每个 server 都手工翻译一遍"。
- **ADR。** 0008（MCP 配置节）、0031（`.mcp.json` 进门）、0012（凭据不外泄）。
- **验证。**
  - 用 Claude 文档里的示例 `.mcp.json` 做 fixture，stdio 条目能启动，http 条目被跳过并给出警告。
  - 未信任时，`.mcp.json` 不启动任何进程。
  - 内容改了之后，会重新询问。
  - 没设的 `${VAR}` 有警告。
  - 变异检查：继承全部环境，"没列出的变量不出现"的测试应该失败。

### ③ hook 用 Codex 同构的子集（收益中高，成本中；在 8-extensibility ①② 的基础上做）
- **做什么。** 8-extensibility 已经提出 `post_tool`、`pre_tool`、`stop` 三个命令 hook。这里把字段对齐：
  - 配置可以直接写 Claude/Codex 的 JSON 子集：事件名 `PreToolUse`、`PostToolUse`、`Stop`，加 `matcher` 和 `hooks[{type:"command",command,timeout}]`。这正是 Codex 导入器筛出来的那个子集（`hooks_cla.rs:113-170`）。
  - stdin 用 Claude 的字段名：`hook_event_name`、`tool_name`、`tool_input`、`tool_response`、`cwd`、`session_id`。
  - exit 0、exit 2、其他退出码，以及 `hookSpecificOutput.permissionDecision:"deny"` 和 `updatedInput`，含义都和 Claude 一样。
  - **`allow` 和 `ask` 拒收**，和 Codex `output_parser.rs:450-459` 一致，也符合 ADR 0012 P1"不能放宽"。
  - `matcher` 只认 Radian 的工具名。
  - 工具名和参数名**不做运行时别名**。有两个可选的办法，交给 定：(a) 在导入命令里把 `Edit|Write` 翻成 `patch|write`、把 `Bash` 翻成 `shell`，并提示"脚本里的 `.tool_input.file_path` 要改成 `.path`"；(b) stdin 里额外给一个 `tool_input_claude` 视图。推荐 (a)，(b) 等于为别家的 schema 背长期包袱。
- **兼容程度。** 读配置的子集。外部脚本需要按工具名和参数名改一遍，这一点在文档里明说。
- **安全。**
  - 用户级 hook 直接生效。
  - 项目级 hook 要经过信任门，并按内容哈希确认（Codex `discovery.rs:718-738`）。
  - `workspace` 档位下，hook 在沙箱里跑。
  - 永远不读 `.claude/settings.json` 里的 hook。别家的项目 hook 要进来，只能经过 ④ 的导入，由用户自己执行。
- **和极简的冲突。** 中等。只开 3 个事件，不做 33 个；不做 `async`、`if`、`http`、`prompt` 这类 handler。
- **ADR。** 和 8-extensibility ①② 合写在同一份新 ADR 里；涉及 0012、0031、0011。
- **验证。** 用一份"Claude 文档示例 hook 配置"的 fixture，确认能被解析。`allow` 输出被拒收并给出警告。exit 2 时调用被拒绝，原因进入结果。

### ④ `radian import claude`：一次性翻译（收益中，成本中；依赖 ①–③ 先有目标格式）
- **做什么。** 照 `import-pi` 的骨架和 Codex 的 10 类导入，只做能保真的几类：
  - `.claude/commands/*.md` 和 `~/.claude/commands` → skill。frontmatter 补上 `name`；正文里的 `$ARGUMENTS` 保留，前提是 8-extensibility ③ 已经支持它，否则在报告里列出。
  - `.claude/skills` → 只在配置里写一条 `extra_paths` 指过去，不复制文件。
  - `.mcp.json`、`~/.claude.json` 里的 `mcpServers` → ② 的 Radian 配置。
  - `settings.json` 里的 `hooks` → ③ 的子集，工具名照 ③(a) 翻译。
  - 每一条被丢弃的内容（`if`、`async`、http hook、`context: fork`、agents、plugins、LSP……）都在报告里单独列一行。
- **兼容程度。** 读源文件，写 Radian 的**新**文件。不覆盖已有文件（`--force` 除外），不改源文件，不导入会话。
- **安全。**
  - 照 `import-pi` 的规则：不读凭据文件，不复制密钥字面值（改成环境变量引用），不执行任何命令。
  - 导入结果落在用户级配置里。项目级产物（例如项目里的 hook）仍然要经过信任门。导入不等于信任。
- **和极简的冲突。** 低。翻译逻辑只存在于这个命令里，运行时只认 Radian 自己的格式。这和 ADR 0029 第 3 条"翻译是一次性转换，不是运行时兼容层"是同一个原则。
- **ADR。** 新 ADR，"外部 agent 配置导入"；援引 0029 修订 2、0008 §5.4（不写回）。
- **验证。** 准备一个 Claude 配置的 fixture 目录，生成的 golden 草稿要能被正式读取器接受；丢弃清单要完整；读取路径的日志里永远不出现 `.credentials.json` 或 `auth.json`。

### ⑤ CLAUDE.md 的 `@path` 导入（收益中低，成本低）
- **做什么。** 在 `context.rs` 里照 Claude 文档的语义展开导入：
  - `@` 必须在行首或空白之后；相对路径按所在文件的目录解析。
  - 最多 4 层；跳过行内代码和围栏代码块；循环导入只展开一次。
  - **只展开仓库内的路径**，指向仓库外的导入保留原样，并在启动行列出（ADR 0036 不读仓库外的文件）。
  - 展开后的字节计入 128 KiB 的总预算。
- **兼容程度。** 只读。`.claude/rules` 的 `paths:` 和 `CLAUDE.local.md` 都不做。前者需要"按文件加载"这套新机制；后者是 Claude 私有的约定。
- **安全。** 上下文文件本来就不进信任门，按不可信内容处理；只在仓库内展开，不会扩大读的范围。
- **和极简的冲突。** 很低，一个约 100 行的纯函数。
- **ADR。** 0036 的第 4 个修订。按 AGENTS.md 的约定，第三次修订之后要整篇改写，0036 已经是改写过的格式，追加修订即可。
- **验证。**
  - `@docs/a.md` 被展开；写在反引号里的 `` `@README` `` 不展开。
  - 第 5 层的导入不再展开。
  - 指向仓库外的导入不读。
  - 变异检查：去掉深度上限，循环导入的 fixture 应该挂住超时。

### ⑥ ACP 服务端 `radian acp`（收益看用户群，可能较大；成本中高）
- **为什么。** 列表里有 43 个 agent，opencode 是原生的，pi、Codex、Claude Code 走适配器。编辑器用户是 Radian 现在完全触达不到的人群，而 `radian serve` 这个子命令已经预留了（`cli.rs:374`）。
- **代价。** 要把审批（ADR 0024）映射到 ACP 的 `request_permission`，把 inbox/steer（0011）映射到 `session/prompt`，live 事件（0010）映射到 `session/update`。这不属于格式兼容，是一套新的宿主面。建议先像 pi 那样做一个外部适配器验证需求，不进主二进制。
- **ADR。** 新 ADR；涉及 0032（一个二进制）、0009（全屏 TUI）。

### ⑦ 不值得做的
- **运行时读 `.claude/` 整个目录树（omp 的做法）。** 名字像是兼容，格式其实对不上（omp 的 hooks 目录就是例子）；项目级没有门；还会把"Claude 改了一个字段"变成 Radian 的 bug。ADR 0025 裁决 2 已经否掉了这条路。
- **`.cursorrules`、`GEMINI.md`、copilot-instructions、`.windsurfrules`。** AGENTS.md 已经赢了，连 Claude Code 都改成读它。这些文件的用户可以自己建一个 AGENTS.md 软链接，Radian 会按真实路径去重。
- **Claude 插件和 marketplace 的完整兼容。** 组件有 13 类（`bin/` 会加进 PATH，还有 monitors、LSP、userConfig 的安全存储……），每一类都是一套新的执行面。如果要做，只考虑 **Agent Plugins 1.0 里 skills + MCP 那一部分**：等 ①② 落地后，读插件根目录的 `plugin.json` 就几乎没有额外成本。在有用户提出要求之前不做。
- **`.claude/agents`、自定义命令目录。** ADR 0043 已经删掉了自定义 agent 和 `prompts/`，这里没有反证；命令的需求由 ④ 转成 skill 来覆盖。
- **导入别家的会话。** 各家 JSONL 的语义差别很大（工具名、reasoning、压缩记录），翻译出来也很难通过 journal-selfcheck 的重放。收益只在"迁移那一天"，不划算。
- **`allowed-tools` 预授权。** 维持 ADR 0025 裁决 4。规范自己也标着 Experimental。
