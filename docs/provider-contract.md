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

## 变异保证

10 条契约每条都有一个针对性变异（在一次性检出里破坏 adapter 对应行为），全部被对应契约测试抓住，见 `crates/agent-provider-contract/MUTATIONS-CONTRACT.md`。抓不住错误的契约没有价值——这条保证是套件本身的一部分。

## 两个出口：provider 没有这个概念时怎么办（2026-10-12）

契约描述的是**语义**，不是某家的字节。当某个 provider 确实**没有**契约假定的某个概念时，统一做法是：

> **在 `ContractHarness` 里显式声明，然后要么按「缺席」断言，要么按「不适用」跳过——绝不允许静默跳过，也绝不允许为了让契约通过而伪造 provider 行为。**

只有**显式声明**能改变检查；默认声明是「全都支持/全都报告」。目前有两个出口，都是接 Anthropic adapter 时暴露出来的真实缺口：

### 出口 1：finish reason（跳过被声明的项）

`fn unsupported_reasons() -> &'static [Reason] { &[] }`。`check_finish_reasons` 跳过被声明的项；未声明的必须满足。`reasons_to_check(h)` 暴露筛选结果，便于测试。

**实例**：Anthropic 的官方 `stop_reason` 共 7 个值（`end_turn`/`max_tokens`/`stop_sequence`/`tool_use`/`pause_turn`/`refusal`/`model_context_window_exceeded`），**没有** `content_filter`（它用 `refusal`，由契约的 Refusal 子检查覆盖）。所以 Anthropic harness 声明 `Reason::ContentFiltered`。

### 出口 2：usage 字段（**断言缺席**，不是跳过）

`fn unreported_usage_fields() -> &'static [UsageField] { &[] }`（`UsageField` = `Input`/`Output`/`Total`/`CacheRead`/`CacheWrite`/`Details`）。`check_usage_shapes` 对未声明的字段照旧断言等于 provider 报的值；对声明的字段**断言它是 `None`**——在「provider 报了完整 usage」的场景里也断言 `None`。这是**更严**的检查，不是更松：声明了一个 provider 不报的字段后，adapter 若把它编出来就会失败（ADR 0007 J2：provider 没报的数字永不得编）。`expected_usage(h, scripted)` 给出该断言用的期望值；`check_total_not_computed` 对所有 adapter 不变，不受出口影响。

**实例**：Anthropic 官方 usage 的完整字段集合是 `input_tokens`、`output_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens`、`cache_creation`、`output_tokens_details`、`server_tool_use`、`service_tier`、`inference_geo`（`https://platform.claude.com/docs/en/api/messages`，访问 2026-10-12），**没有** `total_tokens`。所以 Anthropic harness 声明 `UsageField::Total`，adapter 的 `ModelUsage.total` 必须永远为 `None`。

两个出口都有「声明了就生效、没声明就必须满足」的测试，以及「声明了却仍然填值」的变异被抓住（见 `crates/agent-provider-contract/src/lib.rs` 的 `tests` 与 `crates/agent-provider-anthropic/MUTATIONS-ANTHROPIC.md`）。OpenAI adapter 两个出口都**不声明**，它的 12 个契约测试保持全绿。

## Anthropic adapter 已接入

`crates/agent-provider-anthropic`（API key 认证，无 OAuth）的 12 个契约测试全部委托给同一套检查，现已全绿。线格式依据（官方 URL + 访问日期）、`stop_reason` 取值集合与 usage 字段全集见 `docs/providers/anthropic-wire-notes.md`。

## 接第三家时预计会暴露的地方（2026-10-12 评估，非结论）

第二个实现（Anthropic）暴露出两处泄漏。以下是在接第三家（Gemini/Cohere/Bedrock 等）前、按现有契约内容**预计**还会撞上的地方——写在这里是为了下次不用重新发现，不是为了预先放宽契约：

1. **错误文本格式与错误分类是规定的**。`check_error_classification` 要求错误信息包含 `HTTP {status}` 并引用响应体，且 500/429/408 → `Transport`、其余 4xx → `Protocol`。若第三家的错误不是 HTTP（gRPC/仅 SSE error 事件）或错误体需要解析出别的码，adapter 会被迫拼出 `HTTP …` 文本。可能需第三个出口：声明“错误不以 HTTP 表达”或让检查只断言分类、不断言文本。
2. **`Reason` 枚举本身就是 OpenAI 与 Anthropic 的混合**（`ContentFiltered` 来自 OpenAI，`PauseTurn` 来自 Anthropic）。Gemini 的 `SAFETY`/`RECITATION`/`MALFORMED_FUNCTION_CALL`、Cohere 的 `STOP_SEQUENCE`/`ERROR` 都会要求加变体。考虑改成「每家声明的 provider 原值 → 中性分类」映射，而不是继续扩枚举。
3. **`Refusal` 假定拒绝文本在 content 里**（`check_finish_reasons` 断言 `joined_text == "I can't help."`）。Anthropic 的拒绝还带 `stop_details`（本 adapter 未解析）；若第三家的安全拒绝**无文本**或只有结构化原因，这条会硬失败。
4. **usage 的 `Wire` 变体是 OpenAI 的块形状**（`UsageTrailing` = “无 choices 的尾块”）。Anthropic 没有这种块，靠 harness 映射到 `message_delta`；第三家（如 Gemini 的每块 `usageMetadata`）可能需要新变体或更抽象的“usage 在末尾/每块”描述。
5. **`check_plain_text` 断言内容恰为单个文本 part**（`vec![ContentPart::text("hello")]`）。若第三家把推理/思考作为 `ContentPart` 暴露（而不是像本 adapter 那样忽略），或多文本块，这条会失败。
6. **取消必须是 `Transport` 且消息含 `cancelled`**（`check_cancellation`）。自然返回 `Protocol` 或不同文本的 adapter 会被迫迎合。
7. ~~**usage 出口可被滥用成真空**~~ **已堵（2026-10-12，评审者合并时加）**：`check_usage_shapes` 现在先断言「不是所有字段都被声明为 unreported」，否则直接失败并说明"provider 真的完全不报 usage 是需要决定的事，不是 harness 能声明出来的"。Anthropic 侧有 `declaring_every_usage_field_unreported_is_refused`（`#[should_panic]`）钉住它；两家 adapter 的契约测试均仍全绿。更强的做法（把声明改成"字段 → provider 线上名字"的正向映射，声明即承诺能读到值）仍可考虑，**未做**。