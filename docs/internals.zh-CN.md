# 内部资料

这一页写给要构建 radian、或者要改它代码的人。
要使用 radian 或 arc，先看 [README](../README.zh-CN.md) 和 [`docs/user/`](user/index.md) 里的用户手册（手册是英文）。

反引号里的路径都从仓库根目录算起。

## Crate

| Crate | 是什么 | 详细文档 |
|---|---|---|
| `radian-core` | 运行时本身：轮次循环（`Agent`/`AgentSession`/`ActiveRun`）、消息、工具、hooks、steering、日志接缝、事件、上下文预算估算。 | `crates/radian-core/README.md` |
| `agent-extensions` | 把 MCP stdio 服务器包装成 `radian-core` 工具，别名固定为 `server__tool`。 | `crates/agent-extensions/README.md` |
| `agent-provider-contract` | 与适配器无关的契约测试集：每个 `Model` 实现都必须满足的语义，与线上格式无关。 | `crates/agent-provider-contract/README.md` |
| `agent-provider-openai-compatible` | OpenAI 兼容 Chat Completions（`/chat/completions`）的模型适配器，流式。 | `crates/agent-provider-openai-compatible/README.md`、`crates/agent-provider-contract/README.md` |
| `agent-provider-anthropic` | Anthropic Messages API（`/v1/messages`）的模型适配器，流式，API key 认证，extended thinking。 | `crates/agent-provider-anthropic/README.md`、`crates/agent-provider-anthropic/WIRE.md` |
| `radian-session` | 会话 JSONL v1 存储：持久的 `TranscriptJournal` 接缝、读取和重放路径、可丢弃的 SQLite 索引、导出渲染器。 | `crates/radian-session/README.md` |
| `radian-runtime` | 轮次循环之上的一层：持久、有界的收件箱（`followup`/`steer`/`inject`/`cancel`）和每个会话的驱动器。 | `crates/radian-runtime/README.md` |
| `radian-permissions` | 权限层：`ApprovalPolicy` + `ToolRuleSet` 合成一个 `Hooks` 实现，决定 allow / ask / deny。它本身不强制执行任何东西。 | `crates/radian-permissions/README.md` |
| `radian-sandbox` | 操作系统沙箱后端（macOS `sandbox-exec`、Linux `bubblewrap`），按策略构造命令；没有经过验证的后端时**拒绝运行**。 | `crates/radian-sandbox/LIMITS.md` |
| `radian-coding` | 内置编码工具：`read`（文件和目录）、`write`、`patch`（单处或原子的多处编辑）、`glob`、`grep`，在宿主提供工作区时挂载；`shell` 和 `job` 工具还需要沙箱。模型通过 `shell` 运行 git（在不受信任的项目里用 `GIT_CONFIG_*` 加固）。被截断的前台结果登记为普通 job id；保留输出还需要会话私有的输出目录。所有工具都限制在工作区路径内。 | `crates/radian-coding/README.md` |
| `radian-memory` | 记忆存储层：Global/Project 两种作用域的日志和重放、来源规则（agent 不能改用户写的记录）、Forget/Purge、`projects.jsonl` 登记表、Markdown 导入导出视图。不依赖宿主：调用方传入 `~/.radian/memory/`。 | `crates/radian-memory/README.md` |
| `radian-host` | TUI 和无头命令共用的宿主管道，保证两者不会走偏：`--header` 解析、会话 id、`--mcp-config` 文件、模型构造、启动说明、"内置工具 + MCP" 提供者、`~/.radian/` 目录布局和诊断、模型目录，以及记忆层（`MemoryService`、项目身份、agent 记忆工具、注入、共用的 `radian memory` / `/memory` 命令）。 | `crates/radian-host/README.md` |
| `radian-exec` | `radian exec` 背后的库：行提示聊天客户端，可选内置工具、MCP 服务器、会话管理命令和 `radian memory` 命令。 | `crates/radian-exec/README.md` |
| `radian-tui` | `radian` 二进制：基于 `radian-runtime` 的全屏 TUI（只有全屏，没有内联模式），带斜杠命令层（`/help`、`/memory …`、`/access`、`/jobs`）和自己的对话滚动。 | `crates/radian-tui/README.md` |

另有两个二进制不属于产品界面：`agent-extensions` 的 `mcp-fixture`（给它的测试用的手写 MCP 服务器）和 `radian-sandbox` 的 `sandbox-escape`（需要 `--features testing`）。

## 状态

- **`~/.radian/` 目录布局**已实现：`config/` 和 `cache/` 沙箱可读；`sessions/` 和 `memory/` 被拒绝。`RADIAN_HOME` 覆盖根目录。（`--session-dir` / `RADIAN_SESSION_DIR` 仍然只覆盖会话目录。）
- **记忆**已实现：`radian memory` 命令行命令、TUI 的 `/memory` 斜杠命令（同一组命令）、agent 的 `memory_search` 工具（按关键词搜索，或按 id 读取记录；只在超出注入预算时挂载），以及带 `MemoryInjected` 审计事件的确定性注入。
- **配置文件**已实现，只读：`--config`/`RADIAN_CONFIG`/`~/.radian/config/config.toml`，`model.*`、`permissions.profile` 和 `compaction.auto` 作为默认值生效（命令行优先）。它从不包含密钥值。
- **Pi 配置迁移（Phase 5）**已实现：`radian config import-pi --from <pi-agent-dir>` 起草一份 Radian 配置（stdout）和一份映射报告（stderr）。它只读你指定的目录，从不读 `auth.json`，从不读密钥值；没有 `--write` 时什么都不写（`--write` 遇到已有文件会拒绝）。Radian **不**承诺兼容 Pi 的文件格式：见 [`docs/user/pi-migration.md`](user/pi-migration.md)。
- **会话分叉**已实现：`radian exec --fork <parent> [--at <seq>] [--session-id <child>]` 和 TUI 的 `/fork`。分叉点是父会话的轮次边界；子会话在那里物化父会话的有效历史（已提交的压缩生效，未提交的替换丢弃），先于父会话的记录写入，可以恢复，也可以再分叉。
- **自动压缩**已实现：主动预算路径、provider 溢出回退（压缩到一半并重试一次），以及两种模式下的手动 `/compact`。`--no-auto-compact` 只关闭主动路径，溢出回退仍然运行。模型目录和上下文预算预测是它的输入。

## 构建

工作区的最低 Rust 版本是 **Rust 1.99.1**。调试构建：

```sh
cargo build --workspace --locked
```

发布构建：

```sh
cargo build --workspace --locked --release
```

二进制在 `target/debug/`（或 `target/release/`）下，名为 `radian`（别名 `rad`）；`radian exec` 是它的无头命令。agent 在宣布改动完成前必须跑的完整验证命令在 `AGENTS.md` 里。

## 运行

两种模式都**只**从凭据存储（`~/.radian/credentials/auth.json`，由 `radian login` 写入）读 API key，从不从命令行读，也从不打印它。具体见 `radian docs providers`。一个会话是一个 `<session-dir>/<session-id>.jsonl` 文件；会话目录依次取 `--session-dir`、`$RADIAN_SESSION_DIR`、`~/.radian/sessions`（设置了 `RADIAN_HOME` 时是 `$RADIAN_HOME/sessions`）。同一个 `~/.radian/` 根目录下还有 `config/`、`cache/` 和 `memory/`；沙箱拒绝读取 `sessions/` 和 `memory/`。

### `radian exec`（行提示）

```sh
# 先存一个 key：`radian login`（见 `radian docs providers`）。

# One turn, a new session named `demo`, provider and tier named explicitly.
./target/debug/radian exec \
  --provider openai-compatible \
  --model gpt-5 \
  --base-url https://api.openai.com/v1 \
  --profile workspace \
  --workspace "$PWD" \
  --session-id demo \
  --session-dir /tmp/radian-sessions \
  -p "summarize Cargo.toml"
```

`--provider anthropic` 改用 Messages API：base URL 默认是 `https://api.anthropic.com`，并且会发送 `--max-tokens`（默认 16384），因为这个 API 要求这个字段。`--thinking off|adaptive|enabled:<budget>` 控制 extended thinking（默认 `off`）。

恢复刚才保存的会话：`--resume` 取代 `--session-id`（两者冲突）：

```sh
./target/debug/radian exec \
  --provider openai-compatible \
  --model gpt-5 \
  --profile workspace \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --resume demo \
  -p "now read docs/"
```

会话管理命令不需要模型、key 和网络：`--list`、`--export <id>`（`--format json|markdown`、`--out FILE`）和 `--search <text>`。`--export` 和 `--search` 作用于当前工作区的会话（`--workspace`，没有时用当前目录）。

### `radian`（全屏）

只在真正的终端里运行：它会进入备用屏幕和 raw 模式，接管整个屏幕。即使只是 `--list` 也必须给 `--model`。

```sh
# The key comes only from ~/.radian/credentials/auth.json, e.g.
#   {"openai-compatible": {"type": "api_key", "key": "$OPENAI_API_KEY"}}

./target/debug/radian \
  --provider openai-compatible \
  --model gpt-5 \
  --profile workspace \
  --approval on-ask \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --session-id demo
```

恢复任一模式写下的会话（常用参数有短形式；下面的 `-m`、`-w`、`-r` 分别是 `--model`、`--workspace`、`--resume`）：

```sh
./target/debug/radian \
  --provider openai-compatible \
  -m gpt-5 \
  --profile workspace \
  -w "$PWD" \
  --session-dir /tmp/radian-sessions \
  -r demo
```

`--list` 打印这个工作区的会话后退出，不进入终端界面。每轮的模型调用保险丝是 `--max-model-calls`（默认 1000，远高于核心的 32，TUI 和 `radian exec` 都是）。按键、审批框、状态栏和上下文预算的措辞见 `crates/radian-tui/README.md`。

常用参数的完整列表：`-m`/`--model`、`-w`/`--workspace`、`-r`/`--resume`、`-l`/`--list` 和 `-y`/`--yes`（`-V` 是 `--version`）。

## 配置文件

两种模式都读 `~/.radian/config/config.toml`（`$RADIAN_HOME/config/config.toml`），除非 `--config FILE` 或 `RADIAN_CONFIG` 指定了别的路径。文件不存在就是空配置，程序也不会创建它。这个版本里文件是**只读**的：没有 `config migrate`，程序从不回写。

```toml
schema_version = 1

[model]
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
context_window = 200000
# 这里不放 key：key 只放在 ~/.radian/credentials/auth.json；见 `radian docs providers`。

[model.headers]
x-tenant = "team"

[permissions]
profile = "workspace"        # workspace | full

[compaction]
auto = true

[tui]
theme = "dark"               # dark | light | mono; NO_COLOR forces mono
```

- **命令行总是优先**；文件只提供默认值。
- **从不接受密钥值。** `api_key`/`token`/`password`/… 必须是 `SecretRef`（`{ from = "env", name = "…" }`；这个版本只支持 `env`），文件里任何像密钥的字符串都会被拒绝。拒绝信息只写键路径，从不写值。
- `schema_version` 必填，必须是整数。版本**更新**的文件以只读方式读取并给出警告；版本**更旧**的文件被拒绝（没有迁移）。
- 未知的 `permissions` 值被拒绝（失败即关闭）；未知的 `tui.*` 值和任何未知字段被忽略并给出警告。启动时打印 `[config] <path|none>` 和被忽略的键（从不打印值）。
- 这个文件**沙箱可读**，正因为如此它只放引用，从不放 key。

## 权限档位

`--profile` 一次选定三层：沙箱（强制边界）、规则集（allow / ask / deny）和宿主挂载的内容。二进制接受的名字是 `workspace` 和 `full`；旧名 `workspace-write` 和 `full-access` 仍作为别名接受，但不在 `--help` 里列出。**`full` 是默认值**：同类工具默认都不开沙箱，沙箱只差一个参数（`--profile workspace`）。项目可以在 `<workspace>/.radian/config.toml`（`[permissions] profile = "workspace"`）里为自己打开它；这个声明只能**收窄**，不能放宽。

| 档位 | 沙箱 | Shell | MCP | 内置文件工具 |
|---|---|---|---|---|
| `workspace` | 强制：macOS seatbelt 或 Linux bubblewrap，启动时探测 | 提供，在沙箱**内**运行 | 询问 | 只有解析后的路径留在工作区内时才允许 `write`/`patch` |
| `full` | 无，这是默认值 | 提供，**不**在沙箱内，不询问 | 允许 | `write`/`patch` 同 `workspace` |

- **探测失败就拒绝运行。** 后端缺失或探测失败时，宿主报错退出，不会退回到无沙箱运行。没有降级路径。可以用 `RADIAN_SANDBOX_PROBE_FAIL=1` 演练这个路径；它只能导致拒绝。
- **没有 `--workspace`** 时完全没有写或执行工具：内置工具只在有工作区时挂载，`shell` 还需要沙箱。
- **在 `workspace` 档位下**，写入限制在工作区内；`.git` 保持可写，但 git 会执行的条目除外（`config`、`config.worktree`、`hooks/`，以及 submodule 和 worktree 的对应条目）；敏感目录（`~/.ssh`、`~/.aws`、`~/.gnupg`、浏览器配置目录等）默认不可读；网络打开（出站连接，以及本地回环的 connect/bind/listen），pty 设备可用，所以项目自己的测试能跑。这**不是**凭据隔离的承诺：macOS 后端没有关闭 `mach-lookup`，IPC 边界没有收紧。这个版本里 MCP 服务器进程在沙箱外启动，启动提示会说明这一点。
- **`--yes` 不是"全部允许"。** 它把审批者换成一个不显示问题、直接回答"允许一次"的审批者；`deny` 规则照样拒绝，`allow` 规则照样允许，沙箱不变，每个决定照样审计。`radian` 还有 `--approval on-ask|always|never`（默认 `on-ask`）。
- **`full` 信任你的环境。** 它不弹任何内置询问，也不防御来自陌生仓库的提示注入：恶意的 skill、配置或上下文文件会畅通无阻地运行。要边界就用 `workspace`、容器或专用的系统用户。
- **审计**：每个决定都往会话文件写一个 `PermissionResolved` 事件（规则或授权时 `approver` = `policy`，用户回答询问或 `PermissionRequest` hook 作答时 `approver` = `user`）。
- 审批显示工具和**解析后的**资源；`y` 允许一次，`a` 本会话内允许（只在内存里，恢复会话后失效），其他任何输入都是拒绝。`--allow-shell-command PREFIX` 不询问地允许一个*简单*命令前缀；复合命令只有在**每一段**都被允许时（用户规则或内置只读表）才允许，否则仍然询问。前缀要写到子命令一级，不要只写程序名。

常用退出码（统一的一组，加上 `radian exec` 的轮次结果码）：0 成功（这一轮完成），1 这一轮或这次运行失败，2 用法、设置或启动被拒（clap 自己的错误、错误的配置、未知的按键绑定、启动被拒、沙箱探测失败），3 预算用完（`BudgetExceeded`、超过截止时间），4 回答没完成（`Incomplete`、`Refused`），101 程序 panic，143 收到 SIGTERM（128 + 15），130 请求取消或第二次 Ctrl-C 强制退出。`radian exec` 和 TUI 用同一组退出码。`radian exec --list | head`（以及每个打印后退出的模式）遇到断开的管道仍然静默以 0 退出。

## 文档

用户手册在 `docs/user/`；决策在 `docs/adr/`；设计在 `ARCHITECTURE.md`；`research/` 是实验记录（大部分是中文）。

| 内容 | 位置 |
|---|---|
| 设计 | `ARCHITECTURE.md` |
| 决策（改代码前先读） | `docs/adr/` |
| 沙箱后端实际强制了什么 | `crates/radian-sandbox/LIMITS.md`（spike：`spikes/macos-sandbox/README.md`、`spikes/linux-sandbox/README.md`） |
| Provider 契约和它的两个出口 | `crates/agent-provider-contract/README.md`，线上格式笔记在 `crates/agent-provider-anthropic/WIRE.md` |
| 会话格式样例和检查脚本 | `docs/schema/`（`check-fixtures.sh`、`check-config-fixtures.sh`、`fixtures/`、`config/`） |
| 自用记录（用 radian 开发 radian） | `research/2026-10-12-radian-tui-dogfooding.md`、`research/2026-10-12-opencode-zen-dogfooding.md` |
| 性能基线和复现命令 | `benchmarks/perf-baseline.md` |
| Spike 报告（沙箱、TUI、OAuth） | `spikes/` |
| Agent 指令（验证命令） | `AGENTS.md` |
