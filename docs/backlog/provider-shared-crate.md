# provider 公共代码抽到一个 crate

- 状态：decided
- 涉及 ADR：0026、0017
- 来源：2026-11-21 精简调查

## 背景

三个 provider crate（anthropic、openai-compatible、openai-responses）各有一份几乎相同的代码：`sse.rs`（SSE 解析，约 6 KB ×3）、`model.rs` 里的 `retry_after()`/`parse_retry_after()` 及其测试、`testing.rs` 测试工具（约 13 KB ×3，`sse`、`sse_with_headers`、`sse_split`、`sse_pieces`、`sse_head`），两个 OpenAI crate 还各有一份 `usage_limit.rs`。`agent-provider-contract` 是测试契约 crate，不适合放运行时代码。

## 决定（已定）

- 新建 `agent-provider-common`：放 SSE 解析、`Retry-After` 解析、`usage_limit`；`testing` feature 下放共享测试工具。
- 三个 provider crate 改用它，删除各自的副本；行为不变。
- 三个 provider contract 套件不改、保持全绿（`agent-provider-contract/README.md` 的规则不变）。

## 验收

三个 crate 里不再有 `sse.rs`、`retry_after` 和各自的 SSE 测试工具副本；三个 contract 套件与 Verify 全绿。
