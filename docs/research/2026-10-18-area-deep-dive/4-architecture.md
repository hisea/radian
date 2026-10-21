# 深挖 4：架构——Radian 与五个对手

2026-10-18 · Radian `hisea/radian@356df1e` · 对手源码：codex `8c07694`、opencode `63c1a27`、pi-mono `482c138`、pi 1.3.0 与 omp 18.12.1 的 npm 包；Claude Code 只用官方文档。行数用 `find + wc -l` 统计，包含空行和注释，只用来比较量级。

## 1. 一句话结论

Radian 的骨架本身就快：单个 Rust 二进制，单进程，不需要 daemon；一轮的 harness 开销 17 ms；产品代码约 9.3 万行，和 pi 的核心包一个量级，只有 Codex 的十分之一左右。真正拖慢"端到端完成任务"的是**权限和沙箱的组合方式**：开了沙箱，shell 仍然每次都要问人，所以 dogfood 只能用 `full + --yes` 跑。连带的后果是：只要不在 auto 模式，`general` subagent 就没法跑 shell。`radian exec` 也没有机器可读的事件流，评测和外部编排都接不上。值得做的是三件小事：沙箱内自动放行、exec 输出 JSON 事件流、一个最小的外部命令 hook。不值得做的有：改成 client/server、上 SQLite、进程内 subagent、继续拆 crate。复杂度的主要代价不在代码行数，而在流程上：43 个 ADR，一次 Verify 要 6–7 分钟。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 语言 / 运行时 | Rust，原生二进制，MSRV 1.88（`AGENTS.md`） | TypeScript，Node ≥ 22.19（pi 1.3.0 `package.json` engines）；可以用 `bun build --compile` 打成单文件（`coding-agent/package.json:35`） | Rust（`codex-rs/`），release 开了 thin LTO，`codegen-units = 4`（`codex-rs/Cargo.toml:634-643`） | 闭源原生二进制（brief 给定；实现细节未核实） | TypeScript，Bun ≥ 1.3.14（omp `package.json`），另带 Rust napi 模块 `pi-natives` | TypeScript + Effect，同时支持 bun、node、workerd 三种运行时（`packages/core/package.json` 的 `#sqlite` imports） |
| 分发体积 | 一个二进制（release 体积未测；`Cargo.toml` 没有 `[profile.release]`） | npm 包解压后 26 MB，另有 22 个运行时依赖 | 一个二进制加一个 bwrap 资源（`linux-sandbox/README.md:16-23`） | 原生二进制（未核实） | npm 包解压后 56 MB；依赖里有 OTel 全家桶和 puppeteer-core | 多包 monorepo，共 35 个包 |
| 进程模型 | 单进程。每个会话一个 Unix 控制 socket（`radian-host/src/control.rs:1-9`），没有 `serve` | 单进程，有 text、json、rpc 三种模式（`coding-agent/src/cli/args.ts:11`） | TUI 和 exec 都通过**进程内 app-server**（JSON-RPC）运行（`exec/src/lib.rs:22-24`，`tui/src/lib.rs:32-39`）；可选 daemon 和远程连接（`app-server-daemon/README.md`） | 单进程 CLI；SDK 会"run the Claude Code binary"（[SDK 概览](https://code.claude.com/docs/en/agent-sdk/overview)） | 单进程（Bun） | **client/server**：缺省连接一个托管的后台 service，`--standalone` 才在本进程起 server（`cli/src/services/server-connection.ts:41-49`） |
| 可嵌入性 | `agent-core` 是库，不做任何 I/O，泛型静态分发（设计文档 §3）；`radian exec -p` 的 stdout 只输出模型文本（ADR 0032 A3） | SDK、RPC 和 json 三种模式 | `codex exec --json`（`exec/src/cli.rs:65-70`）加上 app-server 协议 | Agent SDK（Python/TS）、`-p --output-format json`（[SDK 概览](https://code.claude.com/docs/en/agent-sdk/overview)） | 有 SDK（`src/sdk.ts`） | HTTP API 加生成的 SDK（`packages/sdk`） |
| 扩展机制 | MCP（只支持 stdio，`agent-extensions/Cargo.toml:13`）、skills（ADR 0025）；`Hooks` trait 只能在编译期写 Rust 实现；**没有用户 hook** | 进程内 TS 扩展，用 jiti 加载（`core/extensions/loader.ts:2`），有 35 个事件（`extensions/types.ts:1597-1664`）；另有 MCP 和 packages | 12 个 hook 事件（`hooks/src/lib.rs:25-38`）、插件、MCP、skills | 33 个 hook 事件，5 种 handler，command 类缺省超时 600 s（[hooks](https://code.claude.com/docs/en/hooks)）；插件 | 有 extensibility 目录、MCP、skills，体量很大 | 进程内插件，内建工具本身也写成插件（`core/src/tool/AGENTS.md`）；有 CodeMode |
| 会话存储 | 每个会话一个 JSONL（20 种事件，ADR 0006）+ 可随时重建的 SQLite 索引（`radian-session/Cargo.toml:15-16`）；fork 另起一个文件（ADR 0022） | 树状 JSONL，每个条目带 parent，分支写在同一个文件里（`docs/how-pi-works.md:27-31`） | rollout JSONL + SQLite state DB（`state/Cargo.toml:19-21`，`rollout/src/lib.rs:19`） | JSONL，路径 `~/.claude/projects/<cwd>/`（[sessions](https://code.claude.com/docs/en/agent-sdk/sessions)） | JSONL + `bun:sqlite` 索引（`session/history-storage.ts` 等） | SQLite（drizzle，`core/src/database/`） |
| 权限缺省 | 档位 `full`（`agent-cli/src/profile.rs:79-81`）+ `OnAsk`：**shell 和 `memory_save` 每次都问**（`radian-permissions/src/profile.rs:156-170`） | 没有审批（`docs/security.md:3`） | 审批缺省 `OnRequest`（`protocol/src/protocol.rs:1104-1107`），由模型决定什么时候请求提权；沙箱缺省 `ReadOnly`（`protocol/src/config_types.rs:104-107`） | 有权限模式；沙箱缺省关闭，开启后 `autoAllowBashIfSandboxed` 缺省为 true（[sandboxing](https://code.claude.com/docs/en/sandboxing)） | `approvalMode` 缺省 `yolo`（`tools/settings.ts:311-315`） | allow/ask/deny 规则集（`core/src/permission.ts`），缺省值未核实 |
| 沙箱与提示的关系 | **沙箱不减少提示**：`workspace` 档下 shell 照样 `ask`（`profile.rs:139-150`） | 没有沙箱，推荐放进容器 | 命令在沙箱里直接跑，需要越界时才问 | 开了沙箱就不再问 shell | 未核实 | 未核实 |
| 子 agent | `subagent` 工具会起一个子进程 `radian exec`，深度 1，有 explore 和 general 两种，阻塞执行（`radian-host/src/subagent.rs:1-40`） | 不内建（README:19） | 进程内 thread，最多 6 个，深度 1（`core/src/config/mod.rs:256,266`） | 独立的上下文，可以换模型，最多嵌套 3 层，可以放到后台（[sub-agents](https://code.claude.com/docs/en/sub-agents)） | 进程内，跑在主线程（`task/executor.ts:1-5`），深度可配置 | 进程内子会话，可以后台运行，深度缺省 1（`tool/plugin/subagent.ts:44,129`） |
| 代码规模 | 产品代码约 9.3 万行 Rust（15 个 crate，`src/` 共 12.2 万行，其中约 2.8 万行是内联测试）；测试约 9.6 万行；426 个锁定依赖包 | 核心 4 个包的 src 约 12.5 万行（coding-agent 7.6 万、ai 2.7 万、tui 2.0 万、agent 0.25 万）；整个 monorepo 的 src 有 17.6 万行 | 全部 Rust 208.6 万行，其中 `tests/` 目录 51.8 万；`src/` 里非测试文件约 112 万行；workspace 成员 157 个；1473 个锁定依赖包 | 未知 | 只算 coding-agent 包的 src 就有 50.2 万行 TS | 88.8 万行 TS，其中测试约 28.7 万 |
| 最大的源文件 | `agent-core/src/turn.rs` 2363 行（ADR 0043 §5 拆过之后） | `interactive-mode.ts` 7127 行 | `tui/src/bottom_pane/chat_composer.rs` 12828 行 | — | `session/agent-session.ts` 14110 行 | 未统计 |

## 3. 每家的关键做法（只列和"快、有效率"有关的）

### pi
- 有意保持小：README 写明"skips features like sub-agents and plan mode"（`pi-mono/packages/coding-agent/README.md:19`）；没有审批，安全靠容器或 OS 用户来保证（`docs/security.md:3,13-19`）。每次工具调用的交互成本是零。
- 扩展在进程内运行：jiti 直接加载 TS，`tool_call` 和 `tool_result` 两个事件能拦截或改写调用（`extensions/types.ts:1661-1662`）。没有子进程开销，但扩展和 pi 共用同一套权限。
- 它自己也在长大：新增了 `durable`（2.6 万行，"committed to storage before anything is shown"，`packages/durable/README.md`）、`chord`（1.4 万行）、`codemode`、`mcp`。pi 现在也有 MCP 了（`docs/mcp.md`）。这说明"极简"指的是**缺省行为少**，并不是代码少。
- 会话是一棵树，分支写在同一个文件里（`docs/how-pi-works.md:27-31`）。

### Codex CLI
- 架构最重：TUI 和 exec 都是 app-server 协议的客户端，在进程内走 JSON-RPC（`exec/src/lib.rs:22-24,731`）；同一套协议也服务桌面端、移动端和 SSH 远程（`app-server-daemon/README.md`）。好处是多个前端能共用一套协议；代价是 208 万行代码，以及一层每次都要过的协议转换。
- 沙箱换来更少的提示：缺省 `OnRequest`，"The model decides when to ask"（`protocol/src/protocol.rs:1104-1107`），命令在 Linux 的 bwrap + seccomp（`linux-sandbox/README.md:10-23`）或 macOS 的 seatbelt 里直接运行。另有 `execpolicy` 规则（2975 行）。
- 子 agent 是进程内 thread，最多 6 个，深度 1（`core/src/config/mod.rs:256,266`）。
- 有 12 个 hook 事件，包括 `PermissionRequest`、`SubagentStart/Stop`（`hooks/src/lib.rs:25-38`）。
- release 构建做过调优：thin LTO，`codegen-units = 4`（`Cargo.toml:634-643`）。

### Claude Code（来源：文档）
- 沙箱缺省关闭，只罩 shell，文件工具、MCP 和 hooks 都在沙箱外面。沙箱的意义在于"can run sandboxed commands without asking you"，开启后 `autoAllowBashIfSandboxed` 缺省为 true（[sandboxing](https://code.claude.com/docs/en/sandboxing)）。也就是说，沙箱首先是**减少交互**的手段。
- hooks 共 33 个事件，大多数作为外部进程运行，通过 stdin 传 JSON。`PreToolUse` 可以 allow、deny、ask，也可以用 `updatedInput` 改参数（[hooks](https://code.claude.com/docs/en/hooks)）。插件也能注册进程内的 JS hook（同一页）。
- 子 agent 有独立的上下文，可以按类型换模型（例如让 Explore 用 haiku），最多嵌套 3 层，可以放到后台运行；后台子 agent 的权限请求会转到主会话里问（[sub-agents](https://code.claude.com/docs/en/sub-agents)）。
- SDK 的做法是"a library that runs the Claude Code binary"；其他语言的程序用 `-p --output-format json` 驱动它（[SDK 概览](https://code.claude.com/docs/en/agent-sdk/overview)）。嵌入方式就是"子进程 + JSON 流"。

### omp
- 体量最大：单是 coding-agent 包就有 50.2 万行，`src/` 下 72 个顶层目录；最大的文件 `agent-session.ts` 有 14110 行，`interactive-mode.ts` 有 8740 行。
- 审批缺省 `yolo`（`tools/settings.ts:311-315`）。子 agent 在进程内运行（`task/executor.ts:1-5`），有 workpool 和按 provider 的并发上限（`task/provider-concurrency.ts`）。
- 依赖面很宽（OTel 全家桶、puppeteer-core），npm 包 56 MB。

### opencode
- 缺省是 client/server：TUI 先找或启动一个托管的后台 service，版本不一致就替换它（`cli/src/commands/handlers/default.ts`、`cli/src/services/server-connection.ts:41-49`）。`--standalone` 时在本进程起 server。
- 存储用 SQLite（drizzle）。内建工具也写成插件，和第三方插件走同一条注册路径（`core/src/tool/AGENTS.md`）。
- CodeMode：工具缺省进入一个自己写的 JS 子集解释器，模型写一段程序来组合调用多个工具（`packages/codemode/README.md`）；`read` 和 `shell` 明确不进 CodeMode（`tool/plugin/read.ts:42`，`shell.ts:195`）。
- subagent 支持 `background: true`，深度上限缺省 1（`tool/plugin/subagent.ts:44,129`）。

## 4. Radian 的优势

1. **启动和每轮开销已经很低。** TUI 启动到出现输入框约 98 ms，`radian exec` 启动到提示符 32–55 ms（`benchmarks/perf-baseline.md` §1）。组提交之后，进程内一轮 17 ms，一次工具往返 26 ms（同文 §2）。读 JSONL 每个事件约 1.2 µs（§3）。pi、omp、opencode 都要先启动 Node 或 Bun 的运行时，它们的启动时间这次没有测（未核实），但 Radian 至少在结构上没有运行时预热，也没有 daemon 握手。
2. **只有一个进程，一个文件。** 没有 opencode 那样的托管 service 和版本替换，也没有 Codex 那层 app-server 协议。外部要驱动会话，用每会话一个的控制 socket（`control.rs:1-24`）；要旁观，用 `radian attach` 只读 tail journal（ADR 0043 §8）。herdr 需要的能力都齐了，代价很小。
3. **分层干净，可以嵌入。** `agent-core` 不做任何 I/O，`ToolProvider`、`ToolSession` 和 `Hooks` 都是静态分发（设计文档 §3）。journal seam 加上工具执行标记（ADR 0001/0002）提供了崩溃恢复，而 pi 要靠还在实验阶段的 `pi-durable` 才有这种能力。
4. **规模受控。** 产品代码约 9.3 万行，和 pi 核心包的 12.5 万行 TS 是同一量级，大约是 omp 的 1/5、Codex 的 1/12；锁定依赖 426 个，Codex 有 1473 个。ADR 0043 删掉了约两万行编排代码，拆分之后最大的文件是 2363 行，omp 和 Codex 都有超过一万行的单个文件。
5. **subagent 是子进程。** 每个 subagent 有自己的 journal，取消时分两步收掉（`subagent.rs:24-34`），可以 `attach`。一个子会话出问题，不会弄脏父会话的内存状态。

## 5. Radian 的劣势和缺口

1. **沙箱和审批是两套互不相干的机制，开沙箱不会少一次提示。** `workspace` 和 `full` 两档都把 shell 设成 `ask`（`radian-permissions/src/profile.rs:139-150,156-170`）。所以开沙箱只有成本（约 25 ms 的探针，加上 ADR 0043 背景里那四条撞过的接缝），交互上换不回任何东西，dogfood 自然全用 `full + --yes`。Codex 和 Claude Code 正好相反：沙箱就是用来免掉审批的（见 §3）。"本次会话允许"也只按 (tool, 完整命令串) 精确匹配，而且只在人发起的回合里生效（`radian-permissions/src/hooks.rs:236-239`）：允许过 `cargo test -p a`，`cargo test -p b` 还会再问一次；由 steer 或后台任务通知触发的回合根本用不上这些授权。
2. **不开 auto 时，`general` subagent 实际上跑不了 shell。** 子进程只在父会话是 auto 时才拿到 `--yes`，否则它的 `ask` 一律按拒绝处理（`subagent.rs:15-17,398-400`；ADR 0043 1a.7）。加上缺省 `full + OnAsk`，`general` 碰到 shell 就会被拒。子进程还会拿到父会话的 `--mcp-config`（`subagent.rs:390-393`），推断每次调用都要重新起一遍 MCP server，这部分开销没有测过。`explore` 也不能单独配一个更便宜的模型（gap-analysis #9b 已经提过，这里不展开）。
3. **`radian exec` 没有机器可读的事件流。** `-p` 的 stdout 只有模型文本（ADR 0032 A3），要拿结构化信息只能自己读 journal 或连控制 socket。Codex（`--json`）、pi（`--mode json|rpc`）、Claude Code（`--output-format json`）都有这个能力。没有它，评测框架（gap-analysis 优先级 #1，Harbor/Terminal-Bench）和 herdr 脚本都得各自去解析 journal。
4. **用户层面没有任何 hook。** `Hooks` trait 只能用 Rust 在编译期实现，配置文件里没有 hook 这一节（`docs/schema/config/` 下的 31 个 fixture 都没有）。gap-analysis 里的"编辑后自动 lint"（#3b/#4），在 Claude Code 里是一个 `PostToolUse` command hook 的事，在 Radian 里要改源码、重新编译。
5. **复杂度的代价主要出在流程上，不在代码行数。** 现在有 43 个 ADR，共 7282 行，光 ADR 0006 就有 461 行。测试代码（约 12.4 万行，含内联测试）比产品代码多。Verify 一次 6–7 分钟，冷构建 111 s，其中 `radian-tui` 的 build script 单独占 40.9 s，链接出来的测试二进制有 162 个、共 2.36 GiB（`docs/research/2026-10-15-build-timings.md` §1）。这些成本每次迭代都要付。另外，TUI 和 host 两个 crate 合计约 7.4 万行（src 含内联测试），而 `agent-core` 只有 6.7 千行，复杂度集中在外围。
6. **release 构建没有调优。** 没有 `[profile.release]`（`Cargo.toml:40-44` 只配了 dev），没开 LTO，也没设 `codegen-units`；二进制体积和冷启动可能还有余量（未测）。

## 6. 改进方案（按收益/成本排序）

### ① 沙箱里的 shell 自动放行，越界时再问（收益高，成本中）
- **做什么。** 新增一个 `workspace` 档的规则：沙箱探针成功、并且这次调用确实在沙箱里执行时，shell 由 `ask` 改成 `allow`。命令因为沙箱拒绝（EPERM、只读文件系统、网络不通）而失败时，结果里写明是"沙箱拒绝"，模型可以带理由请求越界执行，这次越界走 `ask`（相当于 Codex 的 `OnRequest`）。`deny` 规则仍然最先生效。"本次会话允许"的匹配从完整命令串放宽到命令前缀，例如 `cargo test`，并复用 README 里"简单命令"的判定。
- **为什么更快。** 现在每条 shell 命令都要等人点一下，这正是 dogfood 退回 `full + --yes` 的原因。Claude Code 的文档把"run sandboxed commands without asking you"作为开沙箱的首要理由。做完之后，`workspace` 档才会真正被用起来，subagent 在 `workspace` 下也能跑 shell（见 ②）。
- **成本。** `radian-permissions` 加一个跟沙箱状态有关的谓词，`radian-sandbox` 补一个失败分类；改动量估计几百行。
- **碰到的 ADR。** 这条改动不违反 ADR 0012 P1 的"approval 不能扩大沙箱"：放行发生在沙箱之内，越界仍然要问人。但它改的是 0012 定下的档位规则（`workspace` 档里 shell 的动作），所以需要 **0012 修订或一个新 ADR**；还涉及 0043 §2（缺省档位不变）和 0016。
- **验证。** 在 `workspace` 下跑 `cargo test`，不出现提示；写 `~/x` 被沙箱拒绝，结果里有"沙箱拒绝"的标记，再次请求时走 `ask`。变异测试：探针失败时仍然放行，测试应该失败。A/B：同一组任务在 `workspace` 档下统计提示次数和墙钟时间。

### ② subagent 继承父会话的放行能力（收益中高，成本低；依赖 ①）
- **做什么。** 父会话在 `workspace` 档并且沙箱有效时，子进程按 ① 的规则自动放行沙箱内的 shell；仍然需要问人的调用，不再一律拒绝，而是经控制 socket 转回父会话的审批人（Claude Code 的后台子 agent 就是这么做的，见 §3）。这一步可以留到第二步做。同时测一下每次 subagent 调用的启动时间，以及重起 MCP server 的开销；如果偏大，就让 `explore` 不挂 MCP。
- **为什么更有效。** 让 `general` subagent 在非 auto 模式下也能真正干活，不再白白浪费一次模型调用的 token。
- **ADR。** 0043 1a.7 需要修订。**验证：** 在 `workspace` 档、非 auto 模式下，`general` 子会话里的 `cargo check` 能成功。

### ③ `radian exec --output jsonl`（收益高，成本低）
- **做什么。** 加一个新的输出模式，把已经写进 journal 的事件和流式 delta 按行输出成 JSON（复用 ADR 0010 的 live/durable 划分）；最后一行是 result 记录，带 usage、费用、退出原因和 session id。缺省输出不变。
- **为什么。** 评测（gap-analysis #1）和 herdr 都需要它，三家对手也都有。拿到之后，A/B 统计不必再去离线解析 journal。
- **ADR。** ADR 0032 A3 写了"若将来要统一 JSON 输出，另立决定"，所以要**写一个小 ADR**。**验证：** 写一个 golden 测试，用 schema 校验每一行；`check-fixtures.sh` 覆盖新格式。

### ④ 一个最小的外部 command hook：`after_edit` 和 `on_turn_end`（收益中，成本中）
- **做什么。** 在配置里加 `[[hooks]] event = "after_edit" | "turn_end"`、`command`、`timeout_secs`。stdin 传 JSON；退出码非 0 时，把 stdout 的前 N 行附加到工具结果里。它通过 `after_tool_call` 和 `on_turn_end` 接到现有的 `PermissionHooks` 组合上（设计文档 §4 的 `(A, B)` 组合），**不改 `agent-core`**。命令放在当前档位的沙箱里执行。项目级的 hook 要过信任门（ADR 0031 T1 的清单加一项）。
- **为什么。** 编辑之后马上拿到诊断，能减少无效回合（gap-analysis #3b 引用的 SWE-agent 证据）。做成 hook 之后，项目可以自己配 `cargo check` 或 `tsc`，不用把具体逻辑写进 Radian。只开两个事件，不照搬 Claude Code 的 33 个。
- **ADR。** 新 ADR；涉及 0031 和 0012（hook 进程是否进沙箱）。**验证：** 一个用 fixture 脚本做的集成测试；变异测试：hook 失败时不附加输出，测试应该失败。

### ⑤ 降低迭代成本：Verify 和构建（收益中，成本低）
- 采纳 `docs/research/2026-10-15-build-timings.md` 里的选项：合并集成测试二进制（144 个链接产物是最大的一项）；`radian-tui` 的 build script 用了 40.9 s，查一下它在做什么，看能不能缓存；设 `[profile.release]`，用 thin LTO 加 `codegen-units`，并量一下体积和启动时间。这些都不碰 ADR，只改构建配置。
- **为什么。** 6–7 分钟的 Verify 乘以每天的迭代次数，是"迭代速度"里最大的一项可控成本。

### ⑥ 不值得做的
- **改成 client/server 或 daemon（opencode、Codex app-server 那样）。** 对单会话、单机的 Radian 没有速度收益，反而多出握手、版本替换和协议层。控制 socket 加 `attach` 已经够 herdr 用了。只有将来做 `radian orca` 或跨机器时才需要（ADR 0043 远期）。
- **主存储换成 SQLite。** 读 JSONL 每个事件 1.2 µs，一轮只有 2–3 次 fsync，而且索引已经是可重建的 SQLite。换掉会动 ADR 0006、0001、0040 和整套 `journal-selfcheck`，换不来什么。
- **pi 那样同文件的树状分支。** 已经有 fork（ADR 0022）。这项对完成任务的速度没有影响。
- **把 subagent 改成进程内运行。** 子进程启动约 32–55 ms，相对于一次模型调用（秒级）可以忽略；进程隔离和独立 journal 更值钱。真正要测的是 MCP 重起（见 ②）。
- **CodeMode（opencode、pi）。** 实测 93% 的模型回复只调用一个工具（`docs/research/2026-10-15-tool-concurrency-measurement.md`），程序化组合多个工具的空间很小，可以以后再看。
- **再拆或再合并 crate。** 15 个 crate 的边界清楚，大文件也已经拆过。继续拆的收益是负的（会增加测试二进制和链接时间，见 ⑤）。

### 关于"复杂度是否已经超出 pi 式极简"的判断
- 按**代码量**看，没有超出。Radian 的产品代码（9.3 万行 Rust）和 pi 核心（12.5 万行 TS）同一量级，pi 自己也在往 durable、MCP、codemode 方向长。Radian 离 omp（50 万行）和 Codex（100 万行以上）的复杂度还很远。
- 按**缺省行为**看，超出了。pi 缺省不问、不沙箱、不分档位；Radian 缺省是 `full`，但仍然要问 shell，还有三个档位、信任门、"只降不升"的来源规则和凭据脱敏。安全上这些大多值得保留，但它们的**交互成本**应该靠 ① 降下来，而不是继续删机制。
- 按**流程**看，超出了，而且这是最值得处理的一块。43 个 ADR 加上一次 6–7 分钟的 Verify，让每一个小改动都很重。AGENTS.md 已经规定 ADR 到第三次修订就要重写；建议再加一条"按主题合并已经搁置或被取代的 ADR"，并按 ⑤ 把 Verify 压到 3 分钟以内。这些是流程上的改动，不需要动代码。

## 7. 2026-10-20 状态Ｐ后补记，代码 `5545b15b`）

报告的四个核心判断（骨架快、慢在权限与沙箱的组合、exec 没有机器可读输出、没有用户 hook）仍然成立。以下事实已变：

- **ADR 规模**：pi/compact-adrs 合并后是 41 个文件、3099 行，0006 是 242 行；0030 和 0041 已并掉。§5.5 "流程复杂度主要在 ADR"只剩 Verify 这一半。
- **`memory_save` 每次都问**：模型已没有记忆写工具，只剩 `memory_search` 且永远 allow。
- **`explore` 子进程带父的 `--mcp-config`**：已修，只有 `general` 还带，重起 MCP 的开销仍未测。
- **`radian-tui` build script 40.9 s**：已改成校验提交的 `syntaxes.packdump`，这项没有了。
- **`[profile.release]`**：仍然没有；决定先不做。

方案的修正：

- ① 的前提是先用 seccomp 堵上 Linux bwrap 的 Unix socket（`crates/radian-sandbox/LIMITS.md`，决定 4），前缀授权受 shell 匹配器看不见复合命令的限制；成本中高。它同时解决无头 `radian exec` 不带 `--yes` 跑不了 shell 的问题。
- ② 不只是修订 0043 1a.7：缺 durable 的 `PermissionRequested`，`control` 还拒绝审批答复；成本中高，依赖 ①。
- ④ 由 `docs/backlog/command-hooks-claude-subset.md` 的项目级 `PostToolUse` 替代（编辑后检查：配了才跑、每次同步，不内置）。
- ⑤ 的测试二进制合并要单独立 ADR（`set_var` 与全局状态已实测），release 调优先不做。

对应的 backlog：`sandbox-auto-allow-shell.md`、`subagent-ask-forwarding.md`、`subagent-explore-model.md`、`exec-jsonl-output.md`、`command-hooks-claude-subset.md`、`verify-test-binaries.md`。
