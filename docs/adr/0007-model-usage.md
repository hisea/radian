# ADR 0007: Usage (`ModelUsage`, `include_usage`, Journal Records)

Status: accepted

## Background

Status bars, fees, and session totals need token usage for one model call. Providers report different shapes (OpenAI `prompt_tokens`/`completion_tokens`/`total_tokens`/`prompt_tokens_details`, Anthropic-style `input_tokens`/`output_tokens`/`cache_*_input_tokens`). This ADR defines a provider-independent, self-contained, serializable `ModelUsage`, the OpenAI-compatible adapter `include_usage` configuration plus SSE parsing, and one journal usage record plus event fields.

## Decisions

### J1: `ModelUsage` keeps all fields optional

```rust
pub struct ModelUsage {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub total: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub details: Option<OpaqueJson>,
    pub input_includes_cache: Option<bool>,
}
```

Each field uses `#[serde(default, skip_serializing_if = "Option::is_none")]`. `ModelUsage::default()` serializes as `{}`. A partially filled value shows only present fields. Old JSON without a field reads as `None`. **"Not reported" is `None`, not 0**. In an audit, 0 means "reported 0", and the meanings differ.

### J2: Core never computes `total`

Core stores `total_tokens` only when the provider reports it. A missing value stays `None`. Core **never** fills it from `input + output`: providers define total differently (whether cache counts, whether audio or refusal tokens count). An invented exact number is worse than no number.

### J3: `details` keeps the provider usage subobject unchanged

`details: Option<OpaqueJson>` carries the OpenAI `prompt_tokens_details` (or its `details` key). Core does not interpret, reorder, or reformat it. The SSE chunk already passed through the `serde_json` parse into a `Value`, so key order can be normalized. Tests assert content and nesting structure, not byte order.

### J4: `ModelResponse` carries `usage: Option<ModelUsage>`

Usage belongs to one response, not to a session or a turn.

### J5: The journal records `ModelUsage` directly after its assistant message; no usage means no record

`JournalRecord::ModelUsage { usage }` (the enum is already `#[non_exhaustive]`). Core appends it after the assistant message `commit` **succeeds** and before it dispatches tools. A `None` usage writes nothing. `replay`/`repair`/`recover` ignore it.

### J6: A usage append failure follows the same rule as other write points

`RunError::Journal` plus `RecoveryRequired`. Usage gets no "ignore on failure" exception: a journal failure means memory and journal may diverge.

### J7: The event adds fields; no new event exists

`AgentEvent::ModelResponseReceived` carries `usage: Option<ModelUsage>`. It is the "complete response received" event, and usage is a property of that response.

### J8: `include_usage: Option<bool>`, with default policy by exact host match

`None` = default policy, `Some(b)` = explicit override. Default policy: enable **only when the host is `api.openai.com`**; disable for all other endpoints (including local ones). The adapter parses the host once in `OpenAiCompatible::new` and stores it (`include_usage()` exposes it). The match is **exactly `host == "api.openai.com"`** and never uses `base_url.contains("openai.com")`. When enabled, the request body carries `"stream_options": {"include_usage": true}`. When disabled, the `stream_options` key never appears. `stream_options` and `stream` are both reserved keys (RESERVED). User settings and `extra_body` cannot override them.

Rationale: compatible endpoints handle unknown fields inconsistently (some return 400 at once). Off by default is the safe side. Only the OpenAI endpoint itself is confirmed to support streaming usage return.

### J9: SSE parsing takes usage from three shapes

Core first reads `chunk["usage"]` (non-null), then decides choices. It keeps the last non-empty usage object. Three shapes exist: a trailing standalone usage chunk (`choices: []` plus `usage`), usage on every chunk (take the last non-empty one), and no usage at all (`usage: None`). `null` counts as absent. The mapping uses one alias group (OpenAI plus Anthropic styles). `total` reads only `total_tokens`.

### J10: A 400 hint must point the way and stay masked

When the server rejects `stream_options` with a 400, the error message adds one sentence: "set include_usage to Some(false) to turn the option off". The whole message still passes through the existing `mask()` redaction path. The hint appears only when the request truly sent `stream_options`.

### J11: The adapter marks explicitly whether `input` includes cache hits

Providers define "input" differently: OpenAI-compatible `prompt_tokens` **includes** cache hits; Anthropic `input_tokens` **excludes** `cache_read_input_tokens` and `cache_creation_input_tokens`. Core still stores the provider number unchanged. `input_includes_cache: Option<bool>` is filled by the **adapter**: OpenAI-compatible sets true, Anthropic sets false.

Consumers always convert through two methods and never add from `input` directly:

- `prompt_total()`: all input tokens for this request (including cache reads and writes). Context size and budget baselines (ADR 0015) use it.
- `uncached_input()`: full-price input (excluding cache reads and writes). Fees use it, and cache reads and writes use their own unit prices.

Old records (field is `None`): infer the protocol from the session provider at that time. When inference is impossible, fees show nothing (no invented number), context size treats `input` conservatively, and the report states the assumption.

### J12: Call fees travel with usage records

`ModelUsageRecorded` and `CompactionUsageRecorded` can carry an optional `fee { micros, incomplete? }` (the ADR 0027 call fee). An old record without it restores through the current price table and marks `~`.

## Constraints and costs

- `details` key order can be normalized: the SSE chunk first passes `serde_json::Value`, so byte order is not preserved. Only content and nesting stay. A host that needs byte-exact provider bytes builds its own adapter.
- `include_usage` parses once in `OpenAiCompatible::new`: a later change to `config.include_usage` does not affect a constructed adapter. The host rebuilds the adapter when it changes configuration.
- A usage record follows only a response: a cancelled or interrupted model call with no complete response has no usage record. Half-stream usage is not available.
- `stream_options` stays a reserved key forever. Even with include_usage off, users cannot inject it through settings.

## Verification

HTTP/SSE tests in both provider adapters cover the three usage shapes, non-invented `total`, the `include_usage` request-body matrix, the 400 hint plus redaction, and the `input_includes_cache` mark. `radian-core` covers serialization round trips, journal order, event fields, and `replay`/`recover` ignoring usage. `journal-selfcheck` replay recognizes only `Message` records.
