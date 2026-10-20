# MCP 配置接受 `mcpServers` 结构

- 状态：decided＼�
- 顺序：生态兼容四件事的第 2/4＼�按 `context-at-imports` → `mcp-servers-json` → `command-hooks-claude-subset` → `import-claude` 做；ACP 暂定不做）
- 涉及 ADR：0008（MCP 配置节与 `[mcp]`）、0031（项目级 `.mcp.json` 进信任门）、0012（子进程环境白名单、凭据不外泄）、0043（MCP 不进沙箱）
- 来源：`docs/research/2026-10-18-area-deep-dive/11-ecosystem-compat.md` §5.3、§6 ②；总报告 README 第 31 条；结果上限来自 `8-extensibility.md` §5.5、§6 ⑤c＼�

## 背景

今天的行为（2026-10-20 对 `main` 核实）：

- MCP server 只能从命令行 `--mcp-config <FILE>` 读，结构是 Radian 自有的 `{"servers":[{"id","command","args","env","inherit_env","shared_tools","call_timeout_secs","shutdown_timeout_secs"}]}`，`deny_unknown_fields`（`crates/radian-host/src/mcp.rs`）。配置文件里的 `[mcp]` 节被跳过并列在启动行（`config/mod.rs` 的 `skipped`）；`import-pi` 不迁移 `mcp.json`。
- 子进程环境是白名单：只传 `PATH`、`inherit_env` 里列出的变量和 `env` 里显式写的值（`crates/agent-extensions/src/process.rs`）。
- 主流客户端（Claude Code、Cursor、pi、omp）都用 `{"mcpServers":{"<name>":{command,args,env,…}}}`，键是名字。Claude Code 还支持 `${VAR}` 与 `${VAR:-默认值}` 展开和 `type: stdio|http|sse|ws`。Codex 只在导入时读这个结构。
- 结果：一份现成的 `.mcp.json` 拿过来一个字也用不了，每个 server 都要手工翻译一遍。
- MCP 工具的文本结果**原样进入上下文**，只在压缩时才截到 2000 字符；内建 `read`、`grep`、`shell` 都有字节预算，MCP 是唯一没有上限的工具结果。一个返回 1 MiB 的 server 工具就能触发压缩甚至溢出。

## 决定（已定）

- `--mcp-config` 与将来接上的 `[mcp]` 节都接受标准的 `mcpServers` 对象；现有 `servers` 数组继续兼容。运行时内部只有一种表示（`McpServerConfig`），两种输入在加载时归一。
- `${VAR}` 与 `${VAR:-默认值}` 由宿主在加载时展开，语义照 Claude Code 文档；未设且无默认值的变量给一行警告。
- `type` 为 `http`/`sse`/`ws`、或带 `url` 的条目：报一行"Radian 只支持 stdio，已跳过"，不让整个会话失败。只做 stdio 子集，不写回 `~/.claude.json`，不做 OAuth。
- 项目根的 `.mcp.json`：加进 ADR 0031 T1 的触发清单，受信任后才读；并按内容哈希确认（Codex 对项目 hook 的做法），文件变了就重新问。MCP 是可执行内容，比 skill 更该进门。
- 环境白名单不变：`env` 里显式写出的变量（含 `${VAR}` 展开后的值）照常传，其余宿主环境照旧不传。照 Claude 文档的做法，项目条目里引用 `ANTHROPIC_API_KEY` 这类凭据变量时展开为空。
- `mcpServers` 条目缺省 `inherit_env = ["HOME"]`，写进 ADR 0008；依据是报告的推断（`npx`/`uvx` 启动的 server 通常需要 `HOME`，未验证），落地时用一个 `npx` 起的 server 实测一次，不成立就去掉。显式写了 `inherit_env` 的条目以写的为准。
- 用户级 `mcpServers` 只走 `config.toml` 的 `[mcp]` 节，不另读 `~/.radian/mcp.json`。
- **结果上限**＼�：MCP 工具的文本结果进入上下文前套用与 `read` 缺省字节预算相同的上限（64 KiB），超出部分丢弃，末尾加一行截断标记并写明原始大小；截断在 MCP provider 把 server 响应映射成工具结果的地方做，所以 journal 记录的就是截断后的结果，重放一致；不改 `agent-core`，压缩时的 2000 字符截断不动。上限不加配置项；写进哪个 ADR（0043 的 MCP 一节或本条落地时的 ADR 改动）由落地定。
- **一道门**＼�：项目 `.mcp.json` 的信任确认（按内容哈希）就是对它里面 server 的批准，确认过的文件里的 server 不再按 server 首次询问；`mcp-full-tier-ask.md` 的首次询问只管不经过这道门的来源（命令行 `--mcp-config`、用户级 `[mcp]`）。文件内容变了，整份重新确认。

## 验收

- 用 Claude 文档里的示例 `.mcp.json` 做 fixture：stdio 条目能启动；http 条目被跳过并给出警告；现有 `servers` 数组的 fixture 结果不变。
- 未受信任的项目：`.mcp.json` 不启动任何进程；内容改了之后会重新询问。
- 没设的 `${VAR}` 有警告；凭据变量在项目条目里展开为空。
- 返回 1 MiB 文本的 MCP 工具：进入上下文的结果不超过 64 KiB 并带截断标记；不超上限的结果逐字节不变。
- 变异检查：改成继承全部环境，"没列出的变量不出现在子进程里"的测试必须失败。
- 变异检查：去掉结果上限，"1 MiB 被截断"的测试必须失败。
- 落地时同分支改 ADR 0008、0031（docs-only commit）并删掉本文件。
