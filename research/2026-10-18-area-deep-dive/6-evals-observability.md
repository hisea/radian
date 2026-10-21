# 深挖 6：评测与可观测性

调研日期：2026-10-18。对手源码路径都相对于 scratchpad 下的 `comp/` 目录（例如 `pi-mono/...` 指 `comp/pi-mono/...`）。Radian 路径相对于仓库根 `<repo>`。网页结论都带链接和页面日期，没有核实的写“未核实”或“推断”。

## 1. 一句话结论

Radian 的 journal 已经比五家里任何一家的会话文件都更适合做离线评测：每条记录都有 `seq` 和微秒级 `ts`，工具起止时间、每次调用的 usage 和**当时价格下的费用**都已持久化。缺的是三样东西：一套能自动判分的任务集，一个把 journal 变成指标的提取器，以及和 pi 跑同一套任务的配对对比。pi（`packages/evals`）和 Harbor（已内置 `pi` agent）都可以直接借用。第一步不用改任何 Rust 代码，也不碰 ADR。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 自有评测体系 | 没有任务成功率评测；只有 `#[ignore]` 的性能测试（`benchmarks/perf-baseline.md` “测量代码位置”） | 有：`packages/evals`，基于 vitest-evals 和 Docker 隔离，做 with/without docs 的配对比较（`pi-mono/packages/evals/README.md:1-25`） | 仓库里没有评测目录（`find -iname '*eval*'` 无结果）；有 250 个 core 集成测试文件（`codex-rs/core/tests/suite/`） | 闭源，未公开评测集 | 有基准命令：`omp bench`（TTFT、tok/s、缓存）、`omp if-bench`（指令遵循）（`src/commands/bench.ts:25-29`、`src/if-bench/index.ts:1-9`） | 没有评测集；core 有 266 个测试文件，cli 有 49 个，另有 `simulation` 包 |
| 测什么 | harness 延迟：启动、fsync、渲染、resume | 文档带来的提升、冒烟测试、文档与实现是否一致（`evals/documentation-audit.eval.ts`） | 协议和行为的正确性：mock SSE 加上下文快照（`core/tests/common/responses.rs:753`、`context_snapshot.rs:1-5`） | — | provider 吞吐、缓存前缀、多轮工作记忆 | 用真实 HTTP 录制回放（`http-recorder`）测 runner（`core/test/session-runner-recorded.test.ts`） |
| Terminal-Bench 2.0 | 无 | 榜上没有条目 | GPT-5.5 82.2%（第 4 名，2026-04-23） | Opus 4.6 58.0%（2026-02-07） | 无 | Opus 4.5 51.7%（2026-01-12） |
| 会话日志格式 | JSONL v1，有信封、`seq`、`ts`、`critical`（ADR 0006 §3） | JSONL v3 树形结构，每条 assistant 消息自带 usage 和 cost（`coding-agent/docs/session-format.md:45-121`） | rollout JSONL，持久化 `TokenCount`、`TurnStarted`、`TurnComplete`（`codex-rs/rollout/src/policy.rs:127-139`） | `~/.claude/projects/` 下的 JSONL（[headless 文档](https://code.claude.com/docs/en/headless)，属于文档） | 沿用 pi 系 JSONL，由 `omp stats` 导入（推断） | SQLite，消息数据存在 JSON 列里（`core/src/session/stats.ts:136-141`） |
| 离线分析工具 | 一个一次性脚本 `scripts/measure-tool-concurrency.py` | footer 根据会话条目算缓存命中率（`footer.ts:122-133`） | `rollout-trace` 离线 reducer，默认关（`rollout-trace/README.md:3-20`） | — | `omp stats` 把会话文件同步到数据库并汇总，提供本地看板或 `--json`（`src/cli/stats-cli.ts:70-93`） | `opencode stats`，用 SQL 跨会话按模型、工具、成本、天汇总（`cli/src/commands/handlers/stats.ts`） |
| 用量、成本、缓存的展示 | footer 显示费用，不完整时显示 `≥$X`（`radian-host/src/cost.rs:1-19`）；**不显示缓存命中率** | footer 显示 tokens、cost 和 `latestCacheHitRate` | `exec --json` 的 `turn.completed` 带 usage，含 cached 和 reasoning（`exec/src/exec_events.rs:48-73`） | `--output-format json` 输出 `total_cost_usd` 和按模型的成本（文档） | `omp stats`、`omp usage` | `opencode stats --models --tools --cost` |
| 延迟指标 | 只能用 journal 的 `ts` 差推算；流式增量不持久化（ADR 0010） | 评测里记 `totalMs`（`evals/src/harness.ts:461`） | OTel 指标 `codex.turn.ttft.duration_ms` 等（`otel/src/metrics/names.rs:36`） | OTel 事件 `api_request.duration_ms`、`tool_result.duration_ms`（文档） | `omp bench` 测 TTFT | `stats` 里有工具耗时（`stats.ts:67-74`） |
| 遥测默认 | **不存在任何上报路径**（ADR 0013 决定 1） | `pi-telemetry` 只定义契约，不带 exporter（`packages/telemetry/README.md`） | analytics **默认开启**，除非设 `analytics.enabled=false`（`core/src/session/session.rs:1647-1659`、`analytics/src/client.rs:314-323`） | OTel 需显式开启（`CLAUDE_CODE_ENABLE_TELEMETRY=1`，文档）；另有 Anthropic 自己的运营遥测（未核实细节） | OTLP 导出开关默认为 true，但要配置 `OTEL_*` 端点才会导出（`src/telemetry-settings.ts:8-19`）；auto-QA 在用户同意后推送到 `qa.omp.sh`（`src/tools/report-tool-issue.ts:1-32`） | 设置 `OTEL_EXPORTER_OTLP_ENDPOINT` 时导出（`cli/src/index.ts:128-135`）；不设时不导出（推断） |
| 回放和回归测试 | `journal-selfcheck` 用 journal 回放来核对 transcript；两套 provider 契约测试 | `fauxProvider()` 返回脚本化响应（`packages/ai/README.md:1414`） | wiremock SSE；上下文快照会归一化易变文本 | — | — | `http-recorder` 先录真实 HTTP，再按 cassette 确定性回放；`simulation` 提供模拟 provider 和终端录制（`simulation/src/recording.ts:8-33`） |

公开榜单来源：[tbench.ai TB 2.0 榜单](https://www.tbench.ai/leaderboard/terminal-bench/2.0)（2026-10-18 抓取）。

## 3. 各家的关键做法

### pi（`pi-mono/packages/evals`，1.3.0）

- **配对设计**。每个 case 按 `(case, variant, model, runNumber)` 预先规划，每个 arm 都在全新容器里跑（`README.md:68-78`）。重复运行时交替顺序，减少顺序偏差（`README.md:80`）。
- **严格的“不可用”语义**。有缺失、重复或出错的 arm 时，这一对作废；只要有一对作废，就不给总通过率。缺少的遥测记为“不可用”，不当作 0（`README.md:103`）。这和 Radian ADR 0007 J1 的“没报就是 None，不是 0”是同一个原则。
- **每次运行都采运营指标**：`inputTokens`、`outputTokens`、`cacheReadTokens`、`cacheWriteTokens`、`toolCalls`、`estimatedCostUsd`、`totalMs`（`evals/src/harness.ts:391-411`、`report.ts:466-472`）。没有价格时就不写成本（`harness.ts:392-394`）。
- **落盘内容**：`protocol.json`、`expected-runs.json`、`observations.jsonl`、原生 `session.jsonl`，加上报告（`README.md:91-100`）。
- **判分是确定性的**：模型必须在最后调用一个结构化的 `submit_*` 工具，用 JSON schema 约束输出，判分只看这个调用（`evals/documentation-audit.eval.ts:9-29`）。
- 测试用 `fauxProvider()`；`--mode json` 输出严格的 LF 分隔 JSONL（`coding-agent/docs/json.md`）。
- pi 的价值主要在**效率**，不在 TB 分数。PointFive 2026 年 8 月的论文测得：Claude Code 的固定前缀有 15,983–20,330 tokens，PI.DEV 只有 1,147–1,642；在成功率相近时，Claude Code 每次成功的成本是 5–30 倍，回合数是 2–7 倍（[arXiv 2608.01347](https://arxiv.org/pdf/2608.01347)）。论文的结论是：**每次成功的成本**才是该看的指标，单看 token 数说明不了是哪一种机制省了钱。

### Codex CLI

- 没有评测集，但测试基础设施很重：用 wiremock 构造 SSE（`core/tests/common/responses.rs:753-790`）。`context_snapshot` 把每次请求的模型可见上下文渲染成归一化快照，用来检测上下文漂移（`core/tests/common/context_snapshot.rs:1-5`）。
- `codex exec --json` 输出 `thread.started`、`turn.started`、`turn.completed{usage}`、`item.*`（`exec/src/exec_events.rs:11-36`）。usage 分出 `cached_input_tokens`、`cache_write_input_tokens` 和 `reasoning_output_tokens`（`:61-73`）。
- rollout 只持久化必要的事件，`TokenCount` 在其中（`rollout/src/policy.rs:133`）。`rollout-trace` 的思路是“先观察，后解释”：热路径只写原始事件，语义图交给离线 reducer 来建（`rollout-trace/README.md:14-20`）。它只在设置 `CODEX_ROLLOUT_TRACE_ROOT` 时开启。
- OTel 指标覆盖了“快不快”的各个环节：`codex.turn.ttft.duration_ms`、`codex.turn.ttfm.duration_ms`、`codex.tool.call.duration_ms`、`codex.turn.cost_microusd`、`codex.responses_api_overhead.duration_ms`、`codex.startup.phase.duration_ms`（`otel/src/metrics/names.rs` 及同目录）。
- 遥测默认开启：`AnalyticsEventsClient` 只有在 `analytics.enabled == Some(false)` 时才禁用（`core/src/session/session.rs:1647`、`analytics/src/client.rs:321`）。app-server 默认关闭（`cli/src/main.rs:604-621`）。
- 榜单上，**同一模型换 harness，分数差得很多**：GPT-5.2 在 Codex CLI 上是 62.9%，在 Terminus 2 上是 54.0%（[tbench.ai](https://www.tbench.ai/leaderboard/terminal-bench/2.0)）。SWE-bench Pro 上，GPT-5.2 Codex 用自家脚手架得 56.4%，用 Scale 的脚手架得 41.0%（[Morph，2026-09-28，厂商文章](https://morphllm.com/swe-bench-pro)）。

### Claude Code（全部来自文档）

- OTel 需显式开启（`CLAUDE_CODE_ENABLE_TELEMETRY=1`）。指标有 `claude_code.cost.usage`、`token.usage{type=input|output|cacheRead|cacheCreation}`、`active_time.total`。每次 API 调用产生一个 `api_request` 事件，带 `duration_ms`、缓存 tokens 和 `cost_usd`；每次工具运行产生一个 `tool_result` 事件，带 `duration_ms`、结果字节数和 `decision_source`（[monitoring-usage](https://code.claude.com/docs/en/monitoring-usage)）。
- `claude -p --output-format json` 输出 `total_cost_usd` 和按模型的成本，并说明这是**客户端估算**。另有 `--max-budget-usd` 和 `--bare`；`--bare` 不加载 hooks、skills、MCP 和 CLAUDE.md，适合“每台机器结果都一样”的脚本场景（[headless](https://code.claude.com/docs/en/headless)）。
- 榜单：TB 2.0 上 Opus 4.6 用 Claude Code 得 58.0%，用 Terminus 2 得 62.9%（[tbench.ai](https://www.tbench.ai/leaderboard/terminal-bench/2.0)）。Artificial Analysis Coding Agent Index v1.5（页面没写日期）同时报告 cost/task、time/task、turns/task、tokens/task 和 cache hit rate。Claude Code（Sonnet 5.5 max）每个任务 $14.19、1.5 小时、265.9 轮；Codex（GPT-6.1 Sol xhigh）每个任务 $1.04、15.5 分钟、40.1 轮（[AA 对比页](https://artificialanalysis.ai/agents/coding-agents/comparisons/claude-code-vs-codex)）。这一组指标正好可以作为 Radian 的指标模板。

### omp（oh-my-pi 18.12.1）

- `omp stats` 把会话文件增量同步到本地数据库，再刷新 rollup，提供 `--json`、`--summary` 或 `127.0.0.1:3847` 看板（`src/cli/stats-cli.ts:70-93`、`src/commands/stats.ts:13-17`）。
- `omp bench` 有 chat、prefill、generation、mix 四种 profile，测 TTFT、tok/s 和缓存前缀（`src/commands/bench.ts:25-29`）。`omp if-bench` 测多轮工作记忆（`src/if-bench/index.ts:1-9`）。
- auto-QA：模型把工具异常写进本地 SQLite，用户同意后推送到 `qa.omp.sh`（`src/tools/report-tool-issue.ts:1-32`）。这是“用真实使用找工具 bug”的思路，只是带上了上报。
- Composio 的测试用 DeepSeek V4 Flash 跑 30 个任务：omp 成功 17/30，最高，但每个任务 272 s，最慢；Claude Code 每个任务 122 s，最快；opencode 每次成功 $0.073，最便宜；30 个任务里有 7 个的成败只取决于 harness（[The Decoder，2026-08-06](https://the-decoder.com/claude-code-is-the-fastest-agent-framework-but-costs-nearly-three-times-more-than-the-cheapest-rival/)，二手报道）。

### opencode（2.0.32）

- 注意：本快照里**没有** `packages/opencode/src`，主体在 `packages/core` 和 `packages/cli`。
- `opencode stats` 用 SQL 直接从消息 JSON 里取 `tokens.input/output/reasoning/cache.read/cache.write` 和 `cost`，按模型、工具（调用、成功、失败、未完成、耗时）和时间段聚合（`core/src/session/stats.ts:60-74, 136-141`）。
- `@opencode/http-recorder` 录一次真实的 HTTP/WebSocket 流量，之后用 JSON cassette 确定性回放（`http-recorder/README.md`）。runner 测试直接用录下的流量（`core/test/session-runner-recorded.test.ts`）。
- `@opencode/simulation` 提供模拟 provider、控制服务器，以及按毫秒计时的终端录制（`simulation/src/recording.ts:8-33`）。

### 公开基准本身

- **Terminal-Bench 2.0**：89 题，用 Harbor 运行；每题一个 Docker 容器，跑完执行测试脚本，结果二值（[Morph 的总结](https://www.morphllm.com/terminal-bench-2)，2026-03-09）。Harbor **已内置 `pi`、`codex`、`claude-code`、`opencode`**（[Harbor 文档](https://docs.harborframework.com/agents/pre-integrated-agents)）。自定义 agent 继承 `BaseInstalledAgent`，实现 `setup` 和 `run`，往 `AgentContext` 填 `n_input_tokens`、`n_cache_tokens`、`n_output_tokens`、`cost_usd`；可以写 ATIF trajectory；用 `--agent-import-path` 加载（[自定义 agent 指南](https://mintlify.com/harbor-framework/harbor/guides/custom-agents)）。AA 已经在用 “Terminal-Bench 4.0”；更高版本的细节未核实。
- **SWE-bench Pro**：1,865 题，分为公开 731、商业 276、留出 858。Scale 的榜单用统一脚手架，但部分行用了不同的回合上限（50 和 250）。分数受脚手架影响很大（[Morph，2026-09-28](https://morphllm.com/swe-bench-pro)，厂商文章，有利益相关）。

## 4. Radian 的优势

1. **journal 本身就是一份完整的 trajectory**。每行都有 `seq`、UTC 微秒 `ts`、`event` 和 `critical`（ADR 0006 §3）。`ToolExecutionStarted` 和 `ToolExecutionSettled` 夹住了每个工具的实际执行时间（§5.3/5.4）。`TurnFinished.outcome` 的终态枚举很完整，包括 `incomplete/max_tokens`、`budget_exceeded`、`cancelled/deadline_exceeded`（§5.6）。`scripts/measure-tool-concurrency.py` 已经证明：只靠 journal 就能算出模型、工具和其他时间的占比（工具 65.5%，模型 24.6%，见 `research/2026-10-15-tool-concurrency-measurement.md` §5）。
2. **费用口径比对手更严**。每次调用按**当时的价格**记 `fee.micros`，价格表缺项时标 `incomplete`（`radian-session/src/fee.rs:1-31`、ADR 0007）。缓存命中只按缓存价计一次（ADR 0007：`uncached_input()` 和 `prompt_total()`）。pi 和 Claude Code 给的都是“估算”。Radian 换模型或价格表更新之后，旧会话的成本也不会变。
3. **“没报就是 None”** 和 pi 评测的“缺失就是不可用”一致（ADR 0007 J1），聚合时不会把缺失当成 0。
4. **零遥测，而且做得彻底**（ADR 0013 决定 1：路径根本不存在）。评测完全在本地进行，不和隐私承诺冲突。
5. 已有确定性的回放核对（`journal-selfcheck`）和两套 provider 契约测试，回归测试的底子比 Codex 以外的几家都好。
6. **dogfood 记录已经在用 journal 计数**，例如 D25：“479 次工具调用，7 次压缩，前 239 次调用零编辑”（`research/2026-10-12-radian-tui-dogfooding.md`）。这些就是现成的任务和行为指标。

## 5. Radian 的劣势和缺口

1. **没有任务成功率评测**，所有 harness 改动都只能靠 dogfood 来判断。`radian-gap-analysis.md` 已把它列为第一缺口，本节不重复，只补下面这些具体的缺项。
2. **`radian exec` 没有机器可读的结果摘要**。stdout 只输出模型的话，退出码只区分 0/1/2/130（`crates/agent-cli/README.md` “Exit codes”）。Harbor、pi 和 Claude Code 的 `--output-format json` 那种 tokens、成本、轮数摘要，Radian 目前必须靠解析 journal 才能拿到。
3. **TTFT 和单次模型调用耗时不在 journal 里**。流式增量只在内存里（ADR 0010）。现在只能拿 “assistant `MessageAppended.ts` − 上一个锚点” 来近似，这个值里混进了 harness 开销（测量文档 §“Metric definitions” 5）。Codex 和 Claude Code 都有单次调用的 `duration_ms`。
4. **usage 可能缺失**。OpenAI 兼容端点默认只对 `api.openai.com` 请求 usage（ADR 0007 J8）。ab3 有 78 条 `ModelUsageRecorded`，比 assistant 消息少（测量文档 §Data）。原因是端点没回 usage 还是没请求，尚未核实。评测必须显式加 `--include-usage`。provider 描述符是否已经覆盖了这个默认值，未核实。
5. **不显示缓存命中率**。radian-host 和 radian-tui 里 grep 不到 hit rate。pi 的 footer、AA 和 Codex 的 exec 都会显示。
6. **没有跨会话汇总**（没有 `stats` 命令），对比 `opencode stats` 和 `omp stats`。
7. **没有基于录制流量的回放**。契约测试用手写夹具，没有 opencode 那种“录一次真实流量，之后确定性回放”的 cassette。

## 6. 改进方案（按收益/成本排序）

| # | 做什么 | 为什么能更快或更有效 | 成本 | ADR | 怎么验证 |
|---|---|---|---|---|---|
| 1 | **journal 指标提取器**：在 `scripts/` 加一个只用标准库的 Python 脚本（与现有脚本同风格，`python3 -I`），把一个或多个 journal 变成每次运行一行 JSON（指标定义见 §7.4） | 后面每一项都靠它度量。它直接复用已有的 `ts`、`fee` 和工具标记，不改产品代码 | 低（约 1 天，可以交给 pi） | 无 | 拿 ab2、ab3、t4 三份 journal 核对：工具对数 479/479，时间占比与测量文档 §5 一致 |
| 2 | **dogfood 任务集**：从 D7、D11、D16、D18、D25、D26 各提一个任务，固定 base commit，配一个确定性的判分脚本 | 这些是真实任务，而且问题就是“先核实很久才动手”这类效率问题（D24–D26） | 中（每题半天） | 无 | 每题先在已知正确的结果上跑判分脚本，必须通过；在空白 diff 上跑，必须失败 |
| 3 | **Harbor 自定义 agent `radian`**，和 Harbor 内置的 `pi` 跑同一个 TB 2.0 子集 | 拿到外部可比的成功率，并在同模型、同任务下和 pi 配对比较。榜单显示 harness 带来的差距可达 8–15 分 | 中 | 无（只在仓库外加一个适配层，或者放进 `evals/`） | 先跑 hello-world 任务，核对 Harbor 的 reward 和 journal 里的 `TurnFinished` |
| 4 | **`radian exec --summary-json <FILE>`**：结束时写一个文件，含 outcome、session_id、wall、模型调用数、工具调用数、tokens（分四类）、缓存命中率、`fee` 和 `incomplete` | Harbor 填 `AgentContext` 不必再解析 journal；脚本也能拿到 Claude Code `--output-format json` 那样的摘要 | 低到中 | 改动 `radian exec` 的输出契约，需要修订 ADR 0032（或新开一个小 ADR）；数据本身仍只落本地，符合 ADR 0013 T2 | 断言：摘要里的数字等于提取器（#1）从同一个 journal 算出的结果 |
| 5 | **评测里强制拿到 usage**，并统计缺 usage 的调用占比；如果默认值会让常用端点缺 usage，就评估是否改默认 | 缺 usage 时 token、缓存和成本都算不出来，评测会被作废 | 低 | 改默认值属于 ADR 0007 J8 的决定，需要新修订 | 报告里输出 “usage 覆盖率 = 带 usage 的调用数 / assistant 调用数” |
| 6 | **缓存命中率进 footer 和摘要**；再加一个只读本地的 `radian stats`（按天、模型、工具汇总，可以复用 SQLite 索引） | 缓存命中直接决定成本（PointFive：缓存让账单降了 61–75%）。`prompt_cache_key` 一类的优化需要这个数字来验证 | 中 | footer 组件属于 ADR 0027 的贡献项；`stats` 是新的子命令；纯本地读取，不触 ADR 0013 | 单测：一个 OpenAI 形状的样本和一个 Anthropic 形状的样本，按 `prompt_total()` 算出的命中率一致 |
| 7 | **持久化单次调用的计时**：新增一个非 critical 事件，例如 `ModelCallTimed{ttft_ms, total_ms}`，跟在 `ModelUsageRecorded` 后面 | 把“模型时间”拆成排队、TTFT 和解码，才能判断流式提前派发工具（测量文档最后一条）值不值得做 | 中 | ADR 0006 §4 允许新增非 critical 事件而不升 schema，但它是一个新的持久事实，需要 ADR 0006 和 0010 的修订；每次调用多一行，但不是同步点，不加 fsync（ADR 0040） | fixtures 和 `check-fixtures.sh`；用 mock 服务器注入固定延迟，断言测得的值落在误差范围内 |
| 8 | **录制回放回归**：把 dogfood 会话里的真实 provider SSE 录成 cassette，回放驱动 `radian exec`，断言工具序列和最终状态 | 改 harness 后能在零成本、确定性的条件下发现行为漂移，补上真实任务评测之间的空档 | 中到高 | 可能要给 provider 加一个注入点；需对照 provider 契约文档的“绝不编造 wire 值”一条 | 回放同一份 cassette 5 次，结果一致；人为改一个工具描述，测试应该变红 |
| 9 | （不推荐现在做）OTel 或任何上报 | 对“更快”没有直接帮助 | — | ADR 0013 决定 1：路径不存在，要加必须另开 ADR，且默认关 | — |

## 7. Radian 评测方案设计（只写设计，不写代码）

### 7.1 目标与原则

- 主指标是**单次成功成本**和**成功率**。耗时、token、缓存命中率和工具调用数是解释指标，用来说明某个结论是怎么来的。这和 PointFive 的结论一致：只看 token 判断不了是哪种机制起了作用。
- 判分只看外部验证器（Harbor 测试脚本或我们自己的判分脚本），不信 agent 自己说“完成”。journal 里的 `TurnFinished.outcome` 只作为辅助列，并单独统计“agent 自称完成但验证失败”的比例。
- 照搬 pi 的作废规则：缺一个 arm 这一对就作废，有作废时不报总数；缺失的数值记为不可用，不当作 0。

### 7.2 任务来源（最小可行版本共 20 题）

**A. Terminal-Bench 2.0 子集（15 题）**

- 从 89 题里按难度分层抽 15 题，偏向软件工程类（构建、调试、改代码）。排除需要 GPU、超过 30 分钟或明显依赖特定模型知识的题。
- 题单固定写进 `protocol.json`，换题必须换协议版本。
- 理由：外部可比；Harbor 已内置 `pi`，配对成本最低。

**B. dogfood 提炼的真实任务（5 题）**

每题有固定的 base commit、一条提示词和一个确定性的判分脚本。

| ID | 来源 | 任务 | 判分 |
|---|---|---|---|
| R1 | D11 | 给两个二进制加 `--version` | `cargo test` 通过，且两个二进制的 `--version` 输出 `radian <ver>` |
| R2 | D7 | 核对 README 提到的名字在代码里存在，只修文档 | 脚本抽出所有反引号名字，逐个 grep，全部存在；diff 只碰 `*.md` |
| R3 | D26 | 让指南 §8 与 `radian-tui/README.md` 的键位表一致 | 解析两张表，键集合相等；diff 只落在 §8 的行范围里 |
| R4 | D16 | 根据 `--help` 写 README 的 flags 段 | 每个 `--help` 里的 flag 在 README 出现，README 里的 flag 都真实存在 |
| R5 | D25 | 中等规模的文档同步 | 判分同 R3，并额外记录“第一次编辑之前的调用数” |

- R3 和 R5 专门测量 D24–D26 那种“先核实很久才动手”的行为。这类任务里效率比成功率更能说明问题。
- 最小可行版本以后，可以按 D 编号继续扩到 20–30 题，与 `coding-agent-optimization.md` 的建议一致。

### 7.3 怎么接入（radian exec 无头模式）

一次运行的命令形状（flag 都已存在，见 `crates/agent-cli/src/lib.rs` 的 `Cli`）：

```
radian exec -p "<instruction>" --workspace <task dir> --profile full --yes \
  --session-id <run id> --session-dir <logs>/sessions --include-usage \
  --max-model-calls 1000 --turn-timeout-secs <secs> [--offline-catalog]   # 后者固定 catalog，可选
```

- `--session-id` 固定会话文件名，journal 就是 `<session-dir>/<run id>.jsonl`（`radian-host/src/session/open.rs:160`）。subagent 子会话也写在同一目录，靠 `SessionCreated.parent` 关联，指标要把它们一起算进去。
- `--profile full`：Harbor 容器本身就是隔离边界；Radian 套在沙箱里时会继承沙箱（ADR 0016）。如果用 `workspace`，要单独记录沙箱探针的开销（约 25 ms，`benchmarks/perf-baseline.md` §1）。
- 退出码 0 表示回合完成，1 表示失败（agent-cli README）。这只是辅助信号，判分仍以验证器为准。
- **Harbor 适配层**：写一个 `BaseInstalledAgent`。`setup` 往容器里装一个静态的 release 二进制；`run` 执行上面的命令；结束后把 journal 下载到 `logs_dir`，用提取器（#1）把 tokens 和成本填进 `AgentContext`。ATIF 转换可选，最小可行版本不做。
- **dogfood 题**不走 Harbor，用一个本地 runner 处理：为每题 `git worktree add` 到 base commit，跑同一条命令，然后执行判分脚本。worktree 放在 `.radian-worktrees/` 或 scratchpad，与 AGENTS.md 的规则一致。

### 7.4 指标，以及怎么从现有 journal 直接算

全部只读 journal，不需要新事件。记号：E(x) 表示事件 x 的集合。

| 指标 | 算法 | 注意 |
|---|---|---|
| 成功率 | 验证器结果（Harbor reward 或判分脚本） | journal 不参与 |
| agent 自称完成 | 最后一个 `TurnFinished.outcome == completed` | 与成功率交叉，得出“自称完成但失败”的比例 |
| 端到端耗时 | runner 测的进程墙钟（含启动）；journal 口径为最后一个 `TurnFinished`/`TurnFailed` 的 `ts` 减去 `SessionCreated.ts` | `ts` 只用来求差，不用来排序（ADR 0006 §3） |
| 工具时间 | Σ(`Settled.ts` − `Started.ts`)，按 `call_id` 配对 | 与 `measure-tool-concurrency.py` 的定义相同 |
| 模型时间（近似） | 每条 assistant `MessageAppended.ts` 减去上一个锚点（上一个 Settled、用户输入或回合开始） | 含 harness 开销，要在报告里注明；做了 #7 之后才有精确值 |
| 模型调用数 | assistant 变体的 `MessageAppended` 条数，压缩用的调用另记（`CompactionUsageRecorded`） | — |
| 工具调用数 | `ToolExecutionStarted` 条数，并按 `name` 分类；被拒绝的调用等于 assistant `tool_calls` 里没有对应 Started 的 id | D10 的 shell 占比就是这样算的 |
| 输入 token | Σ `prompt_total()`，即 `input`，当 `input_includes_cache == false` 时再加上 `cache_read + cache_write` | 字段缺失的旧记录按 ADR 0007 推断，推断不了就标为不可用 |
| 输出 token 和推理 token | Σ `output`；推理 token 从 `details` 里按 provider 形状取 | — |
| 缓存命中率 | Σ`cache_read` / Σ`prompt_total()`，按 token 加权；另报每次调用的中位数 | 分母要包含压缩调用 |
| usage 覆盖率 | 带 `ModelUsageRecorded` 的调用数 / 模型调用数 | 低于 100% 时，token、成本和缓存都标为下界 |
| 成本 | Σ `fee.micros`（`ModelUsageRecorded` 加 `CompactionUsageRecorded`），任一条 `incomplete` 就整体标 `≥` | 对比时还要用同一张价格表重算一遍（见 7.5） |
| 单次成功成本 | 这一组所有运行的成本之和 / 成功次数 | 失败运行的花费也要算进去 |
| 压缩 | `ContextCompacted` 次数；因压缩失败导致的 `TurnFailed`（D20、D24） | — |
| 行为指标 | 第一次 `write`/`patch` 之前的调用数；同一路径被 `read` 的次数；shell 里 `sed -n`/`cat` 的占比 | 对应 D24–D26 和 D10 |

### 7.5 怎么和 pi 跑同一套任务做对比

- **同条件**：同一 provider 端点、同一模型和推理级别、同样的回合时间上限、同一容器镜像、同一题单。pi 用 Harbor 内置的 `pi` agent；dogfood 题用 `pi --mode json -p` 跑在同一个 worktree 起点上。
- **重复和配对**：每题每个 harness 跑 k=3 次（最小可行版本；稳定性研究用 5 次），顺序按运行序号交替（照搬 pi 的 `README.md:80`）。按 (题, 序号) 配对。成功率差用配对 bootstrap 给 95% 区间，k=3 时只报区间，不下“显著”的结论。
- **口径统一**：两边的 token 都用 provider 报的原始数。pi 的 usage 在每条 assistant 消息里（`session-format.md:45, 91`），`usage` 条目和压缩条目也要计入（`:115-160`）。成本**用同一张 models.dev 价格表重算**，不用各自的估算；Radian 自己的 `fee` 另列一栏，用来自检。缓存命中率用同一个公式。
- **报告**：每对给出 Δ成功率、Δ单次成功成本、Δ耗时中位数、Δ工具调用数、Δ缓存命中率，再列出只有一边成功的题（Composio 的测试里有 7/30 的题成败只取决于 harness，这些题最值得逐个读 journal）。

### 7.6 落盘和隐私

- 每次评测写进一个被 gitignore 的 `.eval/<时间戳>_<id>/` 目录：`protocol.json`（题单、模型、二进制版本、提交）、`expected-runs.json`、`observations.jsonl`（每次运行一行，由提取器生成）、原始 journal 和 pi 的会话文件、`report.md`。
- 全部在本地。出网只有 provider 请求和 Harbor 拉镜像，不违反 ADR 0013。产物里有提示词和代码，按敏感数据对待（ADR 0013 T4），不提交进仓库。

### 7.7 最小可行版本的范围

- **做**：提取器（#1）；5 道 dogfood 题和它们的判分脚本；Harbor 适配层；15 道 TB 2.0 题；一个模型；Radian 和 pi 各跑 3 次；指标限定为 §7.4 里不需要新事件的那些。
- **不做**：TTFT（等 #7）、`--summary-json`（#4，第二步）、录制回放（#8）、多模型矩阵、SWE-bench Pro（公开集 731 题，成本高，脚手架争议大，放到后面）。
- **预算**：20 题 × 2 个 harness × 3 次 = 120 次运行。按 dogfood 的量级（一题十几到几十分钟），在 Harbor 并行下大约一个晚上能跑完（推断）。成本取决于所选模型，第一次先用 dogfood 用的便宜模型。
- **完成标准**：同一份协议跑两遍，Radian 自身的成功率差落在区间内（检验评测本身是否稳定）；报告能列出“只有一边成功”的题，并附 journal 路径。

## 来源

- [Terminal-Bench 2.0 榜单（tbench.ai）](https://www.tbench.ai/leaderboard/terminal-bench/2.0)
- [Terminal-Bench 2.0 总结（Morph，2026-03-09）](https://www.morphllm.com/terminal-bench-2)
- [SWE-bench Pro 总结（Morph，2026-09-28）](https://morphllm.com/swe-bench-pro)
- [Harbor 内置 agent 列表](https://docs.harborframework.com/agents/pre-integrated-agents)、[Harbor 自定义 agent 指南](https://mintlify.com/harbor-framework/harbor/guides/custom-agents)
- [Claude Code monitoring-usage](https://code.claude.com/docs/en/monitoring-usage)、[Claude Code headless](https://code.claude.com/docs/en/headless)
- [Prompt-Induced Waste in Coding Agents（PointFive，2026-08）](https://arxiv.org/pdf/2608.01347)
- [Artificial Analysis：Claude Code vs Codex](https://artificialanalysis.ai/agents/coding-agents/comparisons/claude-code-vs-codex)
- [The Decoder 报道 Composio 测试（2026-08-06）](https://the-decoder.com/claude-code-is-the-fastest-agent-framework-but-costs-nearly-three-times-more-than-the-cheapest-rival/)
