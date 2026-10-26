# ADR 0014: Reasoning Content

Status: accepted

## Background

Reasoning models stream the reasoning process in a separate field (for example `delta.reasoning_content`). Visible `delta.content` is often empty at the same time. When Core accumulates only visible text, the cancellation prefix (ADR 0003) is always empty for such models. The whole reasoning phase would be lost. Reasoning needs its own events, fields, and persistence. It also needs per-model pass-back rules.

## Decisions

### R1: `AgentEvent::ModelReasoningDelta { text }`

`AgentEvent` is already `#[non_exhaustive]`. The adapter emits deltas in order. They use the same event outlet as `ModelTextDelta`. They follow the same short-and-non-blocking `EventSink` contract. They **never share one variant**: otherwise the host cannot distinguish them, and the TUI cannot render reasoning and visible answers separately.

### R2: `Message::Assistant.reasoning: Vec<ContentPart>`

`#[serde(default, skip_serializing_if = "Vec::is_empty")]`. It is **not** `Option<String>`: `ContentPart` matches the shape of `content`, so non-text reasoning blocks use the same field. It must persist: otherwise "commit produced reasoning on cancellation" is impossible.

### R3: No new `ContentPart::Reasoning`

`ContentPart` items in `content` are exactly what goes to the provider. Reasoning in the same sequence would read as visible content. One field carries one meaning.

### R4: No pass-back by default; pass back only when the model declares it

Both adapters ignore `reasoning` when they build request bodies by default: history with full `reasoning` and history with the same content but no `reasoning` produce byte-identical request bodies. But DeepSeek and similar models require full `reasoning_content` pass-back when tools are present. Without it they return 400.

- `OpenAiCompatibleConfig.reasoning_replay: Option<String>`. The value is the field name used for pass-back. The default is `None`.
- With `None`, the request body is fully identical to history without reasoning (R4 tests pin this behavior).
- With `Some(field)`, each assistant message with non-empty reasoning writes `"<field>": <reasoning text>`. Multiple parts join with `\n`.
- **With pass-back on, an assistant message without reasoning still carries `"<field>": ""`**. DeepSeek docs require it on "all later requests". This covers pre-switch history, cancellation prefixes, and old-session messages.
- The Anthropic adapter is unchanged: extended-thinking `thinking` and `redacted_thinking` blocks (including `signature` and `data`) pass back verbatim through `provider_payload`. They **never** become `reasoning`. They **never** emit `ModelReasoningDelta`. This is a provider-visible channel, different from R4.

### R5: The field name comes from the catalog, never guessed from provider name or URL

`radian-host` parses the models.dev entry `interleaved`:

- `{"field": "reasoning_content"}` or another plain-text field (`reasoning`, `reasoning_text`) sets it as `reasoning_replay`;
- `{"field": "reasoning_details"}` (a structured array with a different shape) or a bare `true` means **no** pass-back. `--verbose` and diagnostics state the reason (unsupported, not supported).

The `interleaved` value is read from the catalog in ADR 0018's order: the local cache when it parses, else the bundled snapshot, never the network.

`[providers.<id>]` accepts `reasoning_replay = "reasoning_content"` or `false` to override the catalog. `--offline-catalog` or a wrong catalog entry still leaves a way out.

### R6: Passed-back reasoning counts toward the context budget

With pass-back on, reasoning really consumes input tokens. `AgentConfig.count_reasoning` (default off) counts `reasoning` in budget estimates when on. The host turns it on when it turns `reasoning_replay` on.

### R7: Core `StreamTap` accumulates visible text and reasoning separately

Two independent buffers, never mixed. Each model call creates a new tap (ADR 0003 F2). Reasoning from one call never leaks into the next. The ADR 0003 F4 "empty" therefore means **visible text and reasoning are both empty**. When either buffer is non-empty, Core commits one `interrupted: true` assistant message. The two buffers enter `content` and `reasoning` respectively.

### R8: `ModelReasoningDelta` is a live projection

Same class as `ModelTextDelta`: slow consumers can drop, merge, or rate-limit it. **The durable form is the `reasoning` field on the committed message**. It passes the single `commit` entry to disk. No new durable record type exists.

### R9: JSONL schema stays at the same version

The assistant variant of `MessageAppended.message` gains one **optional** field. A missing field omits the whole key. Old readers ignore it as an unknown optional field (ADR 0006 §6).

### R10: `ModelResponse.reasoning`

Parallel to `content`, it is where the adapter hands Core the reasoning of one complete response. The OpenAI-compatible adapter reads `delta.reasoning_content` (the observed field name). It emits `ModelReasoningDelta` and accumulates `ModelResponse.reasoning`.

## Crash and interrupt semantics

| Case | Result |
|---|---|
| Cancel with reasoning only | Commit `Assistant { content: [], reasoning: [accumulated], interrupted: true }` |
| Cancel with visible text only | Commit `Assistant { content: [accumulated], reasoning: [], interrupted: true }` |
| Cancel with both | Each enters its own field, never mixed |
| Cancel with neither | Commit nothing |
| Complete response | `reasoning` commits with the assistant message (one journal `Message` record, no independent record) |
| SIGKILL | Still no guarantee for an uncommitted prefix (ADR 0003 F9) |

## Constraints and costs

- `StreamTap` adds one `Mutex` lock per reasoning delta. The event path is already short, so the cost is negligible.
- `Message::Assistant` gains one field: all construction sites update at once (the compiler enforces it).
- Passed-back reasoning consumes input tokens and can return internal model process to the model. The code opens pass-back only when the model requires it.
- Reasoning text enters JSONL, so the sensitive scope widens. The ADR 0006 "JSONL is potentially sensitive data" rule covers this risk. Export drops reasoning by default and includes it only on opt-in.
- Anthropic and OpenAI-compatible reasoning channels stay asymmetric (the former uses `provider_payload`, the latter uses `reasoning`). This asymmetry is intentional.

## Verification

`crates/radian-core/tests/prefix.rs` covers reasoning only, text only, both, neither, fresh accumulation per call, and no independent journal record for reasoning deltas. `message.rs` covers omission when empty and round trips. Both adapters cover `reasoning_is_not_sent_to_the_provider`, catalog `interleaved` parsing and override, pass-back fields and empty strings. Budget tests cover the `count_reasoning` switch. Both provider contracts stay green.
