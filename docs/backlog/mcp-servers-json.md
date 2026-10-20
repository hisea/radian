# MCP 配置接受 `mcpServers` 结构

- 状态：idea
- 涉及 ADR：0008（MCP 配置节与 `[mcp]`）、0031（项目级 `.mcp.json` 进信任门）、0012（子进程环境白名单、凭据不外泄）、0043（MCP 不进沙箱）
- 来源：`docs/research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.3、§6 ②；总报告 README 第 31 条

## 背景

今天的行为（2026-10-20 对 `main` 核实）：

- MCP server 只能从命令行 `--mcp-config <FILE>` 读，结构是 Radian 自有的 `{"servers":[{"id","command","args","env","inherit_env","shared_tools","call_timeout_secs","shutdown_timeout_secs"}]}`，`deny_unknown_fields`（`crates/radian-host/src/mcp.rs`）。配置文件里的 `[mcp]` 节被跳过并列在启动行（`config/mod.rs` 的 `skipped`）；`import-pi` 不迁移 `mcp.json`。
- 子进程环境是白名单：只传 `PATH`、`inherit_env` 里列出的变量和 `env` 里显式写的值（`crates/agent-extensions/src/process.rs`）。
- 主流客户端（Claude Code、Cursor、pi、omp）都用 `{"mcpServers":{"<name>":{command,args,env,…}}}`，键是名字。Claude Code 还支持 `${VAR}` 与 `${VAR:-默认值}` 展开和 `type: stdio|http|sse|ws`。Codex 只在导入时读这个结构。
- 结果：一份现成的 `.mcp.json` 拿过来一个字也用不了，每个 server 都要手工翻译一遍。

## 方向（研究报告建议，owner 未定）

- `--mcp-config` 与将来接上的 `[mcp]` 节都接受标准的 `mcpServers` 对象；现有 `servers` 数组继续兼容。运行时内部只有一种表示（`McpServerConfig`），两种输入在加载时归一。
- `${VAR}` 与 `${VAR:-默认值}` 由宿主在加载时展开，语义照 Claude Code 文档；未设且无默认值的变量给一行警告。
- `type` 为 `http`/`sse`/`ws`、或带 `url` 的条目：报一行"Radian 只支持 stdio，已跳过"，不让整个会话失败。只做 stdio 子集，不写回 `~/.claude.json`，不做 OAuth。
- 项目根的 `.mcp.json`：加进 ADR 0031 T1 的触发清单，受信任后才读；并按内容哈希确认（Codex 对项目 hook 的做法），文件变了就重新问。MCP 是可执行内容，比 skill 更该进门。
- 环境白名单不变：`env` 里显式写出的变量（含 `${VAR}` 展开后的值）照常传，其余宿主环境照旧不传。照 Claude 文档的做法，项目条目里引用 `ANTHROPIC_API_KEY` 这类凭据变量时展开为空。

## 待决问题

- `mcpServers` 条目要不要给一个缺省的 `inherit_env = ["HOME"]`。报告的推断（未验证）是 `npx`/`uvx` 启动的 server 通常需要 `HOME`；若给，必须写进 ADR，不能悄悄继承整个环境。
- 用户级 `mcpServers` 的位置：只走 `[mcp]` 节，还是也读 `~/.radian/mcp.json`。
- 与 `mcp-full-tier-ask.md`（`full` 档按 server 首次询问）的关系：项目级 `.mcp.json` 的信任门和首次询问是两道门还是合成一道，落地时一起定。

## 验收

- 用 Claude 文档里的示例 `.mcp.json` 做 fixture：stdio 条目能启动；http 条目被跳过并给出警告；现有 `servers` 数组的 fixture 结果不变。
- 未受信任的项目：`.mcp.json` 不启动任何进程；内容改了之后会重新询问。
- 没设的 `${VAR}` 有警告；凭据变量在项目条目里展开为空。
- 变异检查：改成继承全部环境，"没列出的变量不出现在子进程里"的测试必须失败。
- 落地时同分支改 ADR 0008、0031（docs-only commit）并删掉本文件。
