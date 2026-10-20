# 深挖 8：扩展机制（hooks、插件、skills、MCP、自定义命令与 agent）

2026-10-18 · Radian `356df1e` · 对手：pi-mono `482c138`（另核对 pi 1.3.0 npm 包）、Codex `8c07694`、opencode `63c1a27`（`packages/core`、`packages/plugin`）、omp 18.12.1 npm 包；Claude Code 只用官方文档。
下文 `rad/` = `/home/claude/radian/crates`，`comp/` = 对手源码根目录。
**说明：** `comp/codex` 的工作区文件和它的 git HEAD 对不上（目录里放的是 radian 的 crate），所以 Codex 的源码全部用 `git show HEAD:<path>` 读，行号以 HEAD 为准。
不重复 4-architecture.md（"没有用户 hook"、方案④的概要）和 2-tools.md（"MCP 不能按需加载"、P8 tool_search）里已有的结论，只往下挖。

## 1. 一句话结论

Radian 的 `Hooks` trait 已经具备 Claude Code `PreToolUse`/`PostToolUse` 的全部能力：拒绝、改参数、直接给结果、改写结果，而且错误一律 fail closed。缺的只是一个**把外部命令接到这个 trait 上的适配层**，再加一个能让模型"别停、先过验证"的 stop 钩子。这两样能让用户自己加"编辑后 lint"和"收尾跑测试"，复杂度最小，生态上也最划算：Codex 和 omp 都已经直接读 Claude Code 的格式。
另外有三个缺口，代价都小、收益确定，比做插件系统值得：项目级 skill 已经定了但还没接线；MCP 只能用命令行参数配置，server 按顺序启动，一个失败整个会话就失败；`explore` 子会话会重启全部 MCP server，并且拿到全部 MCP 工具。
进程内插件系统（pi、omp、opencode）和插件市场不值得做。

## 2. 对比表

| 维度 | Radian | pi | Codex | Claude Code（文档） | omp | opencode |
|---|---|---|---|---|---|---|
| 用户 hook 事件数 | 0；内部 trait 有 4 个可 await 的点加 1 个通知点（`rad/agent-core/src/hooks.rs:20-26,58-100`） | 41 个扩展事件（`coding-agent/src/core/extensions/types.ts:1597-1664`；1.3.0 的 `types.d.ts` 也是 41 个，brief 写的 25 个、4-architecture 写的 35 个都不对） | 12 个（`codex-rs/hooks/src/lib.rs:25-38`） | 33 个（[hooks](https://code.claude.com/docs/en/hooks)） | 扩展 42 个 + 旧式 hooks 23 个（`src/extensibility/extensions/types.ts`、`hooks/types.ts`，按 `on(event:` 计数） | 5 个运行时域：session 10 个、tool `execute.before/after`、shell、permission、aisdk；另有 11 个 transform 域（`packages/plugin/src/promise/*.ts`、`src/README.md:65-77`） |
| 进程内还是外部 | 只有编译期 Rust | 进程内 TS（jiti 加载，`loader.ts:576-581`） | 外部命令；或调用某个 MCP 工具（`config/src/hook_config.rs:166-200`） | 外部 command/http/mcp_tool，另有 prompt/agent；插件可以带 JS hooks 模块 | 进程内 TS（Bun） | 进程内 TS/Effect |
| 拦截或改写工具调用 | trait：`Deny`/`Rewrite`/`Respond`（`hooks.rs:39-51`） | `tool_call` 可以 block，也可以原地改 `event.input`，改完**不再校验**（`types.ts:1253-1255,1454-1463`） | `PreToolUse` 可以 block，也可以给 `updated_input`（`hooks/src/events/pre_tool_use.rs:44,129-132`） | `updatedInput`；exit 2 表示阻断 | 同 pi | `execute.before` 可以抛 `Tool.Error` 拒绝（`core/src/plugin/hooks.ts` 的注释） |
| 改写结果或补上下文 | trait：`after_tool_call` 拿到 `&mut ToolResult`（`hooks.rs:89-96`） | `tool_result` 可以替换 content、details、isError（`types.ts:1483-1489`） | `PostToolUse` 可以 block、给 feedback、给 `additionalContext`，超过 2500 token 就写到文件里（`post_tool_use.rs:42-44`，`output_spill.rs:13`） | `updatedToolOutput` 和 `additionalContext` | 同 pi | `execute.after` 可以改 `result`（`plugin/src/promise/tool.ts:47-63`） |
| "完成前"的验证钩子 | 没有（`on_turn_end` 是同步的、没有返回值，`hooks.rs:98-99`） | `agent_before_settle`、`turn_end` 都带返回值（`types.ts`） | `Stop`（lib.rs:35） | `Stop` 返回 exit 2 时不让停，把 stderr 交给模型 | 有 | 未核实 |
| 每次调用的开销 | 内部 trait：静态分发，可以当作零 | 进程内 await，**没有超时**（`runner.ts:1249-1266`） | 起一个子进程；缺省超时 600 s（`engine/discovery.rs:861`）；匹配到的多个 hook 并发执行（`dispatcher.rs:125` FuturesUnordered） | command 缺省超时 600 s，prompt 30 s，agent 60 s；匹配到的 hook 并行跑；可以 `async: true` | 每个 handler 超时 30 s（`extensions/runner.ts:133`） | 进程内 |
| 信任 | 不适用 | project trust 管 `.pi/extensions`（`docs/packages.md:19-21`） | 每个 hook 按内容哈希做信任审查（`discovery.rs:718-738`） | 项目级 hook 要先信任目录 | 未核实 | 未核实 |
| 插件或包 | 没有 | `pi install npm:/git:/本地路径`，一个包可以带扩展、skills、模板和主题（`docs/packages.md:3-38`）；**gallery 里有 5337 个包**（[pi.dev/packages](https://pi.dev/packages)，2026-10-18） | 插件 = skills + `.mcp.json` + apps + `hooks.json`；**也读 `.claude-plugin/plugin.json`**（`core-plugins/src/manifest.rs:386-446,685`） | `.claude-plugin/plugin.json` + marketplace；官方 marketplace 首次启动时自动添加（[plugins](https://code.claude.com/docs/en/plugins)） | bun 安装器，另外读 `.claude`、`.cursor`、codex、gemini、opencode 的配置（`src/discovery/*.ts`，`claude.ts:1-6`） | npm 插件；`effect` 必须做 peer 依赖（`packages/plugin/README.md`） |
| skill 的发现位置 | `~/.radian/skills`、`[skills] extra_paths`、内建 1 个（`rad/radian-host/src/skills/mod.rs:106-133`）；**不读项目级** | 用户级、`.pi/skills`、祖先目录里的 `.agents/skills`（ADR 0025 对照表） | 用户级、项目级、`.system` 内建、cloud（`skills/src/lib.rs:60-62`） | 用户级、项目级（一直往上到仓库根）、嵌套子目录（读到该目录的文件时才加载）、插件、`--add-dir` | 自己的目录，加上 `.claude/skills` 等（`discovery/claude.ts:199-243`） | 有 `SKILL.md` 扫描（`core/src/config/plugin/skill-scan.ts:29`） |
| skill 目录占多少上下文 | 软上限 1000 token，超出的按顺序截掉并给出剩余数量（`mod.rs:24,501-534`）；正文用 `skill` 工具读 | 只放名字、描述和路径，正文用 read 工具读（`docs/skills.md:41-43`） | 上下文窗口的 2%，没有窗口信息时 8000 字符，最多 1 万 token（`ext/skills/src/render.rs:19-22,126-140`）；另外在影子模式下试验廉价选择器（BM25、n-gram、LRU，`dynamic_skill_selector.rs`） | 每条描述截到 1536 字符；压缩后重新附上每个 skill 的前 5000 token，合计 2.5 万（[skills](https://code.claude.com/docs/en/skills)） | 未核实 | 未核实 |
| skill 参数和动态注入 | `/skill:x args`：args 原样接在正文后面（`mod.rs:548-585`） | 模板支持 `$1`、`$@`、`${1:-默认值}` | — | `$ARGUMENTS`、`$N`、`` !`cmd` ``（缺省超时 2 min）、`context: fork`、frontmatter 里的 `hooks` | — | — |
| 自定义 slash 命令 / 提示模板 | **没有**；`.radian/prompts/` 被 ADR 0043 §1 删掉了 | `prompts/*.md`，文件名就是命令名（`docs/prompt-templates.md`） | 本版没找到 `~/.codex/prompts` 的加载代码（未核实，可能已经移除） | 已经并入 skills，`.claude/commands/*.md` 仍然可用 | `custom-commands/`、`slash-commands.ts` | `{command,commands}/**/*.md`（`config/plugin/command.ts:144-159`） |
| 自定义 agent | 不做（ADR 0043 1a.2）；只有 `explore` 和 `general` | 没有内建，只有扩展示例 | `agents/*.toml` 角色文件（`agent-roles/src/discovery.rs`），内建 explorer | `.claude/agents/*.md`，可以设置 tools、model、mcpServers、hooks、skills、isolation 等 | 有 | `{agent,agents}/**/*.md`（`config/plugin/agent.ts:22`） |
| MCP 传输方式 | 只有 stdio（`agent-extensions`） | stdio + http + OAuth（`docs/mcp.md:18,62,115`） | stdio + streamable HTTP | stdio、http、ws；sse 已弃用 | stdio、http；有 smithery 注册表 | stdio + streamable HTTP（`core/src/mcp/client.ts:10,272`） |
| MCP 的配置位置 | **只能用 `--mcp-config` 文件**；配置文件里的 `[mcp]` 被跳过（`rad/radian-host/src/config/mod.rs:313-315`） | 用户级或项目级（项目级受信任门控制），`pi mcp add` | config.toml，或者插件 | 用户级、项目 `.mcp.json`（交互模式下要人确认）、插件、agent frontmatter | 读 `.mcp.json` 等多家的格式 | config |
| MCP 启动 | **按顺序启动**，每个 server 超时 30 s；**任意一个失败，整个会话就打不开**（`agent-extensions/src/session.rs:70-104`，`config.rs:46`，`rad/radian-host/src/tools.rs:183-184`） | 后台连接；首次提示只为 `direct` 工具等最多 10 s（`docs/mcp.md:100`） | 用 JoinSet 并发启动（`codex-mcp/src/connection_manager.rs:331,863`），启动 30 s、调用 300 s（`rmcp_client.rs:104-105`） | 启动超时由 `MCP_TIMEOUT` 决定 | **最多等 250 ms**，没连上的 server 先用缓存的工具定义（缓存放在 agent.db，30 天），连上后再补（`mcp/timeout.ts:5`，`manager.ts:989-1015`，`tool-cache.ts`） | 带 startup/catalog 超时（`client.ts:218,318`） |
| MCP 工具名 | `server__tool`，非法字符换掉后加 8 位哈希，上限 64 字符（`agent-extensions/src/alias.rs:77-97`） | `mcp__<server>` 命名空间 | `mcp__server__tool`（`codex-mcp/src/mcp/mod.rs:66-85`） | `mcp__server__tool`；插件里的 server 是 `mcp__plugin_<p>_<s>__tool` | 未核实 | 未核实 |
| MCP 结果截断 | 进入上下文时**不截断**（`map_result`，`session.rs:535-580`）；只有压缩时才截到 2000 字符（`agent-core/src/compaction.rs:48`） | 未核实 | 按模型的 truncation_policy 截断（`core/src/tools/handlers/mcp.rs:236-240`） | 缺省 2.5 万 token，超过 1 万 token 时警告；超过 5 万字符就写进文件，模型只拿到文件引用 | 未核实 | 未核实 |
| subagent 怎么用 MCP | 每次调用都给子进程传 `--mcp-config`，所以 server 全部重启（`rad/radian-host/src/subagent.rs:390-393`）；**`explore` 也会拿到全部 MCP 工具**（推断，见 §5.3） | 不适用 | 进程内 thread（未核实是否共享连接） | 按名字引用的 server 共享父会话的连接；inline 定义的 server 在子 agent 启动时连接，结束时断开 | 子会话共享同一个 MCPManager（`mcp/manager.ts:418-423`） | 进程内 |

## 3. 每家的关键做法（只列和"快、有效率"有关的）

### pi
- 扩展点最全，但每个 handler 都在主线程上 await，没有超时（`runner.ts:1249-1266`）。一个慢的 `tool_call` handler 会直接拖住整轮。这样做的代价换来的是零进程开销，以及完整的 API：`registerTool`、`registerCommand`、`setActiveTools`、`registerMcpServer`、`exec`……共约 30 个方法（`types.ts:1666-1900`）。
- 生态是 pi 最大的护城河：gallery 有 5337 个包，其中 `pi-lens` 做的就是"编辑后 LSP/lint/format/类型检查"。可见"编辑后诊断"是社区自发补出来的第一批需求之一。
- MCP 有三种暴露方式：`direct`、`deferred`（经 `tool_search` 加载）和 `codemode`（缺省）。server 列表追加到对话末尾，而不是去改工具声明，"so earlier messages stay cached"（`docs/mcp.md:189-204`）。

### Codex
- hook 只有外部命令和 MCP 工具两种。`Prompt {}` 和 `Agent {}` 在 schema 里有，但加载时会报 "prompt hooks are not supported yet" 并跳过（`discovery.rs:676-680`）。连 OpenAI 自己都还没做 LLM 类 hook。
- 两处工程细节值得抄：一是缺省超时 600 s，SessionEnd 单独夹紧（`discovery.rs:841-861`）；二是 `additionalContext` 超过 2500 token 就写到文件里，只给模型一段预览和路径（`output_spill.rs:13`、`hook_config.rs:177-186`）。后者防止一个吵闹的 lint 把上下文撑爆。
- 每个 hook 都要按内容哈希做信任审查（`discovery.rs:718-738`）：项目里的 hook 改了，就得重新确认。
- 插件格式和 Claude Code 对齐，直接读 `.claude-plugin/plugin.json` 和 `marketplace.json`（`manifest.rs:685`，`marketplace_tests.rs:7-10`）。

### Claude Code（来源：文档）
- 匹配到的 hook 并行执行；`if: "Bash(git *)"` 复用权限规则的语法；`async: true` 在后台跑，`asyncRewake` 在后台 hook 返回 exit 2 时把模型叫醒（[hooks](https://code.claude.com/docs/en/hooks)）。
- 有 `PostToolBatch` 事件，一批工具调用只触发一次，适合放 lint 这类重命令（同页的事件列表）。
- skill 能做的事最多：frontmatter 里可以带 `hooks`（调用之后对整个会话生效）、`` !`cmd` `` 预注入、`context: fork`、`allowed-tools`。自定义命令已经并入 skill（[skills](https://code.claude.com/docs/en/skills)）。
- MCP 缺省开 tool search，开始时只加载工具名和 server instructions；结果超过 5 万字符就写进文件（[mcp](https://code.claude.com/docs/en/mcp)）。subagent 按名字引用 server 时共享父会话的连接（[sub-agents](https://code.claude.com/docs/en/sub-agents)）。

### omp
- MCP 启动是几家里最快的：最多等 250 ms（`timeout.ts:5`）。没连上的 server 先用 30 天内缓存的工具定义顶上，后台连接完成后再触发 `onToolsChanged`。注释直接写了动机："a single unresponsive MCP server blocked startup for the full 30 s"（`manager.ts:1008-1015`）。
- 扩展 handler 有 30 s 超时（`runner.ts:133`），比 pi 多一层保护。
- 直接读其他工具的目录（`.claude`、`.cursor`、codex、gemini、opencode，`src/discovery/`），等于白拿别人的 skills、MCP 配置和规则。

### opencode
- **编辑后自动格式化是内建的**：edit 和 write 写完文件后调用 `formatter.file()`，返回的 diff 是格式化之后的内容（`core/src/tool/plugin/edit.ts:213,307-311`）。按扩展名匹配 13 个内建 formatter（prettier、biome、ruff、clang-format……，`formatter/builtins.ts`），命令模板里用 `$FILE`（`formatter.ts` 约第 63 行）。
- 插件是进程内的 Effect/Promise 程序，可以改 session context，例如"`delete event.tools.write`"、改工具描述（`plugin/src/README.md:113-124`）。这非常灵活，但插件必须和宿主共用同一份 `effect` 实例（`packages/plugin/README.md`），耦合很深。

## 4. Radian 的优势

1. **seam 已经存在，语义也是对的。** `before_tool_call` 支持 `Allow`、`Deny`、`Rewrite`、`Respond` 四种决定，改写后的参数**会重新校验**（`hooks.rs:45-47`；pi 改完 input 不再校验，`types.ts:1254`）。`after_tool_call` 的错误 fail closed，未经处理的输出不会流到模型那里（`hooks.rs:53-57`）。组合子 `(A, B)` 的语义写清楚了："first non-Allow wins"，后一个 hook 看到的是前一个 Rewrite 之后的参数（`hooks.rs:143-185`）。`after_tool_call` 在 `ToolExecutionSettled` 之前跑，可以被取消（`agent-core/src/turn.rs:2290-2346`），所以 hook 改写过的结果会进 journal，重放时结果一致。外部 hook 只是这个 trait 的又一个实现。
2. **skill 的渐进披露做得干净。** 只放名字、描述和路径；同样的输入一定生成逐字节相同的段落，提示缓存不会因为顺序抖动失效（`mod.rs:1-10`）；超出预算的条目截掉时会给剩余数量。1000 token 的预算比 Codex 的"2% 窗口"还紧，便宜。
3. **已经有一个"外部命令贡献者"的先例。** footer 的 `command` 用户命令：stdin 是固定 schema 的 JSON，只读 stdout 第一行，5 s 超时，`kill_on_drop`，输出不进 journal 也不进模型上下文（`rad/radian-host/src/command_footer.rs:1-52,376-379`）。外部 hook 可以照这套写法做：stdin JSON、超时、kill。
4. **MCP 客户端很扎实。** 进程组 kill 加回收线程（`agent-extensions/src/process.rs:1-20`）；进度通知做了单调去序（`session.rs:470-485`）；协议握手用 `Auto`，先试 2026-07-28 版，失败再退回 2025-11-25（`session.rs:184-189`）；别名稳定，增删别的工具不会改名（`alias.rs:68-76`）。
5. **信任门已经实现。** ADR 0031 的记录、解析和触发器都已落地（`rad/radian-host/src/trust.rs:1-18`），项目级资源要进门，代码上缺的只是"按信任结果去加载"这一步。

## 5. Radian 的劣势和缺口

1. **用户没法在两个最值钱的时刻插手：** 编辑之后、宣布完成之前。trait 能做，但没有配置入口。而 `on_turn_end` 是同步的、没有返回值（`hooks.rs:98-99`），就算用 Rust 写，也做不出"测试没过就别停"。
2. **项目级 skill 没接线。** ADR 0025 裁决 3 和 ADR 0031 T7 片 E 都已经定了：受信任的项目要读 `<workspace>/.radian/skills/`。信任触发器也已经把它当作触发条件（`trust.rs:15-17`）。但发现函数只接用户级、`extra_paths` 和内建三个来源（`mod.rs:106-133`；调用处 `session/open.rs:247-252`）。结果是信任门会弹出来问，问完却什么也不加载。"按项目加验证流程"最便宜的载体就是这里。
3. **`explore` 子会话不是真正的只读，而且每次都重启 MCP。** 子进程的参数里总是带着父会话的 `--mcp-config`（`subagent.rs:390-393`）；`--tools read,list,glob,grep` 只卸掉内建工具，MCP 的 spec "are never filtered here (the definition's `mcp` list governs them)"（`rad/radian-host/src/tools.rs:242-254`）。但 agent 定义已经被 ADR 0043 删掉了，这个 `mcp` 列表也就不存在了。`full` 档对 MCP 是 `Allow`（`radian-permissions/src/profile.rs:169`）。**推断（没有运行验证）**：`explore` 子会话能调用可写的 MCP 工具，这和 ADR 0043 1a.2 的"只读"相矛盾；而且每次调用都要把全部 server 按顺序重新启动一遍。
4. **MCP 配置和启动都很脆。** 只能用命令行参数（`config/mod.rs:313-315`；`migrate_pi.rs:128-131` 明确写着 pi 的 `mcp.json` 不迁移），没有项目级配置。server 按顺序启动，一个失败整个会话就打不开（`session.rs:70-80`、`tools.rs:183-184`）。拿 omp 的 250 ms 加缓存比，Radian 最坏要等 N × 30 s。不支持 http 传输。
5. **MCP 结果没有上限。** 内建的 shell 和 read 都有 64 KiB 的上限（ADR 0015 背景），MCP 结果却原样进入上下文，只在压缩时才截断。Claude Code 是 2.5 万 token 或 5 万字符转写文件，Codex 按截断策略处理。
6. **没有提示模板。** `/skill:x args` 只是把参数原样接在正文后面（`mod.rs:554` 起），没有 `$ARGUMENTS` 或 `$1` 这类替换，也没有 `` !`cmd` `` 注入。`.radian/prompts/` 已经被删掉了。
7. **生态为零，也不兼容任何现有格式。** `SKILL.md` 和 pi 兼容（ADR 0025 S1），但 hook、插件和 MCP 配置都不兼容。Codex 和 omp 都在主动读 Claude Code 的格式，Radian 没有。

## 6. 改进方案（按"收益/成本"排序）

### ① 外部命令 hook，只开 `post_tool` 和 `pre_tool` 两个事件（收益高，成本低到中）
- **做什么。** 配置写成 `[[hooks]] event = "post_tool" | "pre_tool"`、`match = "write|patch"`（工具名精确匹配，或用 `|` 分隔的列表）、`command`、`timeout_secs`（缺省 60；Codex 和 Claude Code 是 600，对编辑后的 lint 太长）。
  - 实现：在 radian-host 写一个 `CommandHooks: Hooks`，挂到现有的 `((perm, memory), skills)` 元组上（`session/open.rs:241-243`）。**不改 agent-core。**
  - stdin 用 Claude Code 的字段名：`{"hook_event_name","tool_name","tool_input","tool_response","cwd","session_id"}`，这样别人现成的脚本可以直接用。
  - `pre_tool`：exit 2 时转成 `ToolDecision::Deny{reason: stderr}`；stdout 是 `{"hookSpecificOutput":{"updatedInput":…}}` 时转成 `Rewrite`，改写后的参数会被 core 重新校验。
  - `post_tool`：exit 0 不做任何事；exit 2 时把 stderr 附在 `ToolResult` 后面，作为 `<hook name=…>` 块；超过 N 字节（建议 8 KiB，对应 Codex 的 2500 token）就截断，或者写进文件只给路径；其他退出码只给一行警告，不进入上下文。
- **为什么更快、更有效。** "编辑后马上拿到诊断"是 gap-analysis #3b 的证据（SWE-agent）。opencode 把格式化做成了内建行为（`edit.ts:307`），pi 社区专门做了 `pi-lens`。做成 hook 后，Rust 项目可以配 `cargo check -q --message-format short`，TS 项目配 `tsc --noEmit`，Radian 自己不需要内建任何语言的知识。
  - 延迟：在这台机器上实测，`sh -c` 起一个进程平均 2.7 ms，`python3 -I` 平均 22 ms，`node -e` 平均 24 ms（200 次和 20 次循环的平均值）。相对一次模型调用（秒级）可以忽略，真正的成本在命令本身。所以要有 `match`（只在编辑工具上触发），并且 hook 只在工具成功时运行。
  - 并发：`write` 和 `patch` 都是 `Exclusive`（`radian-coding/src/patch.rs:67`、`write.rs:45`），不会对同一个文件并发跑两份 hook。
- **安全。** v1 只读用户级配置。项目级 hook 按 ADR 0031 T1 进门，并且照 Codex 的做法，内容哈希一变就要重新确认（`discovery.rs:718-738`）。hook 进程的沙箱与档位一致：`workspace` 档时放进沙箱（复用 shell 的启动路径）。
- **不做的。** 33 个事件、http 和 prompt 类 handler（Codex 自己都标着"not supported yet"）、async 和 rewake、改写 MCP 结果。
- **ADR。** 需要新 ADR，涉及 0012（hook 不能扩大权限：`pre_tool` 的 Rewrite 之后，权限 hook 会重新评估改写后的参数，因为 `CommandHooks` 排在 `PermissionHooks` 前面）、0031（进门清单加一项）、0002（结果在 Settled 之前改写，journal 记录的是最终结果）。
- **验证。** 用 fixture 脚本做集成测试：`pre_tool` 返回 exit 2 时调用被拒，原因进入结果；`updatedInput` 生效，非法的改写被 core 的校验拒绝；`post_tool` 的输出被截断并带标记。变异测试：Rewrite 后不再重新评估权限，应有一条测试失败；超时之后不 kill，测试应因为挂住而超时；成功时也附加输出，测试应失败。A/B：同一组 Rust 任务，开和关 `cargo check` hook，统计"编译失败后的额外轮数"和墙钟时间。

### ② stop 钩子："收尾前先过验证"（收益高，成本中）
- **做什么。** `[[hooks]] event = "stop"`。turn 正常结束时（`RunOutcome` 为完成），由 radian-runtime 的 driver 运行这个命令，不经过 `Hooks::on_turn_end`，因为后者是同步的、没有返回值。exit 2 时，把 stderr 包成一条 steer 放进 inbox，按 ADR 0011，"waking steer 会自己开下一轮"。上限是连续 N 次（建议 3 次，对应 Claude Code 的 `stop_hook_active` 防循环），用尽后只提示人。
- **为什么。** 这是"自定义验证"最直接的形态：`cargo test -q` 不过，模型就不能宣布完成。它不需要模型自觉去跑测试，也不需要一个 verify 运行器。ADR 0043 删掉的 verify 运行器（远期那一节）可以用这个十几行配置的机制替代。
- **ADR。** 新 ADR（可以和 ① 合在一起）；涉及 0011（steer 的来源要新增 `InputSource::Hook`，或者复用 System）和 0017（hook 失败不等于模型错误）。
- **验证。** 用一个 fixture 命令，前两次 exit 2、第三次 exit 0，断言恰好多跑了两轮；永远 exit 2 时在第 N 次停下。变异测试：去掉 N 的上限，测试应该挂住并超时。

### ③ 接上项目级 skill，并支持 `$ARGUMENTS`（收益中，成本低）
- **做什么。** 实施 ADR 0031 T7 的片 E：信任时，在 `discover_with_extras` 前面加上 `<workspace>/.radian/skills`，新增一个 `SkillSource::Project`，冲突时用户级优先。`expand` 支持 `$ARGUMENTS`、`$1`…`$9`；正文里一个占位符都没有时，保持现在"原样追加"的行为。
- **为什么。** 一个项目的"怎么跑测试、怎么发版"放进仓库里的 skill，所有人第一轮就能拿到，省掉模型自己摸索的若干轮。`$ARGUMENTS` 让 skill 兼做提示模板，不需要恢复 `.radian/prompts/`（和 Claude Code 把命令并进 skill 的方向一致）。
- **成本。** 约 100 行加测试，不需要新的决定：ADR 0025 裁决 3 和 0031 片 E 已经定了；`$ARGUMENTS` 是 0025 S3 的小修订。
- **验证。** 已信任时项目 skill 被列出；未信任时不列，并在启动行写明（`trust.rs` 已经有触发器）。变异测试：去掉信任检查，"未信任时不加载"的测试应该失败。

### ④ 修 subagent 的 MCP：`explore` 不挂 MCP，`general` 显式选择（收益中，成本低）
- **做什么。** `Kind::Explore` 不再传 `--mcp-config`（`subagent.rs:390`）。`general` 维持现状，但要测一下启动开销，结果写进 `docs/perf-baseline.md`。
- **为什么。** 一是兑现 ADR 0043 1a.2 的"只读"，二是省掉每次 `explore` 都要重启 N 个 server 的时间。`explore` 正是模型最常并行起多个的那一种。
- **ADR。** 这只是让实现符合 0043 1a 的原意，不需要新决定；补一条验收即可。
- **验证。** `explore` 子会话的 `SessionCreated` 或者工具表里没有任何 `server__*`。变异测试：恢复传参，测试应该失败。

### ⑤ MCP 的配置、启动和结果上限（收益中，看用户；成本中）
- **做什么。**
  - (a) 把 ADR 0008 里已有的 `[[mcp.servers]]` 接上（fixture `docs/schema/config/minimal.toml:10` 已经存在）；项目级的 MCP 配置按 0031 T1 进门。
  - (b) server 并发启动（照 Codex 的 JoinSet）；单个 server 失败只给警告并跳过，不让整个会话失败。要不要加一个 `required = true` 的开关，留给 ADR 定。
  - (c) MCP 文本结果套用 read 的 64 KiB 上限，截断时带标记。
  - 按需加载见 2-tools P8，不在这里重复。http 传输等真有需求再做。
- **为什么。** 现在接 MCP 的成本是"每次启动都写参数，按顺序等，任何一个坏了就整个起不来"，所以 MCP 在 Radian 里基本没人用。omp 在注释里给出了实际教训（30 s 卡住启动，`manager.ts:1008-1013`）。
- **ADR。** 0008（接线）、0031（进门）；(b) 改的是 `agent-extensions` 的 `open_session` 语义，ADR 里要写清楚。
- **验证。** 两个 server，一个故意 exit 1，会话仍然能打开并给出警告；两个都 sleep 1 s 的 server，总启动时间小于 1.5 s；返回 1 MiB 的 MCP 工具被截断并带标记。

### ⑥ 不值得做的
- **进程内插件系统（pi、omp、opencode 那种）。** 需要在 Rust 里内嵌脚本运行时，或者要求用户写 Rust；API 面也会远超 `agent-core` 的边界（pi 有约 30 个方法、41 个事件）。pi 的 handler 甚至没有超时。①② 用两三个外部命令事件就能覆盖"lint、验证、拦截"这三个核心需求。
- **插件包管理和 marketplace。** pi 的 5337 个包靠的是 TS 生态和进程内 API，Radian 没法复用。如果将来想吸纳生态，最便宜的做法是**兼容格式**（stdin 用 Claude Code 的 hook 字段、`SKILL.md`、`.mcp.json`），而不是自建一个市场。Codex 和 omp 走的就是这条路。
- **自定义 agent 类型。** ADR 0043 1a.2 已经否掉了，这里也没有发现足够强的反证。真正需要的"`explore` 用更便宜的模型"已经在 0043 里了。
- **prompt 和 agent 类 hook、`` !`cmd` `` 注入。** 前者 Codex 也还没做；后者可以用 skill 正文让模型自己去跑 shell，多一轮，但少一套执行和权限路径。
- **33 个事件、async/rewake、PostToolBatch。** 等 ① 落地、真有人反馈"每次编辑都跑太慢"时，再考虑加一个"每批一次"的触发方式。
