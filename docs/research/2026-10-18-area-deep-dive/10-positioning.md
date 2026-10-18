# 深挖 10：产品定位与差异化

2026-10-18 · Radian `hisea/radian@356df1e` · 对手：pi（`earendil-works/pi`，本机 pi-mono `482c138` 与 npm 1.3.0）、Codex CLI（`openai/codex` HEAD `8c07694`）、Claude Code（闭源，只用官方文档）、omp（`can1357/oh-my-pi`，npm 18.12.1）、opencode（`anomalyco/opencode` `63c1a27`）。
网页数据的抓取日期都是 **2026-10-18**。npm 周下载量来自 `api.npmjs.org/downloads/point/last-week/<包>`，各包返回的统计区间不一样（接口有缓存），区间写在单元格里。GitHub 星数来自仓库首页。HN 数据来自 `hn.algolia.com` 的搜索接口。
本区域按第三轮 brief 的要求，不以速度为唯一目标。"收益"包括用户量、可信度、能用的场景和长期可维护性。

## 1. 一句话结论

五家里每家都有一个清楚的"一句话"：Claude Code 是"Anthropic 官方、什么界面都有、订阅制"；Codex 是"OpenAI 官方、ChatGPT 订阅就能用、开源 Rust"；opencode 是"开源、什么模型都能接、自带 $10 的模型套餐"；pi 是"极简、可以改成你自己的"；omp 是"pi 的全家桶版、把 IDE 接进来"。**Radian 现在还没有这样一句话**。它是一个私有仓库，没有发布过，星数为 0（2026-10-12 建库）。不过它有几样五家都没有凑齐的底子：一个可嵌入的 Rust 运行时、每次写入都可以回放校验的 journal、"后端没证实就拒绝运行"的沙箱、有契约测试的多 provider 层、可以机械验证的"默认不出网"，以及 herdr 一等公民的集成。
**推荐的定位**：短期讲"**在 herdr 和无人值守场景里可以放心交出去的 worker agent**"（方向 B）。把"可审计、可嵌入的 Rust 内核"（方向 A）当作证据和长期护城河，先不单独推广。不要和 pi 在"极简、可改造"这条线上正面打（方向 C）：pi 的生态和发布节奏都远远领先，Radian 赢不了。

## 2. 对比表

### 2.1 定位、目标用户、商业模式

| 维度 | Radian | pi | Codex CLI | Claude Code（文档） | omp | opencode |
|---|---|---|---|---|---|---|
| 自我定位（原话） | "minimal, headless, embeddable Rust agent runtime, plus the coding agent built on top"（`README.md:3`） | "a minimal, extensible agent harness that you can make your own"（`pi-mono/README.md`） | "Lightweight coding agent that runs in your terminal"（[GitHub](https://github.com/openai/codex)） | "agentic coding tool … Available in your terminal, IDE, desktop app, and browser"（[overview](https://code.claude.com/docs/en/overview)） | "Coding agent with the IDE wired in. Built by Stencil Labs"（[GitHub](https://github.com/can1357/oh-my-pi)） | "The open source coding agent."（[GitHub](https://github.com/anomalyco/opencode)） |
| 目标用户 | 目前只有 owner 自己 dogfood；ADR 0043 的目标是"先能快速高效地取代 pi"（0043:12） | 想自己掌控 harness 的开发者、本地模型用户（[implicator](https://www.implicator.ai/pi-is-not-a-claude-code-rival-it-is-a-harness-rebellion/)） | ChatGPT 付费用户、OpenAI 生态里的团队 | Claude 订阅用户和企业；覆盖所有界面 | 想要开箱即用的 pi 用户，重 LSP 和调试器的人 | 想要模型中立的开源用户 |
| 开源与许可 | Apache-2.0（GitHub 元数据），**私有**，未发布 | MIT（核心）；托管功能保留 Fair Source 或专有许可（RFC 0015，据 [implicator](https://www.implicator.ai/pi-is-not-a-claude-code-rival-it-is-a-harness-rebellion/)） | Apache-2.0（`codex/README.md:81`） | 闭源二进制；Agent SDK 受 Commercial Terms 约束（[SDK](https://code.claude.com/docs/en/agent-sdk/overview)） | MIT，Stencil Labs, Inc.（`omp/package.json:17-20`） | MIT |
| 收入来源 | 无 | Earendil（公益公司）的 Radius 网关和 Lefos 云平台（[eesel](https://www.eesel.ai/blog/pi-coding-agent)） | ChatGPT 订阅 | Claude 订阅和 API | 未核实（Stencil 是公司） | OpenCode Go，每月 $10 的模型套餐（[LDS](https://letsdatascience.com/news/opencode-launches-10-monthly-open-source-agent-f29289b1)）；另有 Zen（`opencode.ai/zen` 端点见 ADR 0013 修订记录） |
| 订阅或模型绑定 | 不绑定；支持 API key 和 ChatGPT 订阅 OAuth（`radian-host/src/oauth_chatgpt.rs:1`）；Anthropic 只支持 API key（`README.md` crate 表） | 不绑定；订阅登录最多（HN 评论 "Pi has the most possible subscriptions of all"，[HN 49926069](https://news.ycombinator.com/item?id=49926069)） | 偏向 OpenAI：推荐用 ChatGPT 登录，也能用 API key（`codex/README.md:68-72`） | 绑定 Claude；CLI 也支持 Bedrock、Vertex 这类第三方 provider（[overview](https://code.claude.com/docs/en/overview)） | 不绑定，60 多个 provider（README） | 不绑定；2026-03 起因法律要求删掉了 Anthropic OAuth（[PR 18186](https://github.com/anomalyco/opencode/pull/18186)） |
| 嵌入方式 | Rust crate `agent-core`，不依赖 CLI 或 UI（设计文档 §3）；**未发布到 crates.io** | TS SDK、RPC、print/JSON；另有 Pi Durable（TS，实验性）（[pi-durable](https://earendil.com/posts/pi-durable/)） | TS 和 Python SDK（`codex/sdk/`）、app-server 协议 | Agent SDK（Py/TS），内部"runs the Claude Code binary"；禁止第三方提供 claude.ai 登录 | Node SDK、RPC、ACP | client/server 加 SDK（`packages/sdk`、`server`） |
| 安装方式 | 只能从源码 `cargo build`（`README.md` Build 一节） | curl 安装脚本、npm | curl、npm、brew | 原生安装器，后台自动更新；也有 brew、winget、apt | bun 或 npm | curl、npm、brew、scoop、choco |

### 2.2 社区规模与增长（抓取于 2026-10-18）

| 指标 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| GitHub stars | 0（私有） | 116k，fork 14.8k | 128.1k，fork 20.1k | 151.8k，fork 26.9k（这个仓库只放 issue 和插件，代码闭源） | 34.1k，fork 3.7k | **214.6k**，fork 28.8k |
| 星数增长 | — | 45,041（2026-05-06，[implicator](https://www.implicator.ai/pi-is-not-a-claude-code-rival-it-is-a-harness-rebellion/)）→ 112,973（10-07，[eesel](https://www.eesel.ai/blog/pi-coding-agent)）→ 116k（10-18），5 个月涨了 2.5 倍 | 80,174（2026-05-06，implicator）→ 128.1k | 约 124k（2026-06，[towardsai](https://pub.towardsai.net/opencode-just-dethroned-claude-code-with-172-000-stars-i-ran-both-and-its-78-slower-508961c12959)）→ 151.8k | 未核实 | 172,198（2026-06，towardsai）→ 214.6k |
| npm 周下载 | — | `@earendil-works/pi-coding-agent` 4,145,028（10-06～12）；旧包 `@mariozechner/…` 318,490（10-10～16）。其中包括 OpenClaw 等嵌入使用（eesel 也这么说），**不等于用户数** | `@openai/codex` **15,038,867**（10-10～16） | `@anthropic-ai/claude-code` 11,913,420（10-06～12）；原生安装器的量不在里面 | `@oh-my-pi/pi-coding-agent` 83,575（接口返回的是 09-14～20） | `opencode-ai` 2,860,887（10-06～12）加新包 `@opencode/cli` 207,717（10-10～16）；累计下载在 2026-01-29 是 1019 万（`opencode/STATS.md` 末行） |
| 发布节奏（近 30 天） | 无发布 | 15 个版本，从 0.86.0 到 1.3.0（`coding-agent/CHANGELOG.md`，09-19～10-15；1.0.0 在 10-01） | 稳定版 0.160.1（10-05）、0.161.0（10-07），alpha 几乎每天一个（[releases](https://github.com/openai/codex/releases)） | 本机 npm 包 2.1.302；节奏未核实 | **50 个版本**（`omp/CHANGELOG.md`，09-18～10-17） | 10-08 到 10-16 之间 6 个版本，到 v2.0.32（`opencode/CHANGELOG.md`，文件只记到 v2.0.26） |
| HN 最高热度 | — | "Pi 1.0"：1689 分，610 条评论（2026-10-01，[49926069](https://news.ycombinator.com/item?id=49926069)） | 发布帖 516 分（2025-04）；"装了常驻自更新 daemon" 609 分（2026-10-12，[50060694](https://news.ycombinator.com/item?id=50060694)） | — | 42 分（2026-07，[48994611](https://news.ycombinator.com/item?id=48994611)） | 1274 分（2026-03-20，[47460525](https://news.ycombinator.com/item?id=47460525)）；"OpenCode 2.0" 886 分（10-14，[50094994](https://news.ycombinator.com/item?id=50094994)） |

**怎么读这些数**：星数和下载量反映的是关注度，不是生产使用量（implicator 原话）。npm 下载包含 CI 和嵌入使用。Claude Code 和 Codex 有大量走原生安装器的用户，npm 统计不到，实际量只会更大。

### 2.3 用户公开评价

| | 喜欢 | 抱怨 |
|---|---|---|
| pi | 小、透明，没有隐藏上下文；系统提示词不到 1k token，"$20 plan lasts for hours"（[HN 48883275](https://news.ycombinator.com/item?id=48883275)）；对本地模型友好；扩展好写（[HN 49926069](https://news.ycombinator.com/item?id=49926069)） | 上手难，"I don't know what I want to start with"；"一切都是插件"带来配置负担；默认没有权限提示，有人反馈文件被误删；npm 供应链风险（[eesel](https://www.eesel.ai/blog/pi-coding-agent)）；加入 MCP 被批评偏离了极简（HN 49926069） |
| Codex | 额度宽，"I coded nonstop and never got blocked"；第一次就做对的可靠性高；适合 DevOps 和脚本类工作（[Vaughan 2026-03-30](https://codex.danielvaughan.com/2026/03/30/reddit-codex-cli-sentiment-march-2026/)） | 用量计费不透明（[issue 13186](https://github.com/openai/codex/issues/13186)）；默认装常驻自更新 daemon，旧版本占了 12 GB 磁盘（[HN 50060694](https://news.ycombinator.com/item?id=50060694)）；有人觉得它"buggy and bloated"，转去用 pi |
| Claude Code | 多文件重构和大仓库表现好；MCP 生态（[morph](https://www.morphllm.com/claude-code-reddit)，Morph 是竞品厂商，数据未核实） | 用量上限是头号抱怨（[The Register 2026-01-05](https://www.theregister.com/2026/01/05/claude_devs_usage_limits/)）；启动开销大，"sends 33k tokens before reading the prompt"（[HN 48883275](https://news.ycombinator.com/item?id=48883275)）；长会话多次压缩后质量下降；**锁定**：2026-01-09 起第三方 harness 不能用订阅（[HN 46549823](https://news.ycombinator.com/item?id=46549823)，[时间线](https://falcao.org/posts/anthropic-claude-access-crackdown-ecosystem-fallout/)） |
| omp | LSP 和调试器集成是真功夫；"polished product"；hashline 编辑（作者自测数据）（[whyops](https://whyops.com/blog/oh-my-pi-hype-hashline)） | 开局约 22k token；"burns a lot more tokens than Pi"（HN 49926069）；配置深、文档少；升级容易出问题，"lab project"的感觉 |
| opencode | 不锁 provider，开源免费；2.0 的 client/server 和插件系统（[HN 50094994](https://news.ycombinator.com/item?id=50094994)） | **安全问题多**：未认证 RCE，CVE-2026-22812（[HN 46581095](https://news.ycombinator.com/item?id=46581095)）；"Annoying and alarming things"：文本 bash 过滤能被绕过、默认远端模型、`curl | bash` 升级、TUI 占约 1 GB 内存（[wren.wtf](https://wren.wtf/shower-thoughts/stop-using-opencode/)）；2.0 的插件工具绕过权限，修 bug 慢；2026-10-17 "User credentials leaked"（[HN 50133401](https://news.ycombinator.com/item?id=50133401)，事件细节未核实） |

## 3. 每家的关键做法（只写和定位有关的）

### Claude Code（来源：文档）
- **一个引擎，所有界面**：终端、VS Code、JetBrains、桌面、Web、移动端、Slack、GitHub Actions、Routines 共用一套 CLAUDE.md、设置和 MCP（[overview](https://code.claude.com/docs/en/overview)）。它卖的是"在哪儿都能接着干"，终端只是其中一个入口。
- **嵌入也是 Claude Code 本身**：Agent SDK "A library that runs the Claude Code binary"；不允许第三方提供 claude.ai 登录或订阅额度；品牌上不能叫 "Claude Code"（[SDK](https://code.claude.com/docs/en/agent-sdk/overview)）。嵌入 = 绑定 Claude、按 API 计费。
- **用订阅锁住用户**：2026-01-09 起服务端拦截第三方使用订阅 OAuth；02-17～20 写进条款；04-04 正式规定 Pro/Max 不覆盖第三方工具；05-13 推出"Agent SDK credits"（[falcao 时间线](https://falcao.org/posts/anthropic-claude-access-crackdown-ecosystem-fallout/)）。

### Codex
- **开源，但用订阅引流**：README 第一个推荐是 "Sign in with ChatGPT"（`codex/README.md:68-70`）。OpenAI 和 opencode、OpenHands、RooCode 合作，允许它们使用 Codex 订阅（[falcao](https://falcao.org/posts/anthropic-claude-access-crackdown-ecosystem-fallout/)）。这正是 Radian 已经接上的 ChatGPT OAuth 路线。
- **平台化**：app-server 协议同时服务 TUI、exec、桌面和远程（`codex-rs/app-server*`）；有 TS 和 Python SDK（`codex/sdk/`）。代价是 208 万行 Rust（4-architecture.md 表格），和社区抱怨的常驻 daemon 与磁盘膨胀。

### pi
- **"Harness 归你"是一个价值主张，不只是一个功能清单**：Zechner 和 Ronacher 的论述是，厂商的 harness 会注入隐藏上下文、随版本改变行为（[implicator](https://www.implicator.ai/pi-is-not-a-claude-code-rival-it-is-a-harness-rebellion/)）。这一点同时是 Radian 的灵感来源和主要对手。
- **开放核心加公司**：2026-04-08 被 Earendil 收购；核心保持 MIT，托管功能用 Fair Source 或专有许可。增长靠包生态：2026-05 时 2,143 个包，2026-10-18 时 5,337 个（8-extensibility.md）。
- **正在往 Radian 的方向走**：Pi Durable（2026-10-01，TS，实验性）提供检查点、崩溃后从检查点继续、`replay: "safe"` 的工具重放、`requestId` 保证提交恰好一次（[pi-durable](https://earendil.com/posts/pi-durable/)）。FAQ 里说"not ruled out"以后移植到 Rust。**Radian 的"可恢复、可回放"差异化正在被 pi 追上**。

### omp
- **"Batteries included" 的 pi 分叉**：32 个工具、LSP、DAP、Python 内核；读取 `.claude`、`.cursor` 等其他工具的配置，白拿别人的生态（[GitHub](https://github.com/can1357/oh-my-pi)）。发布节奏极快（30 天 50 个版本），但口碑集中在"token 开销大、不稳定"上，HN 热度最高只有 42 分。说明"功能多"不一定带来用户量。

### opencode
- **星数第一，靠模型中立加社区**：214.6k 星，22 种语言的 README（`opencode/README*.md`）；商业化靠 OpenCode Go 的 $10 模型套餐。
- **可信度是它最大的软肋**：一年里出了 RCE、凭据泄露、权限被绕过，还有"alarming"长文（见 2.3）。**这正是 Radian 能讲"可信"故事的市场空档**：用户需要不锁 provider，但又不想冒 opencode 这种安全风险。

## 4. Radian 的优势（独特底子，都已在代码里）

1. **单个 Rust 二进制，资源占用小**：TUI 启动约 98 ms，空会话 RSS 11.3 MB（`docs/perf-baseline.md:44,163`）；一个二进制同时提供 TUI 和 `radian exec`（ADR 0032 A1）。对比 opencode TUI 约 1 GB（[wren.wtf](https://wren.wtf/shower-thoughts/stop-using-opencode/)），以及 Codex 每份二进制约 300 MB、旧版本累计 12 GB（HN 50060694）。
2. **可嵌入的运行时，不是 CLI 外面包一层**：`agent-core` 不依赖 CLI、UI、HTTP 或数据库；Model、ToolSession、Hooks 都是注入的，静态分发（设计文档 §3）。Claude Agent SDK 实际上是在起 Claude Code 二进制；Codex 的 SDK 走 app-server 协议。**一个能直接链进 Rust 进程的 agent 内核**，五家里只有 Radian 有（Codex 的 crate 理论上也能用，但它们不是为嵌入设计的，推断）。
3. **journal 可以回放校验**：每次 transcript 写入都进 JSONL journal（ADR 0001、0006）；`journal-selfcheck` 特性（`agent-core/Cargo.toml:27`）在测试里把每个会话的 journal 和 transcript 对照回放；`docs/schema/check-fixtures.sh` 校验格式。工具执行标记区分"结果未知"和"没开始"（ADR 0002）。在此基础上已经有 resume、fork（ADR 0022）和只读的 `radian attach --follow`（ADR 0043 §8）。pi 的 Durable 还是实验性的。
4. **沙箱宁可拒绝也不降级**：后端必须先跑探针证明有效，否则直接报错，没有任何路径会悄悄退回无沙箱（`docs/sandbox.md` "硬规则"）；有禁读清单（会话、记忆、凭据），按项目保存，项目配置只能收窄（ADR 0043 §2）。对比 opencode 那种能被绕过的文本过滤，以及 pi 的默认无权限。
5. **"默认不出网"可以机械验证**：唯一的出网白名单是 provider 请求（ADR 0013 T1，`0013:13-15`），而且 T5 要求用测试断言，而不是写一句承诺（`0013:48-58`）。对比：Codex 和 Claude Code 默认后台自动更新（HN 50060694、[overview](https://code.claude.com/docs/en/overview)），opencode 默认连远端（wren.wtf）。
6. **多 provider 有契约测试**：Chat Completions、Anthropic Messages、Responses 三个适配器（`crates/agent-provider-*`），都要通过同一套 `agent-provider-contract`；规则写明"Never invent a wire value"（AGENTS.md）。另外有 ChatGPT 订阅 OAuth，这条路 OpenAI 是允许的。
7. **herdr 一等公民**：上报状态、元数据、`state_labels`、会话身份；`HERDR_*` 环境变量在所有档位放行；仓库里带 herdr 插件（`integrations/herdr/herdr-plugin.toml`，有 start、start-worktree 和 sessions、attach 两个 pane）（ADR 0034）。herdr 本身是 Rust 写的 agent 多路复用器，2026-07 时超过 1 万星（[tecmint](https://tecmint.com/herdr-run-ai-coding-agents-in-linux-terminal)）。
8. **可维护性**：每个约束代码的决定都有 ADR，有 MSRV 检查，测试要求做变异验证（AGENTS.md）。产品代码约 9.3 万行，是 Codex 的十分之一左右（4-architecture.md）。
9. **从 pi 迁移有现成路径**：`radian config import-pi`（`README.md:50`）、pi 兼容的主题文件（ADR 0029）。

## 5. Radian 的劣势和缺口（就定位而言）

1. **没有发布，没有分发，没有一句话**：私有仓库；只能从源码构建；没有 release 二进制、brew 或 curl 安装器；crate 没上 crates.io（6 个 crate 写了 `publish = false`，包括 `radian-host`；`agent-core` 还是 `0.1.0`）。"可嵌入"现在只是设计目标，外部用户用不到。
2. **差异化在架构层，用户看不到**：journal、契约测试、fail closed 这些对用户的价值要靠故事讲出来。现在没有任何对外的证据，没有评测（分报告 6），也没有和 pi 的配对对比。
3. **默认 `full`，削弱了"安全"故事**：ADR 0043 §2 把默认改成不沙箱，理由是 dogfood 现实（开了沙箱 shell 仍然每次都问，见分报告 7）。所以"安全"只能讲成"需要时一个参数就打开"，而不是"默认安全"。
4. **生态是零**：没有用户 hook，没有插件，项目级 skill 还没接上（分报告 8）。pi 有 5,337 个包，Claude Code 有官方插件市场。
5. **单人项目**：ADR 节奏很快（43 个，0043 刚把 0030 整个搁置），说明方向还在变。外部用户和嵌入方需要的是稳定的 API 和文件格式承诺。
6. **界面只有终端**：没有 IDE 插件、桌面版或 Web 版；Windows 没有沙箱后端（`docs/sandbox.md` 只有 macOS 和 Linux），能不能在 Windows 上构建未核实。
7. **Anthropic 订阅用不了**：这是 Anthropic 条款决定的（[SDK 文档的 Note](https://code.claude.com/docs/en/agent-sdk/overview)），所有第三方 harness 都一样，不是 Radian 自己的缺陷，但"用 Claude 订阅的人"这块用户永远拿不到。

## 6. 可选的定位方向（按"对整体价值的收益/成本"排序）

### 方向 B（推荐）：在 herdr 和无人值守场景里，可以放心交出去的 worker agent

- **目标用户**：同时跑多个 agent 的重度终端用户（herdr、tmux）；在 CI、cron、脚本里跑 agent 的团队；需要事后审计"agent 到底做了什么"的人。
- **要讲的故事**："起一堆 agent 去干活，你不在场也放心。"一个 11 MB 的进程，100 ms 启动；herdr 侧栏直接显示它在等审批、在问问题，还是在等信任确认；每一步都写进可回放的 journal，可以 `attach` 旁观，崩了可以 `--resume`，也可以从任意轮 fork；陌生仓库和无人值守时一个参数打开真沙箱，后端没验证过就拒绝运行；除了你配置的模型端点，不连任何网络，这一点有测试证明。模型方面，可以用 ChatGPT 订阅，也可以用任意 OpenAI 兼容端点或 Anthropic。
- **已有支撑**：ADR 0034 的 herdr 集成和插件；`radian attach`；resume 和 fork（ADR 0022）；`radian exec`；一次性 `subagent`（ADR 0043 §1a）；沙箱和禁读清单；ADR 0013；三种 provider 协议。
- **还缺什么**（按收益/成本排序）：
  1. **沙箱内自动放行 shell**（分报告 7 第 16 条，要修订 ADR 0012、0016、0043，并先用 seccomp 堵住 Unix socket）。没有这一条，"无人值守加沙箱"就跑不起来：要么每次都问，要么只能 `full --yes`，故事的核心立不住。成本中到高。
  2. **`radian exec --output jsonl` 加结果摘要**（分报告 4，修订 ADR 0032）。CI 和外部编排都需要它。成本低。
  3. **herdr `--kind radian` 的 PR，以及 `agent prompt --wait` 能驱动 radian**（ADR 0034、6，已定，正在做）。成本低。
  4. **发布和分发**：公开仓库，提供 release 二进制和 curl/brew 安装（新 ADR：发布渠道、签名、升级策略。按 ADR 0013 T3，"更新检查"属于默认禁止的隐性出网，所以只能由用户主动 `radian update` 或走包管理器）。成本中。
  5. **一份可信页**，把 ADR 0013 T5 的出网断言、沙箱探针矩阵、journal 回放检查写成用户能自己跑的 `radian doctor --privacy` 一类命令，或者一份公开的测试清单。成本低。
  6. **评测和对比数据**：分报告 6 的任务集和指标提取器，公开和 pi 的配对结果。成本中。
  7. 外部命令 hook 里的 `stop`（轮末验证，分报告 8），让"交出去的活儿做完会自检"成立。成本中。
- **正面竞争**：pi（herdr 用户里最常见的 agent 之一，推断）、Codex `exec`、`claude -p`。Radian 能赢的点：资源占用、可回放审计、真沙箱、不出网；opencode 的安全口碑反倒衬托了这几点。
- **风险**：herdr 是小众工具（1 万多星），而且 herdr 支持所有 agent，"herdr 一等公民"本身不算护城河，只是体验好一点；默认 `full` 和"放心交出去"之间有张力，第 1 条不做，这个故事就是空的；无人值守场景会把成本和质量问题放大，没有评测就讲不出"放心"。

### 方向 A（长期护城河，先不单独推广）：可审计、可嵌入的 Rust agent 内核

- **目标用户**：要在自己的 Rust 产品里嵌入 agent 的团队，比如 devtools、基础设施、桌面应用；对审计和回放有硬需求的场景（合规、受监管的行业，推断）。
- **要讲的故事**："agent 的 SQLite"：链进你的进程，没有 daemon，没有 Node，不起子进程；每一步都能回放校验；provider 有契约测试；hook 出错一律 fail closed；默认不出网。
- **已有支撑**：`agent-core` 的边界（设计文档 §3）、journal seam 和 JSONL v1 schema 加 fixture（ADR 0006）、`agent-provider-contract`、`radian-session`、`radian-runtime`、`radian-permissions`、`radian-sandbox` 这些分层的 crate。
- **还缺什么**：把 crate 发布到 crates.io，承诺 semver，给出 docs.rs 文档和例子；把"coding agent 作为库"的高层门面理顺（`radian-host` 现在是 `publish = false`）；API 稳定期（和现在每天改 ADR 的节奏冲突）；可能还需要 C ABI 或其他语言绑定（成本高，未评估）。
- **正面竞争**：Claude Agent SDK（绑定 Claude、按 API 计费）、Codex SDK 和 app-server、pi SDK 加 Pi Durable（TS，已经喊出可能移植到 Rust）、Rig（Rust 库，设计文档 §2）、Block 的 goose（Rust，是否适合嵌入未核实）。
- **风险**：市场窄，只面向 Rust；发布即背上 API 兼容的维护负担，会拖慢 ADR 的演进；Pi Durable 会直接吃掉"可恢复"这个卖点；没有方向 B 的产品带用户，库很难被发现。
- **为什么现在不推**：收益要等 API 稳定之后才有，成本却是立刻开始（每次 ADR 修订都要考虑兼容）。但方向 B 里的每一项"可信"证据都来自这层内核。所以内核要继续当作头等工程资产来维护（不放松 journal-selfcheck 和契约测试），等 B 有了用户再发布。

### 方向 C（不推荐）：pi 的严谨替代品，开箱即用的安全与可恢复

- **目标用户**：嫌 pi 配置多、怕 pi 默认无权限、受过 npm 供应链之苦的 pi 用户。
- **要讲的故事**："和 pi 一样小，但默认就有权限、沙箱和崩溃恢复，而且是一个原生二进制，没有 npm 依赖树。"
- **已有支撑**：`import-pi`、pi 兼容的主题、17 个工具、提示词前缀稳定（分报告 3）、权限和沙箱。
- **还缺什么**：扩展生态（pi 有 5,337 个包，Radian 是 0）、prompt 模板和 hook、文档网站、上手引导。
- **正面竞争**：pi（116k 星、30 天 15 个版本、有公司）和 omp。
- **风险**：在对方的主场打。pi 1.0 已经补上了 MCP、加上了 codemode，Durable 也在路上；"极简"并不是 Radian 的独有卖点。owner 的近期目标"取代 pi"可以是**自用的验收标准**，不应该变成**对外的定位**。

### 推荐和理由

**选 B，A 作为证据层，C 只当内部验收。**
1. B 和 owner 已经定下的方向一致：ADR 0043 原话"成为一个可以真正 herdr 编排的 agent"（0043:13），以及默认 `full`、需要时按项目开沙箱（0043 §2）。不需要推翻任何 ADR，只需要补上第 1、2、4 条。
2. 市场有空档：opencode 的星数说明"不锁 provider"的需求很大，但它的安全口碑很差；Claude Code 和 Codex 的锁定、常驻 daemon 和自动更新招来了明确的抱怨（2.3）。"轻、可审计、不出网、真沙箱"正好对上这些抱怨。
3. B 的卖点每一条都能用测试证明（启动时间、RSS、出网断言、沙箱探针、journal 回放），不靠口号。这符合 Radian 的工程文化，也是一个单人项目唯一能和大厂比的东西：可信度。
4. 成本可控：最大的一项（沙箱内自动放行）本来就是分报告 4、7 排在最前的改进，做了对速度、交互摩擦和定位三方面都有收益。

**怎么验证定位是否成立**（ADR 0013 禁止遥测，所以不能靠埋点）：
- 发布后看公开指标：GitHub 星数、release 下载数、crates.io 下载数，每月记一次，写进 `docs/research/`。
- 内部指标：分报告 6 的评测集，Radian 对 pi 的成功率和单次成功成本；无人值守场景里不需要人介入就完成的比例。
- 第一个外部用户的反馈（比如 herdr 社区），问的是"他们为什么选 Radian"，看答案是不是"放心交出去"这件事。

## 来源

- [Claude Code overview](https://code.claude.com/docs/en/overview) · [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview)
- GitHub 首页：[openai/codex](https://github.com/openai/codex) · [codex releases](https://github.com/openai/codex/releases) · [earendil-works/pi](https://github.com/earendil-works/pi) · [can1357/oh-my-pi](https://github.com/can1357/oh-my-pi) · [anthropics/claude-code](https://github.com/anthropics/claude-code) · [anomalyco/opencode](https://github.com/anomalyco/opencode)
- npm 下载接口：`api.npmjs.org/downloads/point/last-week/{@openai/codex, @anthropic-ai/claude-code, opencode-ai, @opencode/cli, @earendil-works/pi-coding-agent, @mariozechner/pi-coding-agent, @oh-my-pi/pi-coding-agent}`
- [Pi 1.0](https://earendil.com/posts/pi-1-0/) · [Pi Durable](https://earendil.com/posts/pi-durable/) · [eesel：Pi 解读](https://www.eesel.ai/blog/pi-coding-agent)（作者是卖其他产品的厂商）· [implicator：harness rebellion](https://www.implicator.ai/pi-is-not-a-claude-code-rival-it-is-a-harness-rebellion/)
- [falcao：Anthropic 封锁第三方的时间线](https://falcao.org/posts/anthropic-claude-access-crackdown-ecosystem-fallout/) · [The Register：用量上限](https://www.theregister.com/2026/01/05/claude_devs_usage_limits/) · [Morph：Reddit 上的 Claude Code](https://www.morphllm.com/claude-code-reddit)（竞品厂商）
- [Vaughan：Reddit 上的 Codex 口碑](https://codex.danielvaughan.com/2026/03/30/reddit-codex-cli-sentiment-march-2026/) · [whyops：omp 与 hashline](https://whyops.com/blog/oh-my-pi-hype-hashline) · [wren.wtf：Annoying and alarming things about OpenCode](https://wren.wtf/shower-thoughts/stop-using-opencode/) · [LDS：OpenCode Go](https://letsdatascience.com/news/opencode-launches-10-monthly-open-source-agent-f29289b1) · [towardsai：opencode 对比](https://pub.towardsai.net/opencode-just-dethroned-claude-code-with-172-000-stars-i-ran-both-and-its-78-slower-508961c12959) · [tecmint：herdr](https://tecmint.com/herdr-run-ai-coding-agents-in-linux-terminal)
- HN 帖子（评论摘要来自 `hn.algolia.com/api/v1/items/<id>`）：49926069、48883275、50060694、50094994、50133401、46549823、46581095、47460525、48994611
