# OpenAI Responses wire notes (`agent-provider-openai-responses`)

Every field and event `agent-provider-openai-responses` relies on, with the source it came
from. The primary reference is the locally installed omp source (read-only, **untrusted**, not
run or imported): `~/.bun/install/global/node_modules/@oh-my-pi/pi-ai` and
`@oh-my-pi/pi-catalog` (v17.3.8). The omp files are a third-party Codex client, so anything
that is **Codex-specific** (rather than the standard Responses API) is flagged. Nothing here was
checked against a live endpoint: `openai-responses-wire-notes` is code-plus-omp evidence, and
the items marked *unverified* are the ones no official OpenAI page in this repo confirms.

## 1. Endpoint and headers

- Codex Responses path: base `https://chatgpt.com/backend-api`
  (`pi-catalog/src/wire/codex.ts:5`), request path `/codex/responses`
  (`pi-catalog/src/wire/codex.ts:43`). This adapter takes the whole prefix (e.g.
  `https://chatgpt.com/backend-api/codex`) as `base_url` and appends `/responses`.
- Headers the caller supplies (`pi-catalog/src/wire/codex.ts:12-39`):
  `Authorization` (the OAuth access token), `chatgpt-account-id`, `OpenAI-Beta:
  responses=experimental`, and `originator`.
  - **Originator**: omp writes its own name `pi`
    (`codex.ts:38`); radian writes `radian` and does **not** impersonate the Codex CLI.
  - **Unverified**: the official OpenAI Responses API does not document `chatgpt-account-id`,
    `OpenAI-Beta: responses=experimental` or `originator`; they come from the omp Codex client.
- The adapter does not parse the token or derive `chatgpt-account-id`; the host does that.

## 2. Request body

- `model`, `input`, `stream: true`
  (`pi-ai/src/providers/openai-codex-responses.ts:1496-1501`).
- `store: false` (`pi-ai/src/providers/openai-codex/request-transformer.ts:387`) — this is the
  stateless mode radian uses (ADR 0026).
- `instructions` = the first system prompt
  (`openai-codex-responses.ts:1522-1525`).
- `include: ["reasoning.encrypted_content"]`
  (`request-transformer.ts:509-511`, `openai-shared.ts:3466-3469`; the `include` value is typed
  at `openai-responses-wire.ts:2815`, documented at `:2800`). Asking for it is what makes the
  server return a replayable `reasoning` item (`openai-responses-wire.ts:4489-4493`).
- `reasoning` object (effort/summary) (`request-transformer.ts:459-468`).
- `tools`: `{"type":"function","name","description","parameters"}`
  (`openai-codex-responses.ts:4524-4561`).
- omp drops `max_output_tokens`/`max_completion_tokens` and sampling params the Codex backend
  refuses (`request-transformer.ts:513-514`, `openai-codex-responses.ts:1503-1510`); this
  adapter sends none of them.

### 2.1 Input items

- User message: `{"type":"message","role":"user","content":[{"type":"input_text","text":…}]}`
  (`openai-shared.ts:1621`, `:4393`); images are `{"type":"input_image","image_url":"data:…"}`
  (`openai-shared.ts:2318-2322`). The Responses input vocabulary has **no audio** part, so the
  adapter sends a text placeholder.
- Assistant message: `{"type":"message","role":"assistant","content":[{"type":"output_text",
  "text":…}]}` (`openai-shared.ts:2096-2097`).
- Tool call: `{"type":"function_call","call_id":…,"name":…,"arguments":"<json string>"}`
  (`openai-shared.ts:2163-2169`; type at `openai-responses-wire.ts:2492-2530`; `id` is optional
  at `:2512`). The adapter uses the provider's `call_id` for `ToolCall.id` and omits the item
  `id`.
- Tool result: `{"type":"function_call_output","call_id":…,"output":"<string>"}`
  (`openai-shared.ts:2302-2306`; type at `openai-responses-wire.ts:2551-2564`).
- Reasoning item (replayed verbatim): `{"type":"reasoning","id":…,"summary":[…],
  "encrypted_content":…}` (`openai-shared.ts:2183-2189`;
  `openai-responses-wire.ts:4472-4499`). This is provider-visible protocol data
  (ADR 0014 R10), stored in `Message::Assistant.provider_payload` and echoed back unchanged.

## 3. SSE events

Standard Responses events; the decoder branches are in `openai-shared.ts` /
`openai-codex-responses.ts`, the type declarations in `openai-responses-wire.ts`.

| Event | Where read | Used for |
|---|---|---|
| `response.created` | `openai-shared.ts:2885` | response id; carries `usage` on some shapes |
| `response.output_item.added` | `openai-shared.ts:2887` | reasoning / message / function_call start |
| `response.output_item.done` | `openai-shared.ts:3045` | authoritative item (text, arguments, reasoning item) |
| `response.output_text.delta` | `openai-shared.ts:2997` | visible text → `ModelTextDelta` |
| `response.refusal.delta` | `openai-shared.ts:3010` | refusal text → `ModelTextDelta` |
| `response.reasoning_summary_text.delta` | `openai-shared.ts:2964` | reasoning summary → `ModelReasoningDelta` |
| `response.reasoning_text.delta` | `openai-shared.ts:2981` | raw reasoning → `ModelReasoningDelta` |
| `response.function_call_arguments.delta` | `openai-shared.ts:3023` | tool-call arguments (split) |
| `response.function_call_arguments.done` | `openai-shared.ts:3028` | authoritative arguments |
| `response.completed` | `openai-codex-responses.ts:2250`, type `openai-responses-wire.ts:1376` | terminal, usage, status |
| `response.incomplete` | `openai-codex-responses.ts:2250`, wire `:2820` | `incomplete_details.reason` |
| `response.failed` | `openai-codex-responses.ts:2273`, wire `:2059` | error |
| `error` | `openai-codex-responses.ts:2273`, wire `:2034` | error |

- `response.output_text.done` / `content_part.*` / reasoning-summary **part** markers carry
  nothing this adapter needs; they are ignored (`openai-shared.ts:2961-2996` shows the part
  bookkeeping the adapter skips).
- Unknown event kinds are ignored (version policy).

## 4. Usage

`response.usage` (`openai-responses-wire.ts:5105-5147`):

- `input_tokens` → `ModelUsage.input`; it **contains** `input_tokens_details.cached_tokens`
  (`:5109-5136`), so `input_includes_cache = Some(true)` (ADR 0007).
- `input_tokens_details.cached_tokens` → `cache_read` (`:5131-5136`).
- `output_tokens` → `output` (`:5117`).
- `output_tokens_details.reasoning_tokens` → kept inside `details` (`:5141-5146`).
- `total_tokens` → `total` (`:5125`); never computed.
- Codex additionally reports `input_tokens_details.cache_write_tokens` (parsed by omp at
  `openai-codex-responses.ts:3240-3243`); the adapter maps it to `cache_write` when present.
  **Unverified**: not in the standard `ResponseUsage.InputTokensDetails` type
  (`openai-responses-wire.ts:5131-5137`).
- The adapter records the two sub-objects under the neutral ADR 0007 J3 keys
  `prompt_tokens_details`/`completion_tokens_details` in `ModelUsage.details` (the same keys the
  Chat Completions adapter uses), keeping the sub-object content verbatim.

## 5. Stop reasons

`mapOpenAIResponsesStopReason` (`openai-shared.ts:3261-3283`) and
`promoteResponsesToolUseStopReason` (`openai-shared.ts:3344-3358`):

- `completed` → `EndTurn`; `in_progress`/`queued` → `EndTurn`.
- `incomplete` → `MaxTokens` (`:3266-3267`). `incomplete_details.reason == "content_filter"` is
  a content filter (`openai-shared.ts:3208`), mapped to `ContentFiltered`.
- tool calls present and `EndTurn` → `ToolUse` (`:3349-3354`).
- `end_turn: false` → `PauseTurn` (`openai-shared.ts:3355-3357`; the field is read at
  `openai-codex-responses.ts:2423`). **Codex-specific / unverified**: `end_turn` is not in the
  standard Responses response type.
- refusal content and no tool calls → `Refusal`.
- any other `status` → `StopReason::Other(status)` (the contract's `Reason::Unknown`), rather
  than omp's silent `stop` (`:3274-3282`).

`ResponseStatus` values: `openai-responses-wire.ts:4794`.

## 6. Rate-limit headers

`x-codex-primary-*` / `x-codex-secondary-*` (e.g. `used-percent`, `window-minutes`,
`reset-at`) are the subscription windows (ADR 0026). The adapter keeps them raw
on `ModelResponse.limits`; nothing is parsed. ADR 0027 slice ④ will parse them.
**Unverified**: the exact header names beyond the two `x-codex-{primary,secondary}-` prefixes are
not enumerated in this repo; the adapter matches the prefixes only.
