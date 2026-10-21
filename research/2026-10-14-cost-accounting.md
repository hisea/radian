# footer 的费用与 token 计数核对 —— 只读调研

日期：2026-10-14 · 调研者：agent · 状态：**只读调研**，未改动任何代码、未 git commit、未建 worktree

工作区 `<worktree>`（分支 `radian/dogfood`），`git log -1` = `1ce8621`，与 `main` 同一个 commit（ADR 0027 与 ADR 0033 的那次提交）。**本文就是 `main 1ce8621` 的行为**。

任务：核对 ADR 0027 之后默认 footer 的 `tokens`/`cost` 算得对不对。结论：**有 5 处需要处理的问题**，其中 1 处是明确的重复计费（对 OpenAI 兼容端点的长会话可高估近 10 倍），2 处是漏计（压缩调用、resume 之前的历史）。已核对正确的部分见 §8。

---

## 0. 方法、范围与数据来源

**读的代码**（只读）：

| 主题 | 文件 |
|---|---|
| 单价解析 | `crates/radian-host/src/catalog.rs`（`ModelPrice` 68–86、`parse_price` 89–104、`Catalog::price` 299–302、`Catalog::resolve_offline` 603–625） |
| 费用与格式 | `crates/radian-tui/src/timing.rs`（`cost_micros` 71–92、`format_cost` 140–146、`format_tokens` 120–129） |
| footer 条目 | `crates/radian-tui/src/footer.rs`（`tokens` 871–896、`cost` 897–912、默认两行 `preset_rows` 174–232） |
| 中立 usage 类型 | `crates/agent-core/src/model.rs` 47–72（`ModelResponse.usage`、`ModelUsage`） |
| Anthropic 归一化 | `crates/agent-provider-anthropic/src/stream.rs` 47–99 |
| OpenAI 兼容归一化 | `crates/agent-provider-openai-compatible/src/stream.rs` 241–289 |
| 会话总计 | `crates/radian-host/src/projection.rs` 182–206（`add_usage`）、292–306（读） |
| 唯一写入者 | `crates/radian-tui/src/events.rs` 156–161 |
| 启动接线 | `crates/radian-tui/src/main.rs` 623–640（catalog provider）、821–826（projection）、1027–1036（footer 事实与单价） |
| 压缩计费 | `crates/agent-core/src/journal.rs` 47–58、`crates/agent-core/src/turn.rs` 454–486 与 1249–1306 |
| 预算基线（对照） | `crates/radian-runtime/src/driver.rs` 957–987、179–195 |

**跑的既有测试**（未改任何测试）：`cargo test -p agent-provider-openai-compatible usage`（10 passed）、`cargo test -p radian-tui --test footer`（20 passed）。

**目录数据**（两份，互相印证）：

- 本机缓存 `~/.radian/cache/catalog/models.json`（mtime **2026-10-13 19:05**，228 providers、5.6 MB）——这是 footer 实际会读到的那份（`CACHE_TTL` 24 小时）。
- 线上 `https://models.dev/api.json`（2026-10-14 读，229 providers）——无 key、只读 GET，只用来核对 `opencode-go` 的 `cost` 行是否存在/是否变化。

**没做**：没有真实 API 调用（没有用任何 key）；没有读 `~/.radian/credentials/`、`~/.pi`、任何 key；没有改代码或测试；没有提交。

**行号约定**：`文件:行号`，相对本工作区（`main 1ce8621`）。

---

## 1. 结论（先看这个）

| # | 现象 | 严重度 | 一句话根因 |
|---|---|---|---|
| **F1** | 对 OpenAI 兼容端点（opencode-go 的 deepseek/gpt/grok…）**缓存命中部分的输入被按全价重复计费**；长会话可高估近 10 倍 | **高** | `input ← prompt_tokens` 含缓存命中（`agent-provider-openai-compatible/src/stream.rs:275`），而 `cost_micros` 又把 `input` 与 `cache_read` 各按自己的单价加一次（`timing.rs:87-90`） |
| **F2** | 生成摘要的那次模型调用的 token/费用**完全不进 footer** | 中高 | 压缩调用用 `NoopSink` 且不发 `ModelResponseReceived`（`turn.rs:454-456`、`turn.rs:1252`），usage 只写 journal 的 `CompactionUsageRecorded`，全仓库无消费者；ADR 0018 要求"会话的 usage 总计都计入它" |
| **F3** | **resume 之后** footer 的 `tokens` 全是 `-`、`cost` 直接隐藏，哪怕 journal 里已有 `ModelUsageRecorded` | 中 | TUI 的 projection 是新建的空对象（`main.rs:821`），唯一的写入者是 live 事件（`events.rs:156-161`），没有任何地方把 journal 的 usage 折进 projection（预算基线**有**这条恢复路径：`driver.rs:967-987`） |
| **F4** | `/model` 切换后**单价不更新**（仍用启动时那个模型的），而 token 总计是跨模型混合的 | 中 | `set_model_price` 全仓库只有一个调用点（`main.rs:1036`，启动）；切换路径只重算预算 `B`（`main.rs:736-743`） |
| **F5** | 目录里带**分档定价**（`cost.tiers`）的模型，长上下文请求的单价被低估（opencode-go 32 个内建模型里 7 个；例如 `claude-haiku-5-5` 超过 100K 后是 5 倍） | 低-中 | `parse_price` 只读平坦的 `input`/`output`/`cache_read`/`cache_write`（`catalog.rs:89-104`），忽略 `tiers`/`context_over_200k`/`input_audio` |

另外三条小问题：**F6** `ModelUsage.input` 的语义在两家（甚至同一 adapter 的两种别名之下）不一致，且类型文档没写（这是 F1 的根因）；**F7** 所有计数都是 0 但价格可用时会显示 `$0.00`（与 ADR 0027 的"绝不显示 `$0.00`"冲突）；**F8** `cache_write` 只计费、从不显示。

---

## 2. Q1：`input` 到底含不含缓存命中

### 2.1 我们实际怎么归一化（代码 + 测试 fixture）

**Anthropic**（`crates/agent-provider-anthropic/src/stream.rs`）：

| wire 字段 | 代码行 | `ModelUsage` |
|---|---|---|
| `input_tokens` | 63–65 | `input` |
| `output_tokens` | 66–68 | `output` |
| `cache_read_input_tokens` | 69–71 | `cache_read` |
| `cache_creation_input_tokens` | 72–74 | `cache_write` |
| `output_tokens_details` | 75–80 | `details`（原样） |
| —（Anthropic 不提供） | 83–99 | `total` 恒 `None` |

即：`input` 是**裸的 `input_tokens`**，没有任何加/减。Anthropic 的 `input_tokens` 不含 `cache_read_input_tokens` / `cache_creation_input_tokens`（两者是并列字段）——所以 `ModelUsage.input` = **未命中缓存的那部分输入**。

fixture/测试证据：`stream.rs:803-823`（`message_start` 给 `input_tokens: 472`、`cache_read_input_tokens: 3`，`message_delta` 给 `output_tokens: 89`、`cache_creation_input_tokens: 7` → `input Some(472)`、`cache_read Some(3)`、`cache_write Some(7)`、`total None`）；`stream.rs:826-838`（逐字段取最后一次非空值）；`stream.rs:867-879`（`total` 恒 `None`，ADR 0007 J2）；`crates/agent-provider-anthropic/tests/http.rs:852-864`（`output_tokens_details` 原样保留，`thinking_tokens: 5`）。
映射说明也写在 `crates/agent-provider-anthropic/WIRE.md:89`（字段表）与 `:167`（映射一览）。

**OpenAI 兼容**（`crates/agent-provider-openai-compatible/src/stream.rs:241-289`）：

| wire 字段 | 代码行 | `ModelUsage` |
|---|---|---|
| `prompt_tokens` **或** `input_tokens` | 275 | `input` |
| `completion_tokens` **或** `output_tokens` | 276 | `output` |
| `total_tokens` | 277 | `total`（绝不计算） |
| `cache_read_input_tokens` **或** `prompt_tokens_details.cached_tokens`（或裸 `details.cached_tokens`） | 278–279 | `cache_read` |
| `cache_creation_input_tokens` **或** `prompt_tokens_details.cache_write` | 280–281 | `cache_write` |
| 每个 `*_details` 子对象 | 246–267 | `details`（按 wire 名保留） |

fixture/测试证据：
`stream.rs:601-631`（`usage_maps_openai_and_anthropic_field_names`）：`prompt_tokens: 11`、`completion_tokens: 4`、`total_tokens: 15`、`prompt_tokens_details.cached_tokens: 3` → **`input Some(11)`、`output Some(4)`、`total Some(15)`、`cache_read Some(3)`**。也就是说 `input`（11）里**已经包含了** `cache_read`（3），这正是 OpenAI 的语义（`cached_tokens` 是 `prompt_tokens` 的一个明细，`total = prompt + completion`）。
`stream.rs:633-668`（`the_dogfooded_usage_shape_keeps_completion_tokens_details`）用的是我们自己 dogfood 到的真实形状：`prompt_tokens: 31`、`completion_tokens: 40`、`total_tokens: 71`、`cached_tokens: 0`、`completion_tokens_details.reasoning_tokens: 30` → `input Some(31)`、`output Some(40)`、`total Some(71)`、`cache_read Some(0)`。真实端点的字段集见 `research/2026-10-12-opencode-zen-dogfooding.md:17`。

### 2.2 结论：**不一致**，而且不一致不止在"两家之间"

- Anthropic 形状：`input` = 未命中缓存的输入（`cache_read` 在它之外）。
- OpenAI 形状：`input` = 整次请求的输入（`cache_read` 在它之内）。

更麻烦的是：**同一个 OpenAI 兼容 adapter 能产出两种语义**——wire 用 `input_tokens`（275 行的第一个别名失败后落到第二个）时是 Anthropic 语义，用 `prompt_tokens` 时是 OpenAI 语义。`ModelUsage` 里**没有任何字段**能告诉消费者是哪种，类型文档（`crates/agent-core/src/model.rs:54-72`）也没写。

而且代码库内部对 `input` 的两个假设是**互相矛盾**的：

- 预算侧把它当"整次请求有多大"：`crates/agent-core/src/budget.rs:4`（"`ModelUsage::input` is the only trustworthy 'how big was this request'"），基线取自 `ModelUsageRecorded.input`（`driver.rs:957-987`）——只有 OpenAI 语义成立。
- 费用侧把它当"未命中缓存的那部分"：`timing.rs:87-90`——只有 Anthropic 语义成立。

**实践影响**（今天）：
- opencode-go 走 OpenAI 兼容协议那条路（`/zen/go/v1`，deepseek/gpt/grok/qwen…）→ **费用高估（F1）**，预算基线正确。
- Anthropic 协议那条路（`api.anthropic.com` 与 opencode-go 的 `/zen/go`）→ 费用公式正确，预算基线**低估**（如果真有缓存命中）。之所以今天还不是问题：本 adapter**没有实现** `cache_control`，radian 永远不会主动创建缓存断点（`crates/agent-provider-anthropic/WIRE.md:179`），所以 `cache_read`/`cache_write` 在实践中是 0/缺失。反过来，opencode-go 的 OpenAI 兼容路径**会**有缓存命中：那个端点要求每个会话一个稳定的 `x-opencode-session`，用途之一就是 prompt 缓存（`research/2026-10-12-opencode-zen-dogfooding.md:22`；该文档 `:72` 注明"缓存效果未验证"，但字段 `prompt_tokens_details.cached_tokens` 已在真实响应里出现过）。

---

## 3. Q2：费用公式与重复计费

### 3.1 公式（`crates/radian-tui/src/timing.rs:71-92`）

```
cost = input × price.input
     + output × price.output
     + cache_read × price.cache_read
     + cache_write × price.cache_write
```

（每项先乘后 `/= 1_000_000`，逐项向下取整，整项 `saturating_add`；四项里有任意一项"计数和单价都有"就认为可显示。footer 的调用点 `footer.rs:900-912` 把 projection 的四个会话总计传进去。）

单价来源：`catalog.rs:299-302`（`Catalog::price(provider, model)`）→ `Window.price` → `main.rs:1036` `app.set_model_price(window.price)`。

所以答案是：**是的，就是那四项**。问题在第二项的语义前提。

### 3.2 具体数字：OpenAI 兼容端点上的重复计费

取 opencode-go 的 `deepseek-v4.1-flash`，单价（2026-10-13 缓存目录与今日线上目录一致）：**$0.15 / $0.60 / cache_read $0.003**（每 Mtok）。

一次长会话请求，端点报（OpenAI 形状）：

```
prompt_tokens: 200000    (其中 prompt_tokens_details.cached_tokens: 190000)
completion_tokens: 2000
```

| | 输入项 | 输出项 | 缓存项 | 合计 | footer 显示 |
|---|---|---|---|---|---|
| **我们算的** | 200000 × 0.15 = **$0.030** | 2000 × 0.60 = $0.0012 | 190000 × 0.003 = $0.00057 | **$0.03177** | `$0.03` |
| **正确（OpenAI 口径）** | (200000−190000) × 0.15 = $0.0015 | $0.0012 | $0.00057 | **$0.00327** | `$0.0033` |

- 多算 **$0.0285**，比值 **9.7×**。
- 多算的那一项**恰好等于 `cache_read × 输入单价`**（190000 × $0.15/Mtok = $0.0285）：缓存命中的那 19 万个 token 先在 `input` 里按全价付了一次，又在 `cache_read` 里按缓存价付了第二次。缓存价越低、命中率越高，误差越大（这里缓存价是全价的 1/50）。
- 同一个数字还污染了 `tokens` 行：`↑200K ⛁190K`（`footer.rs:871-896`）看起来像"请求有 39 万 token"，其实 190K 是 200K 的一部分。

反例（Anthropic 形状，同样长度）：`input_tokens: 10000`、`cache_read_input_tokens: 190000`、`cache_creation_input_tokens: 0`、`output_tokens: 2000`，单价 $3/$15/$0.30（`anthropic/claude-sonnet-4-5`）→ 我们算 $0.03 + $0.057 + $0.03 = **$0.117**，与 Anthropic 的计费口径**一致**（输入按未命中的 1 万算、缓存读按 0.1× 算、缓存写按 1.25× 算）。**公式本身没错，错的是"input 不含缓存"这个前提在 OpenAI 那边不成立。**

### 3.3 三个小边界（不是本次的发现，但一并记录）

- 逐项整数除法：每项最多向下丢 1 微美元，合计误差 ≤ 4 微美元（$0.000004），可忽略。
- `priced` 的判定是"有计数且有价"，**不看计数是否为 0**（`timing.rs:81-86`）：如果一次响应报 `input: 0, output: 0, cache_read: 0` 而模型有价，`cost_micros` 返回 `Some(0)`，`format_cost(0)` 走 `else` 分支 → 显示 **`$0.00`**。空响应/被拒响应是可能的（见 F7）。
- `cost` 条目的阈值（`footer.rs:416`、`911`）用的是 `micros / 1_000_000`，即**整美元**，所以默认的 `70/90` 对 cost 意味着 **$70 / $90** 变色。这大概是想要的，但没写在 ADR 里。

---

## 4. Q3：reasoning token 算在 output 里吗？会漏算或重复吗？

**结论：今天的实现不漏、不重复，但这种"正确"是隐式假设，没有文档也没有测试钉住。**

- **OpenAI 兼容**：`completion_tokens` **包含** `completion_tokens_details.reasoning_tokens`。仓库内的实证：`research/2026-10-12-opencode-zen-dogfooding.md:17`（真实响应的字段集）与 `:57`（"本次实测里 `completion_tokens: 40` 中有 **30 是推理 token**，占 75%"）。归一化时 `output ← completion_tokens`（`stream.rs:276`），reasoning 明细原样进 `details`（`stream.rs:251-267`），**没有**再加一遍（`stream.rs:633-668` 的断言：`output Some(40)`，`details.completion_tokens_details.reasoning_tokens: 30`）。费用公式对 `output` 只加一次 → 与 OpenAI 的计费口径（reasoning 按 output 价）一致。
- **Anthropic**：`output ← output_tokens`（`stream.rs:66-68`），`output_tokens_details.thinking_tokens` 只原样进 `details`（`stream.rs:75-80`，测试 `stream.rs:841-854`、`tests/http.rs:852-864`），没有另行计费。Anthropic 的 `output_tokens` 含思考 token（扩展思考按 output 价计费，`thinking_tokens` 是它的明细）——这与我们的归一化相容，但**仓库里没有任何断言或文档**说明这一点，`model.rs:54-72` 的字段注释也只说"provider-reported"。
- 所以：**不重复、不漏**；风险是"如果哪天把 `thinking_tokens` 也加进 `output`（或反过来从 `output` 里减掉），费用会静默错掉"。建议在 `ModelUsage.output` 的注释里写清"含 reasoning，**不要**再叠加 `details` 里的推理计数"，并加一条契约断言。
- 顺带：`speed`（tok/s）用的也是含 reasoning 的 `output`（`timing.rs` 的 `TurnTimer`，`footer.rs:733-740`）。若 owner 想要"可见文本速率"，那是另一种口径，需要明确——ADR 0027 只说"模型自报的 output token"，所以现在是对的。

---

## 5. Q4：跨轮累计 / resume / 压缩

### 5.1 footer 显示的是**本进程内的会话累计**

`SessionProjection.usage_*` 是四个累加器（`projection.rs:182-206`），每次 `ModelResponseReceived` 就 `+=`（唯一的写入者：`crates/radian-tui/src/events.rs:156-161`），footer 的 `tokens`/`cost` 读的正是这四个总计（`footer.rs:871-912`）。所以：

- 跨轮、跨模型调用（同一轮里的多次模型请求，例如工具循环）**都累加**；
- `speed`/`elapsed` **不累加**，它们只覆盖最近一次生成（`timing.rs` 的 `TurnTimer`）——这一条 ADR 0027 已写明（"resume 后在下一轮之前为空"）；
- 没有任何地方在压缩发生时清零（对，也不应该）。

### 5.2 resume：**从零开始**，journal 里的 usage 没有被折回来

- TUI 的 projection 在 `main.rs:821` 新建（`Arc::new(radian_host::SessionProjection::new())`），resume 时只把 history 推进 app 的 transcript（`main.rs:1112-1151`：`session.history` → `app.push_message`/`prefill_history`），**没有**把 journal 里的 `ModelUsageRecorded` 折算成 usage 总计。
- 结果是：`--resume` 之后，`usage_input()` 等是 `None` → `tokens` 行显示 `-`、`cost` 条目返回 `None` 被过滤掉（不显示），直到本次运行第一个响应回来。**会话此前已经花掉的钱不显示**，而这正是 owner 最想看的那个数（pi 那一行 `$5.039` 是累计值）。
- 对照：这条"从 journal 恢复统计"的路**做过一次**，只是没接到 footer：`driver.rs:957-987` 的 `reported_baseline` 从 `ModelUsageRecorded` 恢复预算基线（`driver.rs:189-190` `restore_budget_baseline`）。journal 侧的记录是齐的（`writer.rs:111` `EVT_USAGE`；`reader.rs:87`/`:557-567`）。

### 5.3 压缩调用：**完全没进 footer**（ADR 0018 明确要求计入）

- 生成摘要的模型调用用自己的 `NoopSink` 作为事件出口（`turn.rs:454-456`），因此**不**发 `AgentEvent::ModelResponseReceived`（那个事件只在正常响应路径发：`turn.rs:1252`）。
- 这次调用的 usage 只写进 journal：`JournalRecord::CompactionUsage { revision, model, usage }`（`journal.rs:51-58`；写入点 `turn.rs:476-486`，带注释"ADR 0018 §计费: the real call is recorded immediately, commit or not"），落盘名 `CompactionUsageRecorded`（`writer.rs:112`，`critical = false`）。
- 全仓库**没有消费者**：`grep -rn CompactionUsage` 只有 `agent-core`（定义/写入/测试）、`radian-session`（reader/writer/export 三个序列化点）与 `radian-runtime/tests`。TUI 只认 `ModelResponseReceived`（`events.rs:156-161`），因此 footer 的 token 与费用**永远不含摘要调用**。
- 这与 ADR 0018 直接冲突：`docs/adr/0018-model-catalog-and-auto-compaction.md:108`（`CompactionUsageRecorded` … "真实调用，必计费"）与 `:117`（"**不论这次压缩之后是提交还是中止，会话的 usage 总计都计入它**"）。注意 `:117` 还要求它**不是** `ModelUsageRecorded`（否则预算基线会错）——修的时候别把它塞进 `ModelUsageRecorded`。
- 两个额外细节，会影响修法：① 摘要无效时允许**一次受控重试**（`turn.rs:454` 的 `for attempt in 0..2`），每次尝试都写一条 `CompactionUsage`，而 `CompactionFinished` 只发一次 —— 所以"把 usage 挂在 `CompactionFinished` 上"会丢掉第一次尝试；按调用发一个新事件才对。② 手动 `/compact` 与自动压缩走同一条路径，所以它也漏。

---

## 6. Q5：opencode-go 这类聚合 provider 的单价

**目录里 `opencode-go` 是一个独立 provider，而且它的每个模型都有 `cost`。**（不是"聚合 provider 没有单价"那种情况。）

| 数据源 | provider 数 | `opencode-go` 模型数 | 带 `cost` 的 |
|---|---|---|---|
| 本机缓存（2026-10-13，footer 实际会读的那份） | 228 | 37 | **37 / 37** |
| 线上 `models.dev/api.json`（2026-10-14 读） | 229 | 38 | **38 / 38** |
| radian 内建的 opencode-go 列表（`crates/radian-host/tests/data/opencode-go-models.json`，32 个 id） | — | — | 32 个 id 在目录里**全部存在**，且**全部带 `cost`**；无 `limit` 缺失 |

于是：

1. **单价查得到**：内建描述符的 id 就是 `opencode-go`，启动时 `catalog_provider = descriptor.id`（`main.rs:623-635`），所以查的是 `opencode-go/<model>`（`catalog.rs:299-302`），不依赖主机名规则；切换时 `budget_for` 也用路由自己的 provider（`main.rs:736-743`）。Anthropic 协议那批（`claude-*`，base `https://opencode.ai/zen/go`）同样算在 `opencode-go` 组。
2. **没有单价时确实不显示**：`Catalog::price` → `None`（`catalog.rs:299-302`）→ `Window.price = None` → `cost_micros` 第一行就 `return None`（`timing.rs:78`）→ `footer.rs:900-912` 返回 `None`，条目被 `filter_map` 掉（**不**显示 `$0.00`）。测试覆盖：`catalog.rs` 的 `the_catalog_price_row_is_parsed_in_micro_dollars`（`no-cost`/`zero`/`missing`）与 `crates/radian-tui/tests/footer.rs:558-578`（无价时断言行里没有 `$`）。
   - 什么时候会走到"没有单价"：模型不在目录里（自制/本地端点、刚上线还在 24 小时 TTL 内）、显式 `--context-window`（此时不联网，只读缓存，缓存里没有就没有）、或**免费模型**（`is_usable()` 要求 `input > 0 || output > 0`，`catalog.rs:83-86`；opencode-go 的 `longcat-2.5-preview-free`、`cat-jam-free`、`step-5-preview-free` 都是 0/0）→ 免费模型的 footer 什么都不显示，**与"单价未知"不可区分**。这是有意的（v5 §5：不编数字），但值得写进 README/ADR。
3. **一个会走错分组的入口**：用 generic 描述符 + `--base-url https://opencode.ai/zen/go/v1`（即 `--provider openai-compatible`）时，`catalog_provider` 是 `None`，于是走主机名规则 `opencode.ai → "opencode"`（`catalog.rs:36-46`）——那是**另一个** provider 组，单价不同：25 个同名模型里 **7 个**不一样（例：`deepseek-v4.1-flash` 在 `opencode` 组是 $0.30/$1.20，在 `opencode-go` 组是 $0.15/$0.60），另外 **13 个** go 独有模型（`qwen3.7-max`、`mimo-*`、`hy*`…）在 `opencode` 组根本没有条目 → 单价与窗口都拿不到。CLI 已有 `--catalog-provider`（`main.rs:149`、`623`）可以手工纠正，但没有文档指向它。内建路径不受影响。

---

## 7. Q6：发现清单（现象 / 根因 / 建议 / 能证明它的测试）

### F1（高）缓存命中的输入被重复计费

- **现象**：OpenAI 兼容端点上报缓存命中时，`cost` 把命中部分按输入全价再算一遍；长会话（缓存命中率高的场景，正好是 opencode-go 那种设计要优化的情况）高估可达约 10 倍（§3.2 的例子：$0.03 vs 正确的 $0.0033）。
- **根因**：`crates/agent-provider-openai-compatible/src/stream.rs:275`（`input ← prompt_tokens`，含 `cached_tokens`，`stream.rs:278-279` 又把它单独放进 `cache_read`）与 `crates/radian-tui/src/timing.rs:87-90`（把两者各按自己的单价相加）叠加。
- **建议**（需要 定一条，因为牵动 `ModelUsage` 的契约）：
  - **(a) 不改编排器（推荐）**：在**费用侧**扣掉重复部分，并在 `ModelUsage` 上显式标记语义，而不是在 footer 里猜——例如给 `ModelUsage` 加一个可选字段（`input_includes_cache_read: Option<bool>`，或更中性的 `uncached_input: Option<u64>`），由各 adapter 填；`cost_micros` 用 `input − cache_read`（饱和减）作为输入单价的那一项。理由：ADR 0007 J1/J3 的精神是"provider 报什么就记什么"，`input` 同时被预算侧当作"整次请求大小"（`budget.rs:4`），改掉 `input` 的含义会连带改变预算基线的行为。
  - **(b) 在 adapter 里归一**：`input = prompt_tokens − cached_tokens`（`stream.rs:275`）。这样 `input` 在两家统一为"未命中缓存"，费用公式不动；代价是 journal 里的 `input` 不再是 provider 的原始数（导出/审计看到的是我们的加工值），且预算基线会变成"未命中部分"（对 Anthropic 形状本来如此，对 OpenAI 形状是**行为改变**，需要一起评估）。
  - 无论选哪条，都要把"`ModelUsage.input` 的确切含义"写进 `crates/agent-core/src/model.rs:54-72` 的注释与 `crates/agent-provider-contract/README.md`（这是 F6）。
- **测试思路**：在 `crates/agent-provider-openai-compatible/src/stream.rs` 的既有测试里加一条，把 OpenAI 形状的 usage JSON 经 `assemble(&[chunk(...)], false)` 走完整链路（已有 helper），断言归一化后的 `input`（或新字段）表示**未命中**输入；再在 `crates/radian-tui/tests/footer.rs` 加一条对应的成本断言：价 `input 3_000_000 / cache_read 300_000`（`test` 里已有的 `ModelPrice` 构造方式，`:571-576`）、usage `input 1_000_000 / cache_read 900_000`、`output 0`，期望 `$0.30 + $0.27 = $0.57`（而不是今天的 `$3.00 + $0.27 = $3.27`）。这一条同时钉住"缓存部分只按缓存价算一次"。

### F2（中高）压缩调用的 token/费用不计入

- **现象**：自动压缩（预算触发、溢出兜底、手动 `/compact`）的摘要调用花费在 footer 里完全不显示；压缩越多，偏差越大（dogfood9 那种一次会话压 11 次的情况）。
- **根因**：`turn.rs:454-456`（`NoopSink`）→ `turn.rs:1252` 的 `ModelResponseReceived` 只覆盖正常响应；usage 只落 `CompactionUsageRecorded`（`journal.rs:51-58`、`turn.rs:476-486`），没有事件、没有 projection 写入、也没有 journal 折叠（`events.rs:156-161` 是唯一写入者）。ADR 0018 要求计入（`docs/adr/0018-…:108`、`:117`）。
- **建议**：在 `agent-core` 加一个 usage 事件（例如 `AgentEvent::CompactionUsage { revision, model, usage }`，与 journal 记录同时发；**不要**复用 `ModelResponseReceived`，那会让 TUI 的 `TurnTimer` 把摘要响应当成本轮响应），host 侧在 `events.rs` 的分支里 `projection.add_usage(usage)`。不要塞进 `ModelUsageRecorded`（会污染预算基线，ADR 0018 `:117` 明说）。
- **测试思路**：① 在 `crates/agent-core/tests/usage.rs` 加一条（那里已有 `usage_lands_in_the_model_response_received_event` 的模式，`:102`）断言一次带 usage 的压缩会发出新事件，且**两次尝试**各发一次；② 在 `crates/radian-runtime/tests/compact_control.rs`（已经有"真实摘要器会上报 usage"的 `:24`）断言压缩之后 projection 的 usage 总计增加了摘要调用的量——今天这条会失败（值为 `None`）。

### F3（中）resume 后 footer 的 token/费用归零

- **现象**：`--resume` 后 `tokens` 显示 `- - -`、`cost` 隐藏，之前几轮的消耗全部消失；与 `speed`/`elapsed` 的"resume 后为空"不同，这一条 ADR 里没有说明，看起来像 bug 而不是设计。
- **根因**：`main.rs:821`（projection 是空的）+ `events.rs:156-161`（唯一写入者是 live 事件）+ resume 只推 transcript（`main.rs:1112-1151`）；对照 `driver.rs:957-987` 证明"从 journal 折出统计"这件事已有先例（那条是给预算基线用的）。
- **建议**：加一个纯函数（放在 `radian-host`，与 `reported_baseline` 同一族）`usage_from_journal(&Session) -> ModelUsage`，把 `ModelUsageRecorded` **与** `CompactionUsageRecorded` 都累加（压缩也是花钱的，见 F2），`open_session` 里在建立 projection 之后调用一次；`TranscriptReplaced` 不清零（会话总计不是 transcript 属性）。要注意"未提交的替换段"里 `CompactionUsage` 是 critical=false 但与压缩无关地独立存在（ADR 0018 `:117`），所以按"读到的所有记录求和"即可。
- **测试思路**：在 `crates/radian-host/tests/` 里构造一个含 `ModelUsageRecorded` + `CompactionUsageRecorded` 的会话文件（`radian-session` 已有写 journal 的测试 helper 与固定 fixture 的做法），`open_session` 之后断言 `projection.usage_input()/usage_output()` 等于两条记录之和；今天会得到 `None`。

### F4（中）`/model` 切换后单价不更新

- **现象**：切换模型之后 footer 继续用**启动时那个模型**的单价去乘**跨模型混合**的总计；从贵模型切到便宜模型（或反之）时费用可以差一个数量级。
- **根因**：`app.set_model_price` 全仓库只有 `main.rs:1036` 一个调用点（启动）；切换路径只重算预算（`main.rs:736-743` 的 `budget_for` 只返回 `.budget`，尽管它调用的 `resolve_offline` 返回的 `Window` 里就带着 `price`，`catalog.rs:603-625`）。
- **建议**：把 `budget_for` 换成 `route_facts_for(provider, model) -> (budget, price)`（或再加一个 `price_for` 回调），切换应用时同时 `app.set_model_price(...)`。更彻底的做法是让费用**按调用**记账（每个 `ModelUsage` 记下当时的单价），因为"会话总计 × 单一单价"在跨模型会话里本质上就是错的——这需要 决定要不要做（`/model` 切换是 ADR 0026 D5 的常用功能）。
- **测试思路**：给切换路径加一条单测，断言 `resolve_offline` 得到的 `price` 与 `budget` 来自同一个 `provider/model`（现在的 `budget_for` 无法测，因为它丢掉了一半）；TUI 侧加一条：`app` 切换后渲染 `cost`，断言用的是新单价（今天会用旧的）。

### F5（低-中）分档定价被忽略

- **现象**：目录里带 `cost.tiers` 的模型，阈值以上的请求单价被低估。opencode-go 内建 32 个里 **7 个**带 tiers：`grok-4.7`、`grok-4.6`、`gpt-5.6-luna`、`gpt-6-luna`、`claude-haiku-5-5`、`minimax-m3`、`qwen3.7-plus`。例：`claude-haiku-5-5`（缓存版目录）平坦价 input 0.1 / output 0.5，`tiers: [{input: 0.5, output: 2.5, tier: {type: "context", size: 100000}}]` → 上下文超过 100K 时低估到 **1/5**。
- **根因**：`catalog.rs:89-104` 的 `parse_price` 只读 `input`/`output`/`cache_read`/`cache_write` 四个平坦键；`ModelPrice`（`catalog.rs:68-86`）也没有承载分档的结构。`tiers` 与旧的 `context_over_200k` 是同一件事的两种写法（两个字段在目录里同时出现），`input_audio` 也忽略（radian 没有音频输入）。
- **建议**：先核实 models.dev 的 schema（本文只能从数据推断"平坦值是阈值以下的价"），再由 决定：要么实现分档（阈值比的是上下文大小，footer 恰好能从 `session.budget().projected_tokens` 拿到投影），要么在 ADR 0027 / README 里写明"只按基础档计价，阈值以上会低估"，并让 `ModelPrice` 至少解析出 `tiers` 以便将来使用。
- **测试思路**：在 `crates/radian-host/tests/catalog.rs` 加一条：给定带 `tiers` 的 fixture，断言 `price` 在"投影 ≤ 阈值"与"> 阈值"两种情形下给出不同的费率（今天两者相同，测试会失败）。

### F6（中，F1 的根因）`input` 的语义没有契约

- **现象**：同一类型在两家、甚至同一 adapter 的两种别名下含义不同（§2.2），而文档与类型注释都没写；预算侧与费用侧的假设互相矛盾（`budget.rs:4` vs `timing.rs:87-90`）。
- **根因**：ADR 0007 J1 只规定了字段名与"没报就是 `None`"，没有规定**字段的包含关系**；`crates/agent-core/src/model.rs:54-72` 的注释逐字照抄了"provider-reported"，没有说清 `input`/`output` 里含什么。
- **建议**：把语义钉进 `crates/agent-core/src/model.rs:54-72` 的字段注释（`input` 是否含 `cache_read`、`output` 是否含 reasoning）与 `crates/agent-provider-contract/README.md`（那里已经在管 `UsageField`），并在 adapter 的契约测试里断言。
- **测试思路**：契约测试（`crates/agent-provider-*/tests/contract.rs` 的 `UsageField` 风格）加一条关于"`input` 与 `cache_read` 的包含关系"的声明，两家都必须显式声明并被测试钉住。

### F7（低）全零计数会显示 `$0.00`

- **现象**：价格可用时，报 `input: 0, output: 0, cache_read: 0` 的响应（空响应/被拒响应）会渲染 `$0.00`，与"拿不到数字就不显示、绝不 `$0.00`"（ADR 0027、v5 §5）不一致。
- **根因**：`timing.rs:81-91` 的 `priced` 只看"计数与单价是否都存在"，不看计数是否非零；`format_cost`（`:140-146`）对 0 走 `else` 分支。
- **建议**：`priced` 改为"至少一项计数 > 0"，或让 `format_cost`/条目把 0 当作"没有可显示的数"。
- **测试思路**：在 `crates/radian-tui/tests/footer.rs` 加一条：价可用 + 三个计数全 0 → 断言行里没有 `$`（今天会得到 `$0.00`）。

### F8（低）`cache_write` 从不显示

- **现象**：`tokens` 行的 `{cache}` 只显示 cache **read**（`footer.rs:871-896`，占位符只有 `in`/`out`/`cache`，`:333`），而 `cache_write` 是**按更高单价计费**的（Anthropic 1.25×、opencode-go 的 `claude-haiku-5-5` 也是 0.125 vs 0.1）。费用里含它，显示里没有它，对账时会困惑。
- **建议**：要么加 `{cache_write}`（或一个 `{cache}` 格式化成 `R33K/W12K`），要么在文档里写明 `{cache}` 只指 cache read。
- **测试思路**：`footer.rs` 加一条 `tokens` 条目在 `cache_write` 有值时的渲染断言。

---

## 8. 已核对且正确（未发现问题的部分）

1. **Anthropic 的映射**：`input ← input_tokens`、`output ← output_tokens`、`cache_read ← cache_read_input_tokens`、`cache_write ← cache_creation_input_tokens`、`details ← output_tokens_details`（原样）、`total` 恒 `None`（Anthropic 确实不提供 `total_tokens`，ADR 0007 J2）。代码 `stream.rs:61-99`；测试 `stream.rs:803-879`、`tests/http.rs:852-864`；文档 `crates/agent-provider-anthropic/WIRE.md:89`、`:167`。
2. **OpenAI 兼容的别名与 details**：`prompt_tokens`/`input_tokens`、`completion_tokens`/`output_tokens`、`cache_read_input_tokens`/`cached_tokens`、`cache_creation_input_tokens`/`cache_write` 的别名回退；每个 `*_details` 子对象按 wire 名完整保留（dogfood 修掉的那个 reasoning token 丢失问题确实修好了）。代码 `stream.rs:241-289`；测试 `stream.rs:563-690`（含真实形状的 `:633-668`）。
3. **`total` 绝不由 `input + output` 伪造**（`stream.rs:277`、`stream.rs:670-690`；`journal`/`ModelUsage` 的注释 `model.rs:59-61`）。
4. **费用公式的形状**：四项各自单价、先乘后除、饱和加法（`timing.rs:71-92`）；无价或无计数就隐藏而不是显示 `$0.00`（`timing.rs:78`、`footer.rs:900-912`，测试 `footer.rs:558-578`、`timing.rs` 的 `the_cost_uses_the_catalog_price`）。
5. **单价解析的容错**：models.dev 的美元/百万 token → 微美元整数（`round`，避免 float 破坏 `Eq`）；缺字段/负值/零价 → `None`；`is_usable` 要求输入或输出有一个非零（`catalog.rs:68-104`；测试 `the_catalog_price_row_is_parsed_in_micro_dollars`）。
6. **数字格式**：`format_tokens` 的 `760` / `19K` / `1.2M`；`format_cost` 对小于一分钱显示 `$0.0030`（不吞成 `$0.00`）；`format_duration` 的三档（`timing.rs:95-146`，测试 `the_numbers_format_short`）。
7. **`cache` 未上报时显示 `-`**，不伪造 0（`footer.rs:886-892`，测试 `the_cache_tokens_are_shown`）。
8. **context 百分比与预算**：与本任务无关，但一并确认它不读 usage 总计、而是读 Core 的 `BudgetReport`（`footer.rs:"context"` 分支），所以 F1 不会污染 context 行。
9. **opencode-go 有单价**（§6）：内建 32 个模型在目录里全部存在且全部带 `cost`；查的是 `opencode-go` 组而不是主机名推出的组。
10. **`tokens`/`cost` 只从投影读、绘制时不做 I/O**（ADR 0023 U2/U3、ADR 0027 C2 的要求满足）。

---

## 9. 未验证 / 本文没有做的事

- **没有真实 API 调用**（无 key）。所以"Anthropic 的 `input_tokens` 不含缓存"、"OpenAI 的 `prompt_tokens` 含 `cached_tokens`"这两条 API 语义来自公开文档与通行实践，**仓库内能证明的只是我们自己的归一化形状**（fixture 里 `input` 与 `cache_read` 同时非零、`total = prompt + completion`）。建议在真实端点上各留一条带缓存命中的 fixture（尤其 opencode-go，它的会话头就是为 prompt 缓存准备的）把这两条钉死。
- **没有核实 models.dev 的 schema**（只看了数据）。F5 里"平坦值是阈值以下的价"是从 `tiers` + `context_over_200k` 并存推断的。
- **没有跑真实会话**（本机 `~/.radian` 下没有 sessions 目录，没有 journal 可对账）；F3/F2 的结论来自代码路径，不是实测数字。
- **只看了 TUI 这条路**：`agent-cli` 不显示费用/总量（`grep` 里没有第二个 `cost_micros` 调用点），所以上述问题只影响 TUI 的 footer。
- **没有改任何代码或测试，没有 git commit**；本文是本次调研唯一的产出。

---

## 10. 附：本文用到的目录数据与复现命令

```sh
# 本机缓存（footer 实际读到的那份；只读）
python3 - <<'EOF'
import json,os
d=json.load(open(os.path.expanduser("~/.radian/cache/catalog/models.json")))
go=d["opencode-go"]["models"]
print(len(go), sum(1 for v in go.values() if "cost" in v))
print(go["deepseek-v4.1-flash"]["cost"])          # {'input': 0.15, 'output': 0.6, 'cache_read': 0.003}
print(go["claude-haiku-5-5"]["cost"])               # 平坦 0.1/0.5 + tiers(>100k → 0.5/2.5)
EOF

# 线上目录（无 key、只读 GET；不写盘，直接管道给 python）
curl -sS https://models.dev/api.json | python3 -c '
import json,sys; d=json.load(sys.stdin)
go=d["opencode-go"]["models"]; print(len(go), sum(1 for v in go.values() if "cost" in v))'

# 既有测试（未修改任何测试）
cargo test -p agent-provider-openai-compatible usage
cargo test -p radian-tui --test footer
```

关键单价（2026-10-13 缓存目录 = 2026-10-14 线上目录，用于 §3.2 的例子）：

| provider / model | input | output | cache_read | cache_write |
|---|---|---|---|---|
| `opencode-go` / `deepseek-v4.1-flash` | 0.15 | 0.6 | 0.003 | — |
| `opencode-go` / `claude-haiku-5-5`（≤100K 档） | 0.1 | 0.5 | 0.01 | 0.125 |
| `opencode` / `deepseek-v4.1-flash`（**另一个组**，见 §6 第 3 条） | 0.3 | 1.2 | 0.006 | — |
| `anthropic` / `claude-sonnet-4-5` | 3 | 15 | 0.3 | 3.75 |

（单位：美元 / 百万 token；`catalog.rs` 里存成微美元/百万 token，即 ×1e6。）
