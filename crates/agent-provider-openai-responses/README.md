# agent-provider-openai-responses

A model adapter for the OpenAI **Responses** API, implementing `agent_core::Model` with
streaming. This is the protocol ChatGPT-subscription (Codex) turns use
(`chatgpt.com/backend-api/codex/responses`), not Chat Completions.

- **No auth logic.** The caller supplies every header — `Authorization`,
  `chatgpt-account-id`, `OpenAI-Beta`, `originator` — via
  [`OpenAiResponsesConfig::headers`]. The login/refresh machinery stays in the host
  (ADR 0026).
- **Stateless.** Every request sends the full `input` with `store: false`
  (ADR 0026).
- **Honest mapping.** Text and tool calls round-trip exactly; a completed `reasoning` item
  (id, summary, `encrypted_content`) is kept verbatim in `provider_payload` and replayed on the
  next request (ADR 0014 R10/revision 1); a payload from another provider is ignored, not
  misread (ADR 0026 D6); `total` is never computed; a context-overflow error is its own kind
  (ADR 0018); a `429` whose body is the plan's used-up usage window
  (`error.type == "usage_limit_reached"`) is a `Protocol` error naming the plan, its window and
  the reset — never retried, and never the raw JSON (`docs/providers/openai-responses-wire-notes.md`
  §7).
- **Rate-limit headers** (`x-codex-primary-*`, `x-codex-secondary-*`) are kept raw on
  `ModelResponse.limits` for a later slice (ADR 0027 slice ④).

Every wire field the adapter relies on, with its source, is in
`docs/providers/openai-responses-wire-notes.md`.

## Testing

The `testing` feature adds an in-process mock HTTP server. The adapter passes the shared
`agent-provider-contract` suite with a Responses harness (`tests/contract.rs`); the wire-level
unit tests are `tests/http.rs` and the per-event units in `src/stream.rs`. Mutations and the
test that catches each are in `MUTATIONS.md`.
