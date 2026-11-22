# agent-provider-common

The code the three provider adapters share (ADR 0026 D12): the SSE reader (`SseParser`), the
`Retry-After` parser (`retry_after`), the usage-limit body recognizers (`usage_limit::go` for
opencode-go, `usage_limit::chatgpt` for the ChatGPT subscription), and, behind the `testing`
feature, the mock HTTP server (`testing::MockServer`, `Reply`, `Barrier`) that each adapter's own
`testing` module wraps with its wire event builders.

Wire-specific code stays in the adapters. The crate changes no adapter behavior; the three
provider contract suites (`agent-provider-contract/README.md`) stay green and unchanged.
Mutation checks are in `MUTATIONS.md`.
