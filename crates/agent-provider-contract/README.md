# Provider 契约测试（Phase 0a 完成标准）

## 契约是什么

`crates/agent-provider-contract` 钉住**任何 `Model` 实现都必须满足的语义**，与具体 provider 的线上格式解耦：

- 一条 **`Wire`**：语义化的响应（纯文本、分片文本、工具调用、分片参数、finish reason、usage 三形态、错误、挂起……），不含任何一家的字节。
- 一个 **`ContractHarness<M>`** trait：由每个 adapter 实现，负责把 `Wire` 翻译成自己的线格式（OpenAI 的 SSE chunk、以后 Anthropic 的事件流），并持有自己的 mock 与凭据。
- 一组**检查函数**：`check_plain_text` … `check_secret_not_leaked`，全部泛型于 `M: Model`，直接驱动 `Model::complete`（不经过 turn loop——契约管 adapter，管 loop 是 `agent-core` 自己测试的事）。

现有 OpenAI 兼容适配器的接入见 `agent-provider-openai-compatible/tests/contract.rs`：约 100 行 harness + 12 个 `#[tokio::test]`，每个委托给契约 crate 的一条检查。**契约里没有 OpenAI 的字节**；`tests/http.rs` 里那些 SSE 分片、header、超时、错误体细节继续留在 adapter 自己的测试里。

## 两份测试的边界（哪些 http.rs 用例被契约涵盖，哪些是专有）

契约涵盖（语义等价，经 harness 重新走一遍 mock）：

- 纯文本响应/流式 delta 按序发出 → `a_turn_sends_the_expected_request_and_returns_the_text`、`a_response_split_into_tiny_pieces_is_read_the_same` 的**语义部分**；
- 单/多工具调用与参数分片装配 → `a_tool_call_round_trips_through_the_core` 与 `a_normal_split` 类用例的语义部分（http.rs 里没有专门的参数分片用例；契约补上了这一条）；
- call id 重复/缺失的去重 → `a_server_that_reuses_tool_call_ids_does_not_break_the_conversation` 的语义部分；
- finish reason 映射、usage 三形态、错误分类（500/400/断开）、取消、凭据脱敏 → 与 `http_errors_are_classified...`、`a_cut_connection...`、`an_http_error_body_that_echoes_the_key_is_masked` 等**并存的另一条路径**，契约跑在直接 `complete` 上，http.rs 跑在 turn loop 上。

OpenAI 专有（只属于 http.rs，契约不管）：请求体/header 的形状（`stream_options`、`tools` 键、authorization 头）、SSE 字节级分片读法、idle timeout、URL 凭据脱敏、redirect 不跟随、`[DONE]` 推断等线格式与传输细节。

## Phase 3 接 Anthropic adapter 时要做的事

1. 实现 `ContractHarness<AnthropicModel>`：一个 `script(wires)` 把每条 `Wire` 译成 Anthropic 的事件流字节（`message_delta` 的 `stop_reason`、`usage` 事件、拒绝文本……），用进程内 mock 并发给 adapter。
2. `secret()` 返回自己配进 adapter 的 key。
3. 在 Anthropic crate 里写一个和 `tests/contract.rs` **几乎逐字相同**的测试文件（12 个 `#[tokio::test]`，每个委托给契约 crate 的同名检查）。
4. 契约通过后，再写 Anthropic **专有的**测试（它的 event 序列、工具块格式、慢/切断行为）——那份测试不在契约里。

## 本切片顺带改的 adapter

契约条目 6 要求 `StopReason::PauseTurn` 有 fixture，而 OpenAI 适配器此前无法产生它：`stream.rs` 的 finish 映射新增 `"pause_turn" => StopReason::PauseTurn`（单位测试的映射表同步加一行）。这是契约驱动适配器补全行为的实例；除此之外适配器行为未动。

## 评审补充：`pause_turn` 这处改动的判断，以及契约的一个设计缺口（2026-10-12）

评审者核实并接受这处改动，但要把理由和边界记清楚：

- **`pause_turn` 不是 OpenAI 的 finish_reason**，是 harness 为 `Reason::PauseTurn` 选的线上形式。所以严格说，这一行是「契约为了凑满 `StopReason` 的变体，让 adapter 支持了一个该 provider 并不发送的值」。
- **判断：可以接受，风险有界。** 语义上是对的——兼容端点代理 Anthropic 模型时确实可能透传 `pause_turn`，而 Core 对 `PauseTurn` 的处理是 `Step::Continue`（`turn.rs:606`），正是「turn 被暂停、继续它」的本意；而且每个 step 开头都会检查 `max_model_calls`，所以不会无界循环。改动前它落到 `Other("pause_turn")`，Core 会把本轮判为 `Incomplete(UnknownStopReason)`——对一个真的发了 `pause_turn` 的端点，新行为更正确。
- **但契约有一个设计缺口要在 Phase 3 之前补**：`check_finish_reasons` 要求每个 adapter 对**每个** `Reason` 都有线上形式，没有「我这家 provider 没有这个概念」的出口。这次是让 adapter 去迎合契约，下次可能逼着某个 adapter 伪造 provider 行为——那会产出错误的 adapter。**建议**：给 `ContractHarness` 加一个显式声明不支持的入口（例如 `fn unsupported() -> &'static [Reason] { &[] }`），`check_finish_reasons` 跳过被声明的项但**要求显式声明**，不允许静默跳过。接 Anthropic 时一并做。

## 评审补充：契约比较 `details` 的方式（2026-10-12）

实测暴露的 `completion_tokens_details` 缺口修好后，契约的 usage 期望值里出现了嵌套的 `details`。因为 `OpaqueJson` 的相等是**字节级**的，而适配器把 provider 的 chunk 解析成 `serde_json::Value` 时会丢掉原始键序（`Value` 的 map 是 `BTreeMap`，ADR 0007 的 J3 已把这一点记为可接受），原先的写法只能把期望值**按某个适配器的解析器恰好产出的顺序**写出来——那等于把一个实现细节编进了"与 adapter 无关"的契约里。

评审时改成：契约用 `assert_usage_eq` 比较，**`details` 按 JSON 内容比，其余字段仍逐字段比**。`OpaqueJson` 的字节级相等仍在 `agent-core` 侧的往返测试里起作用（那里的字节确实应当保真）；契约关心的是子对象的内容与嵌套结构。两家 adapter 的契约套件在改动后仍各自全绿。

## 变异保证

10 条契约每条都有一个针对性变异（在一次性检出里破坏 adapter 对应行为），全部被对应契约测试抓住，见 `crates/agent-provider-contract/MUTATIONS-CONTRACT.md`。抓不住错误的契约没有价值——这条保证是套件本身的一部分。

## 两个出口：provider 没有这个概念时怎么办（2026-10-12）

契约描述的是**语义**，不是某家的字节。当某个 provider 确实**没有**契约假定的某个概念时，统一做法是：

> **在 `ContractHarness` 里显式声明，然后要么按「缺席」断言，要么按「不适用」跳过——绝不允许静默跳过，也绝不允许为了让契约通过而伪造 provider 行为。**

只有**显式声明**能改变检查；默认声明是「全都支持/全都报告」。目前有四个出口，前两个是接 Anthropic adapter 时暴露出来的真实缺口，第三个是 ADR 0018 的「可识别超长错误」加的，第四个是 ADR 0007 的「`input` 含不含缓存命中」加的：

### 出口 1：finish reason（跳过被声明的项）

`fn unsupported_reasons() -> &'static [Reason] { &[] }`。`check_finish_reasons` 跳过被声明的项；未声明的必须满足。`reasons_to_check(h)` 暴露筛选结果，便于测试。

**实例**：Anthropic 的官方 `stop_reason` 共 7 个值（`end_turn`/`max_tokens`/`stop_sequence`/`tool_use`/`pause_turn`/`refusal`/`model_context_window_exceeded`），**没有** `content_filter`（它用 `refusal`，由契约的 Refusal 子检查覆盖）。所以 Anthropic harness 声明 `Reason::ContentFiltered`。

### 出口 2：usage 字段（**断言缺席**，不是跳过）

`fn unreported_usage_fields() -> &'static [UsageField] { &[] }`（`UsageField` = `Input`/`Output`/`Total`/`CacheRead`/`CacheWrite`/`Details`）。`check_usage_shapes` 对未声明的字段照旧断言等于 provider 报的值；对声明的字段**断言它是 `None`**——在「provider 报了完整 usage」的场景里也断言 `None`。这是**更严**的检查，不是更松：声明了一个 provider 不报的字段后，adapter 若把它编出来就会失败（ADR 0007 J2：provider 没报的数字永不得编）。`expected_usage(h, scripted)` 给出该断言用的期望值；`check_total_not_computed` 对所有 adapter 不变，不受出口影响。

**实例**：Anthropic 官方 usage 的完整字段集合是 `input_tokens`、`output_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens`、`cache_creation`、`output_tokens_details`、`server_tool_use`、`service_tier`、`inference_geo`（`https://platform.claude.com/docs/en/api/messages`，访问 2026-10-12），**没有** `total_tokens`。所以 Anthropic harness 声明 `UsageField::Total`，adapter 的 `ModelUsage.total` 必须永远为 `None`。

### 出口 3：可识别的超长错误（ADR 0018，**反例对所有 provider 都跑**）

`fn context_overflow(&self) -> Option<Wire> { None }`。provider 真实可识别的「输入超出上下文窗口」错误（状态码 + 正文）；`None` 是「这家 provider 没有可识别的超长错误」，不是静默跳过。`check_context_overflow` 分两部分：

1. **无条件**（与声明无关）：`HTTP 400 "bad request"`、`HTTP 429 "too many tokens"`、`HTTP 500 "context_length_exceeded"` 都**不是** `ModelError::ContextOverflow`。429/5xx 永远不是超长——即使正文里有 `context_length_exceeded`（那可能是 TPM 限流）。
2. 声明了 `Some(wire)` 的 harness：该 wire 必须变成 `ContextOverflow`，错误消息引用 provider 正文，且不含 harness 的 secret。声明了却分类不出来，或没声明却被分类成超长，都失败。

契约 crate 自己的 `tests` 用一个假 provider 钉住三件事：声明即分类、无声明只跳过正例（反例照跑）、把普通 400 误判成超长会在**没有声明**的 harness 上失败——这条专门防「默认 `None` 就把反例也省了」的变异。两家的 `context_overflow` 都返回真实的 400 形态（OpenAI 的 `context_length_exceeded`；Anthropic 的 `prompt is too long`），对应测试 `contract_11_context_overflow`。

### 出口 4：`input` 是否已含缓存命中（**声明即断言**，ADR 0007）

`fn input_includes_cache(&self) -> bool { false }`——provider 报的输入计数**是否已含**缓存命中：OpenAI 形状（`prompt_tokens` 含 `prompt_tokens_details.cached_tokens`）为 `true`，Anthropic 形状（`input_tokens` 是不含缓存的那部分，与 `cache_read_input_tokens` 并列）为 `false`。`check_input_cache_semantics` 对每一条 adapter 报出来的 usage 断言 `ModelUsage.input_includes_cache == Some(声明值)`。

这条**没有「未设」**：adapter 留下的 `None` 直接失败。不说就是重复计费的来源——同样的两个数，预算侧当成「整次请求有多大」，费用侧当成「未命中缓存的那部分」，缓存命中就被按全价再算一遍（ADR 0007 的 F6 根因）。默认声明是 `false`：第三家若也没有缓存概念，`false` 正是它的真实形状（它数出来的输入全是未命中的），不需要新出口；若它像 OpenAI 一样把命中包在输入里，必须**显式**声明 `true`——默认值故意取 `false`，免得新 harness 意外继承 OpenAI 的形状。`expected_usage` 把声明值放进期望里，所以 `check_usage_shapes` 也顺带钉住它。

**实例**：OpenAI 兼容 harness 声明 `true`（`total_tokens = prompt_tokens + completion_tokens`，`cached_tokens` 是 `prompt_tokens_details` 的明细）；Anthropic harness 声明 `false`。两家各自的 `contract_7c_input_cache_semantics`。

## Anthropic adapter 已接入

`crates/agent-provider-anthropic`（API key 认证，无 OAuth）的 14 个契约测试全部委托给同一套检查，现已全绿（含 `contract_11_context_overflow` 与 `contract_7c_input_cache_semantics`）。线格式依据（官方 URL + 访问日期）、`stop_reason` 取值集合与 usage 字段全集见 `crates/agent-provider-anthropic/WIRE.md`。

## 接第三家时预计会暴露的地方（2026-10-12 评估，非结论）

第二个实现（Anthropic）暴露出两处泄漏。以下是在接第三家（Gemini/Cohere/Bedrock 等）前、按现有契约内容**预计**还会撞上的地方——写在这里是为了下次不用重新发现，不是为了预先放宽契约：

1. **错误文本格式与错误分类是规定的**。`check_error_classification` 要求错误信息包含 `HTTP {status}` 并引用响应体，且 500/429/408 → `Unavailable`（ADR 0017 起；此前是 `Transport`）、其余 4xx → `Protocol`，且 500 必须 `is_transient()`、400 必须不是。若第三家的错误不是 HTTP（gRPC/仅 SSE error 事件）或错误体需要解析出别的码，adapter 会被迫拼出 `HTTP …` 文本。可能需第三个出口：声明“错误不以 HTTP 表达”或让检查只断言分类、不断言文本。
2. **`Reason` 枚举本身就是 OpenAI 与 Anthropic 的混合**（`ContentFiltered` 来自 OpenAI，`PauseTurn` 来自 Anthropic）。Gemini 的 `SAFETY`/`RECITATION`/`MALFORMED_FUNCTION_CALL`、Cohere 的 `STOP_SEQUENCE`/`ERROR` 都会要求加变体。考虑改成「每家声明的 provider 原值 → 中性分类」映射，而不是继续扩枚举。
3. **`Refusal` 假定拒绝文本在 content 里**（`check_finish_reasons` 断言 `joined_text == "I can't help."`）。Anthropic 的拒绝还带 `stop_details`（本 adapter 未解析）；若第三家的安全拒绝**无文本**或只有结构化原因，这条会硬失败。
4. **usage 的 `Wire` 变体是 OpenAI 的块形状**（`UsageTrailing` = “无 choices 的尾块”）。Anthropic 没有这种块，靠 harness 映射到 `message_delta`；第三家（如 Gemini 的每块 `usageMetadata`）可能需要新变体或更抽象的“usage 在末尾/每块”描述。
5. **`check_plain_text` 断言内容恰为单个文本 part**（`vec![ContentPart::text("hello")]`）。若第三家把推理/思考作为 `ContentPart` 暴露（而不是像本 adapter 那样忽略），或多文本块，这条会失败。
6. **取消必须是 `Transport` 且消息含 `cancelled`**（`check_cancellation`）。自然返回 `Protocol` 或不同文本的 adapter 会被迫迎合。
7. ~~**usage 出口可被滥用成真空**~~ **已堵（2026-10-12，评审者合并时加）**：`check_usage_shapes` 现在先断言「不是所有字段都被声明为 unreported」，否则直接失败并说明"provider 真的完全不报 usage 是需要决定的事，不是 harness 能声明出来的"。Anthropic 侧有 `declaring_every_usage_field_unreported_is_refused`（`#[should_panic]`）钉住它；两家 adapter 的契约测试均仍全绿。更强的做法（把声明改成"字段 → provider 线上名字"的正向映射，声明即承诺能读到值）仍可考虑，**未做**。
## 网络异常现状矩阵（Phase 7 / resilience，2026-10-16）

**只记录现状，不改策略。** 由 `crates/agent-provider-openai-compatible/tests/resilience.rs`、
`crates/agent-provider-anthropic/tests/resilience.rs`（各 13 条）与
`crates/radian-runtime/tests/provider_failure.rs` 钉住。所有测试走 adapter 的公开
`Model::complete`（runtime 那层再走 `Driver`）；mock HTTP server 是两家的 `testing` 模块。
超时类测试只断言"会失败"和错误类型，不断言耗时。

| # | 场景 | OpenAI-compatible | Anthropic | 备注 |
|---|---|---|---|---|
| 1 | 400 / 401 / 403 / 404 | `Protocol("HTTP {code}: {body}")` | 同 | 错误体最多引用 **2048 字节**（超出截断），空体 → `(empty body)` |
| 1 | 408 / 429 / 5xx | `Unavailable { message, retry_after }`（ADR 0017 的可重试分类） | 同 | 归类：5xx、408、429 属 provider；其余 4xx 属请求。`retry_after` 取 `Retry-After`（秒或 IMF-fixdate），缺省 `None` |
| 1 | key / header 值 | 不出现在错误里（`<redacted>`） | 同 | HTTP body 与 SSE error 两条路径都 mask |
| 1 | 400 且 `include_usage` | 附一句 `set include_usage to Some(false)` | 无此提示 | 本地 mock 下 `include_usage` 默认 false，故矩阵里两家一致 |
| 2 | 首个 delta 前断开 | `Transport`（reqwest 传输错误） | 同 | 无部分结果 |
| 2 | 若干 delta 后连接重置 | `Transport`；partial 只进 live 事件，`complete` 返回 `Err` | 同 | **部分文本不会被当作完整回答** |
| 2 | 若干 delta 后提前 EOF（正常收尾、无完成标记） | `Transport("...before the response was complete")` | 同 | OpenAI 缺 `finish_reason`/`[DONE]`，Anthropic 缺 `message_stop` |
| 2 | 与 ADR 0003 中断前缀 | 一致：partial 只是 live delta；core 仅在**取消**时把它落成 `interrupted` assistant 前缀，错误时不落 | 同 | 见下方"落盘" |
| 3 | idle timeout（中途静默） | `Transport("the server sent nothing for …")` | 同 | 只断言会失败；测试用小 `idle_timeout`（50 ms），不断言耗时 |
| 4 | 请求途中取消 | 立即返回 `Transport("the request was cancelled")`，不挂起 | 同 | mock 侧连接随 future drop 关闭（未单独断言） |
| 5 | 非 JSON `data:` 行 | `Protocol("unreadable stream chunk")` | `Protocol("unreadable stream event")` | 都是错误，不跳过 |
| 5 | 未知事件 / 未知字段 | 忽略（未知字段照常解析） | 忽略（未知 `type` 忽略） | 与版本策略一致 |
| 5 | 缺字段 | 容忍（缺 `choices` 的块被跳过） | 容忍（缺 `content_block` 视为 ignored） | 不报错 |
| 5 | provider 的 error 事件 | `Transport("the provider reported an error: …")` | 同 | 文本同样 mask |
| 6 | 失败一轮的会话文件 | 有 `InputClaimed`/`TurnStarted`/`MessageAppended(user)`/`InputAdmitted`，**没有 `TurnFinished`**，没有 assistant 消息 | 同 | 见下方 |

### 落盘（item 6）

模型错误（HTTP 5xx、或中途断开）让驱动以 `RunError::Model` 结束：`run_turn` 在
`match result?` 处提前返回，**跳过了 `TurnFinished`**，驱动随之停止、宿主报错。会话文件因此有
用户消息与 `TurnStarted`，却没有终态记录；resume 时 `Session::history()` = 仅那条 user 消息
（一致，不重放）。

**标为有问题的格子：失败的一轮没有 `TurnFinished`。** 读者无法从会话文件区分"这一轮失败了"
与"进程在写终态前崩了"；`TurnFinished { outcome: "failed" }` 本可以表达它。稳定复现是
`crates/radian-runtime/tests/provider_failure.rs` 里 `#[ignore]` 的
`a_failed_turn_should_still_write_a_turn_finished`（当前会失败）。要不要让驱动把 `RunError`
也落成一条终态事件，是 owner 的决定，本切片不改。

其余格子（错误体 2048 字节上限、不解析结构化错误码、取消不单独断言 mock 侧连接关闭）是
**记录**，不视为问题。要不要把错误码结构化解出来，同样是 owner 的决定。

**重试（ADR 0017，2026-10-15）**：矩阵里标为 provider 的错误（408 / 429 / 5xx）
现在是 `Unavailable`，与断流、空闲超时、连接失败同属 `is_transient()`；取消仍是
`Transport` 但**不** transient。重试发生在 core 的一次模型调用内部，策略由宿主的
`[model] retries` 给出，见 `crates/agent-core/tests/retry.rs`。
