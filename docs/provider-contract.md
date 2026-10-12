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

## 变异保证

10 条契约每条都有一个针对性变异（在一次性检出里破坏 adapter 对应行为），全部被对应契约测试抓住，见 `crates/agent-provider-contract/MUTATIONS-CONTRACT.md`。抓不住错误的契约没有价值——这条保证是套件本身的一部分。