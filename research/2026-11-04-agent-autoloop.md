# agent-autoloop 调查（2026-11-04）

起因是 `github.com/KKKKhazix/agent-autoloop` 这个仓库。它把"事件进来、规则派活、Agent 跑一轮、回执里提问、卡片找人、回答后续接会话"做成了一个本机中台。它正好填的是调查 DeepSeek Super-agent 时发现的那个缺口：arc 和 hub 都没有事件入口。本文记录它是什么、怎么做的、和 radian 的对照，以及接 radian 的可行性。判断单列为"判断"，其余为核实过的事实。

## 来源

| 来源 | 性质 | 核实方式 |
|---|---|---|
| 仓库 `KKKKhazix/agent-autoloop`，MIT，TypeScript，Node 24 | 一手 | 浅克隆读完 `README.md`、`docs/protocol.md`、`AGENTS.md`、`SECURITY.md`、`kernel/decide.ts`、`kernel/evolve.ts`、`worker/worker.ts`、`worker/contract.ts`、`plugins/claude/agent.ts`、`plugins/acp/{agent,bridge}.ts`、`kernel/card-page.ts`、`cli/main.ts` |
| GitHub API | 一手 | 仓库建于 2026-11-03，到 11-04 共 63 次提交，2,316 star，467 fork，8 个 open issue；作者账号建于 2025-10，4 个公开仓库，4,600 关注者 |
| README 里的性能与测试数字 | 自报 | 没有复跑 |

整个仓库从提交信息到注释风格都像 Agent 写的；`AGENTS.md` 的约定（测试先于实现、变异检查、零运行时依赖、"协议就是产品"、无界面）和本仓库的 `AGENTS.md` 很像。约 1.8 万行 TS，含 60 个测试文件。

## 它是什么

一个零外部依赖的本机 HTTP 中台，SQLite 存状态，四个口子：事件来源、接单程序（worker）、通道、本人。内核不主动连接任何东西，三样东西都自己连进来。

一件事的流转：

1. **事件进来。** 来源有四种，到内核都是同一种事件：规则里的 `cron`；GitHub / GitLab webhook（`plugins/<adapter>/source.ts` 只验签翻译，不发请求）；`POST /v1/events`；轮询脚本（一个定时跑的 Agent，在回执的 `events` 里交回新事件，中台按 `id` 去重）。
2. **规则派活。** `rules.json` 的每条规则回答五个问题：什么事件（`when`）、交给谁（`to`）、在哪（`cwd`）、照什么做（`skill`、`prompt`）、要不要告诉人（`tell`：`always` / `needed` / `never`）。同一规则同一对象的事叫一个"事项"，排队期间到的事件并进同一轮；`parallel` 按对象拆事项。
3. **Agent 跑一轮就退出。** worker 把事件详情、之前的回答、到期复查、还开着的卡片写成一个资料文件，提示词里只有校验过字符的类型、编号、链接，加一段固定说明。
4. **回执在最后一条消息里。** Agent 不装任何东西、不连中台。第一行写结论（`没跑成：` 自动重派、`没事可做：`、其余算做成），末尾可附一个 ```` ```agent-autoloop ```` JSON 代码块，字段只有 `ask`、`notice`、`resolve`、`later`、`events`。回执写错，接着原会话只让它重写最后一条消息，最多两次，不重做业务。
5. **`ask` 变卡片，等待零消耗。** 卡片推到桌面通知（默认）、飞书（按钮回调）、钉钉 / 企微 / Slack / Discord webhook（靠一个不跑脚本的表单链接回答，7 天有效）。等人期间没有 Agent 进程，状态在数据库。
6. **回答是事件，续接原会话。** 下一轮带着 `answers` 重新派给同一个 Agent，`session` 字段尽量接上：Claude Code 用 `--resume`，Codex 用 resume，ACP 用 `session/load`。接不上就开新会话靠资料文件接着干；接上了干到一半断了算没跑成，留在原会话重派。

另两个动作：`later` 约一个时间再醒来复查（同 key 再写一次就是改时间，`cancel` 取消）；`events` 交回新事件，来源由规则的 `emits` 决定，回执改不了。

## 协议要点

- **规则才是指令，事件只是资料。** 事件里带 `cwd`、`agent`、`access` 直接拒收。`subject` 只能是标识符字符，`link` 只能是纯 ASCII 网址，因为它们会进提示词；`summary`、`detail` 不进提示词。
- **跳数熔断。** 回执交回的事件 `hops` 取本轮事件最大值加 1，经过提问和 `later` 不清零，超过 8 只记录不派发，发一张通知。
- **至少一次。** 响应丢了同一个 `claim` 领回同一份活，同一个 token 重放回执得到同样确认；Agent 可能被叫两次，所以 Skill 里有副作用的动作要先核实现状。
- **失败节奏。** 没跑成 5 分钟后重派，每次翻倍，最长 1 小时；连续第 3 次告诉人，第 10 次不再重试。worker 在线但 5 分钟没报某一轮，标"不确定"并放开这一组；整台 worker 不见了，活留着，10 分钟后告诉人一次。
- **凭据分四种作用域**：来源密钥只能以自己名义发事件；worker 密钥只能领交给它的活；一轮的 token 只能报这一轮；通道密钥只能领卡片、报告、转达 `answerers` 的回答；本人密钥看全部、回答、结案、停止，但不能发事件、领活、改规则（规则只在文件里）。普通密钥只存 SHA-256，轮次 token 和卡片链接由本机 `master.key` 用 HMAC 派生。
- `access` 三档 `read` / `edit` / `full` 由专用 Agent 插件翻译成各自 CLI 的权限（Claude Code：`plan` / `acceptEdits` / `bypassPermissions`）；`command` 类 Agent 只传参数，不替任意程序限制权限。

## 内核与测试

`decide(view, input, now, environment)` 是纯函数，只产生事实；`evolve` 是唯一的投影写入者；判断、追加日志、更新投影在同一个 SQLite WAL + FULL 事务里，提交完才应答。内核不读时钟、不取随机数、不读环境或文件，`tests/purity.test.ts` 守着这条。视图可从日志重建，格式指纹变了就在同一事务里逐条重放，不写迁移脚本。

自报的检查：20 万步固定种子模拟（每 128 步换场景，断网、丢包、崩溃、重复提交），两台 worker、两条事件、一张卡片的全部 8! 种动作顺序穷举，1000 次真进程 SIGKILL，27 处可执行的内核变异（"变异漏过的名字和比例直接输出，不能用语法错误算检出"）。自报性能：冷启动 88.78 ms，空闲 59.94 MB RSS，逐条落盘 2,394 条/秒，派活 p99 1.82 ms；110 万条事实时启动 99.8 ms，状态汇总 927 ms，全量重建 43.1 s。这些都没有复跑。

## 界面

没有。协议和 `AGENTS.md` 把"没有界面"写成原则：总览页、大屏都是"别人用这些口子做的客户端，不进这个仓库"。实际有的：

- 逐行打印的 CLI：`status`、`answer`、`resolve`、`stop`、`why`、`try`、`check`；`try` 和 `answer` 在终端里用 readline 问一句"选几？"。零依赖，没有全屏 TUI。
- 一张卡片网页（`kernel/card-page.ts`）：纯 HTML 表单，CSP 只允许自己的样式，给只会发文字的通道用。
- 通道本身：飞书卡片按钮、四种 webhook 模板、`osascript` / `notify-send` 桌面通知。它把"在飞书里拍板"当主界面。
- 本人客户端的两个读口：`GET /v1/state` 和 `GET /v1/stream?after=<序号>`（SSE 事实流，`id` 是日志序号，断线用 `Last-Event-ID` 续读）。

## 和 radian / arc / hub 的对照

| 它 | radian 一侧 | 差别 |
|---|---|---|
| 事件来源：cron、webhook、HTTP、轮询脚本 | 没有。hub 只收 owner 动作和 session prompt；arc 的任务由人或 lead 加 | 这是缺口本身 |
| `rules.json`：事件 → Agent、cwd、skill、access | arc 的 `[flows]`、`[scopes]` 按任务种类选 worker 参数，但触发只有 backlog 里的任务 | 缺"事件 → 谁来干"的映射 |
| Agent 跑一轮就退，回执在最后一条消息 | `radian exec` 一次性 worker；arc 读 `result.jsonl` 的最后一条助手消息（hub 的 `report` 路由） | 形态相同，只是 arc 不解析消息里的结构化回执 |
| `ask` 结束本轮，等人零进程，回答后 `--resume` | ADR 0024 的 `ask_user` 是阻塞工具调用，答案是工具结果；hub 托管的进程在有 pending question 时不会按 `session_idle_secs` 退出 | 相反的设计：radian 等人时进程挂着 |
| 卡片通道：飞书按钮、webhook、桌面通知 | hub 的 `[hub.hooks] after_pending`、arc 的 `[hooks]`（ntfy / Bark 示例）只出不进；回答走 iOS app 经 hub 的 `answer` 路由 | radian 的"手机上答一下"是自己的 app，它的是飞书 |
| `later` 定时复查 | 没有 | — |
| 事件溯源内核、`why` 一条命令查一轮 | journal（ADR 0001 / 0040）、arc 的 verdict 文件 | 同类做法 |
| 跳数 TTL 8、连续失败 10 次停 | arc 的 `max_attempts`、`max_auto_rework` 与唤醒闸门 | 同类做法 |
| 规则是指令、事件是资料 | ADR 0012 权限来源、ADR 0031 项目信任、ADR 0036 的 AGENTS.md 作为上下文 | 同类做法 |
| 无 TUI、无 app | TUI、hub 会话路由、iOS | 它一个都没有 |

## 接 radian 的可行性

它的 ACP 插件（`plugins/acp/bridge.ts`）对 Agent 的要求，和 `radian acp`（ADR 0049）逐条对：

| 它要求 | `radian acp` |
|---|---|
| `initialize` 返回 `protocolVersion: 1` | 是 |
| 续接时 `agentCapabilities.loadSession` 为真，再 `session/load` | 声明 `loadSession: true`；`session/load` 走 `--resume` 同一套查找，跨进程可用（`crates/radian-exec/src/acp/mod.rs`） |
| 只处理 `session/request_permission`，按 `toolCall.kind` 和 `options[].kind` 选 `allow_once` / `reject_once` | 发 `kind`，选项含 `allow_once`、`allow_always`、`reject_once` |
| `session/prompt` 返回 `stopReason: end_turn` 且有最后一条文字 | 是 |
| 不声明 `fs/*`、终端能力 | radian 用自己的工具，不需要 |

所以 `rules.json` 里写 `"use": "acp", "command": ["radian", "acp"]` 纸面上成立，没有实测。两个坑：

- 它的 `edit` 档只放行 `read`、`search` 和 cwd 内的 `edit`，其余工具种类一律 `reject_once`，shell 也在内。radian 要能干活必须配 `full`，然后靠 radian 自己的 tier 和沙箱兜底。
- 它的提示词是固定中文，含"不要停下来等回答，要问人就把问题写进回执、结束这一轮"。radian 的 `ask_user` 工具仍然注册着，模型会不会照它说的做而不调工具，要试。

## 判断

- 值得拿的是三条设计，不是代码：回执放在最后一条消息里，Agent 零依赖；等人时零进程，回答作为事件重新派活；事件只是资料，规则才是指令。
- 不值得 fork 或照抄：它才两天，单作者，没有 release，`curl | sh` 安装，Node 24 独占（直接 `node --test tests/*.ts`），Windows 待定。hub 已经有 HTTP、token、调度循环、会话读写和 hooks，缺的只是来源、规则、卡片回路、挂起续接这四块，不缺内核。
- "一轮不等人"和 ADR 0024 相反。radian 的 pending question 让 hub 托管的进程一直活着，等三天就挂三天；它的做法是进程退出、状态落盘、回答时 `--resume`。要不要给 hub 一条"问题挂起即退出、回答即续接"的路，是 ADR 0024 和 0049 的事，不能在代码里悄悄改。
- 它的飞书通道和本仓库的 iOS 客户端是同一个场景的两条路。radian 已经选了自己的 app（`docs/backlog/arc-ios.md`），没有理由再做飞书按钮；但它的"文字通道靠表单链接回答"是个便宜的兜底，适合 ntfy / Bark 这类只会发文字的通道。

## 建议

1. 先花一小时做一个 spike：装它，`rules.json` 里把 `radian acp` 配成 `use: "acp"` 的 Agent，跑通 `agent-autoloop try`（提问 → 退出 → 回答 → 续接）。这一下能验证两件事：`radian acp` 的跨进程 `session/load` 在真实客户端下好不好使，以及 radian 的基础提示词能不能配合"回执在最后一条消息"。
2. spike 之后再决定 hub 要不要长出事件入口，待决问题写在 `docs/backlog/hub-event-inlet.md`。
3. 不往本仓库引它的代码。
