# agent-provider-anthropic

A model adapter for the Anthropic Messages API (`POST /v1/messages`), implementing
`agent_core::Model` with streaming, using **API key** authentication only.
`Anthropic::new(config)` builds it; `AnthropicConfig` holds the knobs.

It does **not** implement subscription OAuth — Anthropic's policy forbids routing consumer
subscriptions through third-party tools, and `docs/spikes/oauth.md` records the spike. It also
does not own: the turn loop, tools, hooks or the transcript (`agent-core`); where the key comes
from (the library never reads the environment); retries; and server-side tools
(`server_tool_use`, `web_search_tool_result`, …) — those are ignored, never turned into a
`ToolCall`, because the core's rule is that a tool the provider runs on its own side is never a
`ToolCall`.

Every wire fact this adapter relies on is recorded with its official URL and access date in
`crates/agent-provider-anthropic/WIRE.md`; that file, not this one, is the authority on the
format.

## Config

`AnthropicConfig::new(base_url, model)`, `with_api_key(ApiKey)`, `with_max_tokens(u32)`,
`with_thinking(ThinkingConfig)`, `with_cache(CacheMode)`, `with_anthropic_version(impl
Into<String>)`. Fields:
`base_url` (everything before `/v1/messages`), `model`, `api_key`, `anthropic_version`
(default `DEFAULT_ANTHROPIC_VERSION` = `"2023-06-01"`), `max_tokens` (default 4096; the adapter
**always** sends it — the Messages API lists it without an `optional` qualifier and every
official example carries it), `thinking: Option<ThinkingConfig>`, `cache: CacheMode`, `headers`,
`extra_body`
(`model`/`messages`/`max_tokens`/`stream`/`system`/`tools`/`tool_choice` cannot be set this
way), `connect_timeout` (30 s), `idle_timeout` (120 s), `max_response_bytes` (32 MiB).

`ThinkingConfig` is `Enabled { budget_tokens }` (`{"type":"enabled","budget_tokens":N}`;
deprecated on Claude 4.6, rejected by 4.7+) or `Adaptive` (`{"type":"adaptive"}`; recommended on
4.6+). `None` sends **no** `thinking` key, so a thinking-free request is byte-identical to the
adapter before extended thinking existed. The adapter never turns thinking on by itself, and it
does **not** validate `budget_tokens` locally — the server is the authority (wire notes §2.3);
`radian-host` checks the documented range before a session file is created.

`CacheMode` is `Auto` (**default**) or `None` (wire notes §2.4). Under `Auto` the adapter places
at most **three** explicit `cache_control: {"type":"ephemeral"}` breakpoints — one on the last
tool definition, one on the end of the system prompt (sent as a one-block array), and one on the
last cacheable block of the last message — of the four the API allows. Thinking/redacted_thinking
blocks and empty blocks are never marked, and a part the request does not carry gets no
breakpoint. `None` sends no `cache_control` at all, so the body is byte-identical to the adapter
before prompt caching existed. The adapter never sends the top-level automatic-caching field and
never sets a TTL.

`ApiKey` never appears in `Debug` output, and the adapter masks it plus every custom header value
in every error it returns, including text a server echoes back. `ConfigError` is `BaseUrl` /
`InsecureKey` / `Header` / `Client`.

## The mapping, stated rather than hidden

- Text and tool calls round-trip exactly. Anthropic has no `tool` role: consecutive
  `Message::Tool` results are merged into one `user` message of `tool_result` blocks, in order
  (matching the official round-trip example).
- Images go out as base64 blocks. `Audio`, `ResourceLink`, `EmbeddedResource` and `Opaque`
  become short text placeholders — Anthropic has no input block for them. Lossy **by design**.
- `reasoning` and `MessageMeta` are provider-invisible: a history with them and the same history
  without them produce a byte-identical request body.
- `tool_use.input` is always an object; a string that parses to an object is used, anything else
  becomes `{}`. The adapter does not invent structure for arguments the model produced badly.
- `total` is always `None`: Anthropic's usage object has no total field (wire notes §3), and the
  contract's usage hatch asserts the absence rather than skipping the field (ADR 0007 J2).
- **Extended thinking is `provider_payload`, not `reasoning`.** A `thinking`/`redacted_thinking`
  block (with its `signature`/`data`) is preserved and echoed back **before** the `tool_use` it
  accompanied, in the order it was streamed. Thinking text is never visible content and never a
  `ModelReasoningDelta` — mixing the two would either send provider-invisible reasoning to the
  provider or drop blocks the provider requires back (ADR 0014 R10).
- **Prompt caching is on by default and changes the body only on the wire.** `CacheMode::Auto`
  adds explicit `cache_control` breakpoints; `CacheMode::None` restores the exact pre-caching
  bytes. The choice is the host's; the adapter never reads the environment.
- **Rate-limit headers are kept raw, never parsed.** Every `anthropic-ratelimit-*` response
  header rides on `ModelResponse::limits` in arrival order (`None` when the provider sent
  none); the host's `Headers` usage source parses them (ADR 0027 slice ④).

## What it guarantees (each with the test that holds it)

The **adapter-neutral contract** suite runs first: `tests/contract.rs` implements
`agent_provider_contract::ContractHarness` for this adapter and delegates to twelve checks. It
uses **both** escape hatches, each with the provider documentation as the reason:

- `unsupported_reasons()` declares `Reason::ContentFiltered` — Anthropic's seven official
  `stop_reason` values have no content-filter value (it uses `refusal`, which the contract checks
  separately).
- `unreported_usage_fields()` declares `UsageField::Total` — the contract then **asserts** it
  stays `None`, which is stricter than skipping.

| Check | Test | File |
|---|---|---|
| 1 plain text; 2 streamed deltas in order; 3a/3b one and several tool calls; 4 split arguments rejoined; 5 call ids unique; 6 finish reasons; 7a usage shapes; 7b `total` not computed; 8 error classification; 9 cancellation; 10 secret not leaked; 11 context overflow | `contract_1_plain_text` … `contract_11_context_overflow` | `tests/contract.rs` |
| The usage hatch cannot be widened into an opt-out: declaring every field unreported fails before the check can pass vacuously | `declaring_every_usage_field_unreported_is_refused` (`#[should_panic]`) | `tests/contract.rs` |

Wire-level behaviour, over a real HTTP mock (`tests/http.rs`) and in unit tests:

| Guarantee | Test | File |
|---|---|---|
| A turn sends the expected request (path, `x-api-key`, `anthropic-version`, body) and returns the text | `a_turn_sends_the_expected_request_and_returns_the_text` | `tests/http.rs` |
| The user agent names radian; a base URL with a trailing slash or a path works; the `anthropic-version` header defaults and can be overridden | `the_user_agent_names_radian`, `a_base_url_with_a_trailing_slash_or_a_path_works`, `the_anthropic_version_header_defaults_and_can_be_overridden` | `tests/http.rs` |
| `max_tokens` comes from the config; the system prompt and tools are sent in their Anthropic shape; reserved settings are refused | `max_tokens_comes_from_the_config`, `the_system_prompt_and_tools_are_sent_in_their_anthropic_shape`, `reserved_settings_are_refused` | `tests/http.rs` |
| A response split into tiny pieces is read the same; a tool call round-trips through the core; a server that reuses tool call ids does not break the conversation | `a_response_split_into_tiny_pieces_is_read_the_same`, `a_tool_call_round_trips_through_the_core`, `a_server_that_reuses_tool_call_ids_does_not_break_the_conversation` | `tests/http.rs` |
| Thinking and server-tool blocks are not content; a thinking turn echoes the blocks back on the next request | `thinking_and_server_tool_blocks_are_not_content`, `a_thinking_turn_echoes_the_blocks_back_on_the_next_request` | `tests/http.rs` |
| Every official `stop_reason` value maps through the wire | `stop_reason_values_map_through_the_wire` | `tests/http.rs` |
| A stream that ends before finishing, and a `message_stop` without a `stop_reason`, are errors | `a_stream_that_ends_before_finishing_is_an_error`, `a_message_stop_without_a_stop_reason_is_an_error` | `tests/http.rs` |
| A silent server hits the idle timeout; cancelling stops a request waiting on the server; a directly-called model honours the token; an over-limit response is refused | `a_silent_server_hits_the_idle_timeout`, `cancelling_stops_a_request_that_is_waiting_on_the_server`, `a_model_called_directly_honours_the_cancellation_token`, `a_response_over_the_size_limit_is_refused` | `tests/http.rs` |
| HTTP errors are classified and never show the key; a redirect is not followed so the key stays put; a connection error is a transport error without the URL | `http_errors_are_classified_and_never_show_the_key`, `a_redirect_is_not_followed_so_the_key_stays_put`, `a_connection_error_is_a_transport_error_without_the_url` | `tests/http.rs` |
| `message_start` and `message_delta` usage are merged; `details` are kept verbatim; no usage reports `None`; `total` is never invented | `message_start_and_message_delta_usage_are_merged`, `usage_details_are_kept_verbatim`, `a_response_without_usage_reports_none`, `total_is_never_invented_when_the_provider_omits_it` | `tests/http.rs` |
| An HTTP error body, a stream error and a custom header value that echo the key are masked; `Debug` hides the key and header values; URLs never leak credentials | `an_http_error_body_that_echoes_the_key_is_masked`, `a_stream_error_that_echoes_the_key_is_masked`, `a_custom_header_value_echoed_by_the_server_is_masked`, `the_key_is_hidden_from_debug_output`, `debug_output_hides_the_key_and_header_values`, `urls_never_leak_credentials_through_debug_or_config_errors` | `tests/http.rs` |
| A key is never sent over plain http to a remote host; bad configuration is reported | `a_key_is_never_sent_over_plain_http_to_a_remote_host`, `bad_configuration_is_reported` | `tests/http.rs` |
| Request building: the required fields and `stream: true` are always sent; `meta` and `reasoning` are not sent; a tool conversation maps to Anthropic blocks; interrupted assistants are plain; images are base64 blocks; what the API cannot take becomes a placeholder; arguments that are not an object become `{}`; settings override the config and reserved keys are refused; `max_tokens` comes from the config | `the_required_fields_and_stream_flag_are_always_sent`, `host_message_meta_is_not_sent_to_the_provider`, `reasoning_is_not_sent_to_the_provider`, `a_tool_conversation_maps_to_anthropic_blocks`, `interrupted_assistants_are_sent_as_plain_assistants`, `images_are_sent_as_base64_blocks`, `what_the_api_cannot_take_becomes_a_placeholder_in_text`, `arguments_that_are_not_an_object_become_an_empty_input`, `settings_override_the_config_and_reserved_keys_are_refused`, `max_tokens_comes_from_the_config` | `src/request.rs` |
| Thinking: off leaves the request body unchanged; `Enabled` and `Adaptive` send their objects; the configured thinking wins over `extra_body` | `thinking_off_leaves_the_request_body_unchanged`, `thinking_enabled_sends_the_manual_object`, `thinking_adaptive_sends_the_adaptive_object`, `configured_thinking_wins_over_extra_body` | `src/request.rs` |
| Prompt caching: `Auto` (the default) puts one breakpoint each on the last tool definition, the end of the system prompt (as an array), and the last cacheable block of the last message — three of the four slots; no breakpoint for a part the request does not carry; never on a thinking/redacted/empty block (an empty trailing text block sends the breakpoint back to the previous non-empty one), and an empty text block is detectable only through an empty `EmbeddedResource` (`blocks()` drops empty `Text` parts); the limit counts only the adapter's own breakpoints, so a caller's lookalike `cache_control` key (in a tool schema or tool arguments) is left alone; `None` is the byte-identical pre-caching body | `the_default_cache_mode_is_auto`, `auto_places_the_three_breakpoints_where_stated`, `breakpoints_follow_what_the_request_carries`, `none_sends_the_body_the_adapter_sent_before_prompt_caching`, `auto_leaves_an_empty_system_prompt_as_a_plain_string`, `auto_never_marks_a_thinking_block`, `auto_never_marks_a_string_content_message`, `auto_marks_a_trailing_tool_result_block`, `auto_skips_a_trailing_empty_text_block`, `auto_never_marks_an_empty_text_block`, `auto_does_not_mistake_a_user_data_key_for_a_breakpoint` | `src/request.rs` |
| Preserved thinking blocks are echoed before `tool_use`, in order, with signatures unchanged; a payload read back from JSON still echoes thinking; a payload that is not an array is refused; an empty payload array sends the plain assistant | `preserved_thinking_blocks_are_echoed_before_tool_use_in_order`, `a_payload_read_back_from_json_still_echoes_thinking`, `a_provider_payload_that_is_not_an_array_is_refused`, `an_empty_provider_payload_array_sends_the_plain_assistant` | `src/request.rs` |
| SSE parsing: events split at blank lines; any split point gives the same events; a multi-byte character split across chunks survives; a bare `\r` ends a line; a final event without a blank line is still returned; other fields and comments are ignored; invalid UTF-8 is a protocol error; an endless line is cut off | `events_are_split_at_blank_lines`, `a_split_anywhere_gives_the_same_events`, `a_multibyte_character_split_across_chunks_survives`, `a_bare_carriage_return_ends_a_line`, `the_final_event_without_a_blank_line_is_still_returned`, `other_fields_and_comments_are_ignored`, `invalid_utf8_is_a_protocol_error`, `an_endless_line_is_cut_off` | `src/sse.rs` |
| Assembly: text deltas become one part and stream as events; multiple text blocks stay separate parts; a `tool_use` with partial JSON is assembled; an input from the `content_block_start` is used without deltas; thinking blocks are preserved and never become content; redacted blocks round-trip verbatim; thinking blocks keep their streamed order; a response without thinking has no payload | `text_deltas_become_one_part_and_stream_as_events`, `multiple_text_blocks_stay_separate_parts`, `a_tool_use_block_with_partial_json_is_assembled`, `a_tool_use_input_from_the_start_block_is_used_without_deltas`, `thinking_blocks_are_preserved_and_never_become_content`, `redacted_thinking_blocks_round_trip_verbatim`, `thinking_blocks_keep_their_streamed_order`, `a_response_without_thinking_has_no_provider_payload` | `src/stream.rs` |
| Assembly, continued: missing ids are made up and repeated ids renamed; `stop_reason` values map; a stream without `message_stop`, and `message_stop` without a `stop_reason`, are errors; an error event ends the response; `ping` and unknown events are ignored; usage merges field by field and takes the last non-null value; `details` keep the provider sub-object; no usage means `None`; `total` is never invented; an over-limit response is cut off; garbage is a protocol error | `missing_ids_are_made_up_and_repeated_ids_are_renamed`, `stop_reasons_map_to_stop_reasons`, `a_stream_without_message_stop_is_an_error`, `message_stop_without_a_stop_reason_is_an_error`, `an_error_event_ends_the_response`, `ping_and_unknown_events_are_ignored`, `usage_merges_message_start_and_message_delta_field_by_field`, `usage_takes_the_last_non_null_value_per_field`, `usage_details_keep_the_provider_sub_object`, `no_usage_means_none`, `total_is_never_invented`, `a_response_over_the_limit_is_cut_off`, `garbage_is_a_protocol_error` | `src/stream.rs` |

End to end across a real session file: a thinking block (with its signature) survives close and
resume — `a_thinking_block_survives_close_and_resume` in `radian-runtime/tests/thinking_resume.rs`.

`MUTATIONS-ANTHROPIC.md` records one targeted mutation per contract check (M1–M12), four for the
two escape hatches (M13–M16), seven for extended thinking (M17–M23) plus the end-to-end resume
one (M24), and seven for prompt caching (M32–M38: a missing tool breakpoint, `None` still
caching, a fifth breakpoint, a plain-string system under `Auto`, a marked thinking block, a
whole-body scan that mistakes a caller's lookalike key for a breakpoint, and an empty text block
treated as cacheable).
All caught; the record says plainly that the adapter's own `tests/http.rs` cases were not
individually mutated.

## The `testing` feature

`testing` exposes an in-process mock server for this crate's tests and for dependent crates:
`MockServer::start(Vec<Reply>)`, `Reply` (`Sse`, `SseThenHang`, `SseThenCut`, `Status`),
`Reply::sse`, `Reply::sse_with_headers`, `Reply::status`, `Reply::sse_split`, the event builders `message_start`,
`text_start`, `text_delta`, `input_json_delta`, `tool_start`, `block_stop`, `message_delta`,
`message_stop`, `ping`, `error`, and `RecordedRequest` (`header`, `json`) /
`MockServer::requests`. It binds `127.0.0.1:0` and is compiled only under the feature (ADR 0013).

## Known limits and what is not verified

- **No OAuth, by policy** (see the top). API key only.
- **Not implemented:** `stop_details` (the refusal detail), `container`, `citations`,
  `tool_choice` on the request side, the `anthropic-beta` header, `metadata`, and server-side
  tool blocks (`server_tool_use`, `web_search_tool_result`, `web_fetch_*`,
  `code_execution_*`) — those are ignored and never become a `ToolCall`.
- **Prompt caching covers only the explicit breakpoints, and only on this adapter's three
  sites.** Not implemented: the top-level automatic-caching field, `ttl`, and any adaptive
  placement (the adapter does not measure prompt length, so a prompt below the model's minimum
  cacheable length is simply not cached by the server — no error, wire notes §2.4). `radian-host`
  does **not** yet expose `CacheMode` (neither as a `[providers.<id>] cache` key nor a flag), so
  a host today takes the `Auto` default and cannot turn it off without code; the config key was
  left out of this slice because adding it needs the per-provider plumbing, a schema fixture and
  a decision record (see `docs/adr/`).
- **Hand test (not run here, unverified):** with an Anthropic key already available to radian,
  send the same request with a long system prompt twice in one session: the second response's
  `usage.cache_read_input_tokens` should be greater than 0 (the first reports
  `cache_creation_input_tokens > 0`). This was **not** run in this worktree — no key is present
  and none was written or printed.
- **`thinking` sub-fields are not parsed**: `display`/`block_binding` can be passed through
  `extra_body`, but the adapter does not read them, and the wire notes record as **unverified**
  whether such extra fields can be merged once the adapter inserts the top-level `thinking` key.
- **No local validation of `budget_tokens`/`temperature`/`tool_choice` combinations.** The
  server is the authority; a wrong combination comes back as a 400, exactly as for `extra_body`.
  (`radian-host` checks only the documented budget range, before a session is created.)
- **Unverified in the wire notes:** whether `max_tokens` is formally `required` (the evidence is
  "not marked optional, and every official example sends it"); whether Anthropic rejects a tool
  name containing `.`/`:` (the documented pattern is `^[a-zA-Z0-9_-]{1,128}$`, and the adapter
  does **not** rewrite tool names — MCP aliases can contain other characters); whether a
  `redacted_thinking` block can also carry deltas; whether a `thinking` block can be interleaved
  after text/tool blocks (the adapter preserves block index order but always puts the preserved
  blocks **first** when echoing them back, which would not satisfy a provider that required the
  original relative position); and whether `input_tokens` always appears in `MessageDeltaUsage`.
- **Two lossy neutralisations**, recorded in the wire notes §5 rather than hidden:
  `stop_sequence` → `EndTurn` (the response did finish normally; which sequence fired is lost,
  and the core has no field for it) and `model_context_window_exceeded` → `MaxTokens` (the docs
  say to treat it as truncated, and `MaxTokens` is what keeps the core from executing tool calls
  that may have been cut off). Any other value becomes `StopReason::Other(<raw>)`.
- **Usage fields not mapped**, and therefore lost: `cache_creation` (its TTL breakdown; the sum
  is already `cache_write`), `server_tool_use`, `service_tier`, `inference_geo`.
- **Key order, not byte order, after a round trip.** `provider_payload` is `OpaqueJson`, so the
  JSONL round trip is byte-exact; rebuilding a request goes through `serde_json::Value`, which
  normalises key order (ADR 0007 J3's accepted trade-off). Field values and text are unchanged.
- **Not verified against the real API.** Every test runs against the in-process mock; no real
  key was used, and no real network call was made.
- The contract's usage hatch is asserted to be non-vacuous, but the wire notes still list a
  stronger design as **not done**: turning the declaration into a positive
  "field → provider wire name" mapping, where declaring a field is a promise that a value can be
  read.

## Related ADRs

0005 G4 (`meta` is provider-invisible) · 0007 (usage; J2: never invent a number the provider did
not report) · 0010 (deltas are live projections) · 0013 (T1/T4: no telemetry, redaction) · 0014
R10 (extended thinking belongs to `provider_payload`, **not** to `reasoning` — the record of the
first slice that deliberately did not implement it, and of the follow-up that did). The contract
and its two escape hatches are documented in `crates/agent-provider-contract/README.md`; the wire evidence is
`crates/agent-provider-anthropic/WIRE.md`; the OAuth decision is `docs/spikes/oauth.md`.
