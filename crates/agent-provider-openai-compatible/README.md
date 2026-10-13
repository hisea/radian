# agent-provider-openai-compatible

A model adapter for OpenAI-compatible Chat Completions APIs (OpenAI, and the many servers that
copy it), implementing `agent_core::Model` with streaming. `OpenAiCompatible::new(config)`
builds it; `OpenAiCompatibleConfig` holds the knobs.

It does **not** own: the turn loop, tools, hooks or the transcript (`agent-core`); where the API
key comes from (the library never reads the environment — the host decides); retries (a failed
request ends the turn with a `ModelError` and the host decides whether to try again); and any
provider-specific extension beyond the Chat Completions shape. It does not implement OAuth,
prompt caching, `tool_choice`, or server-side tools.

## Config

`OpenAiCompatibleConfig::new(base_url, model)`, `with_api_key(ApiKey)`,
`with_include_usage(bool)`. Fields: `base_url` (everything before `/chat/completions`),
`model`, `api_key: Option<ApiKey>`, `headers`, `extra_body` (merged under per-request settings;
`model`/`messages`/`tools`/`stream`/`stream_options` cannot be set this way), `connect_timeout`
(30 s), `idle_timeout` (120 s, per silence — there is no total time limit), `max_response_bytes`
(32 MiB), `include_usage: Option<bool>` (`None` = the host-based default: on for
`api.openai.com`, off everywhere else, compared by exact host).

`ApiKey` never appears in `Debug` output, and the adapter masks it — plus every custom header
value — in every error message it returns, including text a server echoes back.
`OpenAiCompatibleConfig`'s `Debug` prints header **names** only and redacts the URL.
`ConfigError` is `BaseUrl` / `InsecureKey` / `Header` / `Client`. `redact_url(raw)` is public so
a host that names the configured endpoint in an error redacts it the same way (ADR 0013 T4).

## The mapping, stated rather than hidden

- Text and tool calls round-trip exactly. Images go out as `image_url` data URLs and audio as
  `input_audio`; `ResourceLink`, `EmbeddedResource` and `Opaque` become short text placeholders
  (`[resource …]`, `[unsupported content omitted]`). This is lossy **by design**.
- `reasoning` and `MessageMeta` are provider-invisible: a history with them and the same history
  without them produce a byte-identical request body.
- `interrupted` assistants go out as plain assistants (same content, byte-identical body).
- `delta.reasoning_content` — the field the dogfooded reasoning model actually sends while
  `content` stays `""` — becomes `ModelResponse.reasoning` and a
  `AgentEvent::ModelReasoningDelta`, never a text delta. It is not in OpenAI's own docs: the
  field name was verified on one endpoint only (`docs/findings/2026-10-12-opencode-zen-dogfooding.md`),
  so another provider may use another name.
- A tool call's `arguments` that were not valid JSON are sent back exactly as received; a
  repeated or missing provider call id is renamed/made up so the history stays unique.
- `total` is taken from the provider or left `None` — never `input + output` (ADR 0007 J2).

## What it guarantees (each with the test that holds it)

The **adapter-neutral contract** suite runs first: `tests/contract.rs` implements
`agent_provider_contract::ContractHarness` for this adapter and delegates to twelve checks. It
declares **no** escape hatches — this adapter claims every reason and every usage field.

| Check | Test | File |
|---|---|---|
| 1 plain text; 2 streamed deltas in order; 3a/3b one and several tool calls; 4 split arguments rejoined; 5 call ids unique; 6 finish reasons; 7a usage shapes; 7b `total` not computed; 8 error classification; 9 cancellation; 10 secret not leaked | `contract_1_plain_text` … `contract_10_secret_not_leaked` | `tests/contract.rs` |

Wire-level behaviour, over a real HTTP mock (`tests/http.rs`) and in unit tests:

| Guarantee | Test | File |
|---|---|---|
| A turn sends the expected request (path, headers, body) and returns the text | `a_turn_sends_the_expected_request_and_returns_the_text` | `tests/http.rs` |
| A base URL with a trailing slash or no path works | `a_base_url_with_a_trailing_slash_or_no_path_works` | `tests/http.rs` |
| A response split into tiny pieces is read the same | `a_response_split_into_tiny_pieces_is_read_the_same` | `tests/http.rs` |
| A tool call round-trips through the core | `a_tool_call_round_trips_through_the_core` | `tests/http.rs` |
| `reasoning_content` streams as reasoning, not text | `reasoning_content_streams_as_reasoning_not_text` | `tests/http.rs` |
| A server that reuses tool call ids does not break the conversation | `a_server_that_reuses_tool_call_ids_does_not_break_the_conversation` | `tests/http.rs` |
| HTTP errors are classified and never show the key | `http_errors_are_classified_and_never_show_the_key` | `tests/http.rs` |
| A cut connection, and a stream that ends before finishing, give an error and **no** partial answer | `a_cut_connection_gives_an_error_and_no_partial_answer`, `a_stream_that_ends_before_finishing_is_an_error` | `tests/http.rs` |
| A silent server hits the idle timeout | `a_silent_server_hits_the_idle_timeout` | `tests/http.rs` |
| Cancelling stops a request waiting on the server, and a directly-called model honours the token | `cancelling_stops_a_request_that_is_waiting_on_the_server`, `a_model_called_directly_honours_the_cancellation_token` | `tests/http.rs` |
| A response over the size limit is refused | `a_response_over_the_size_limit_is_refused` | `tests/http.rs` |
| A redirect is not followed, so the key stays put | `a_redirect_is_not_followed_so_the_key_stays_put` | `tests/http.rs` |
| A connection error is a transport error without the URL | `a_connection_error_is_a_transport_error_without_the_url` | `tests/http.rs` |
| A trailing usage chunk is captured through the SSE stream, and the last non-empty usage wins | `a_trailing_usage_chunk_is_captured_through_the_sse_stream`, `usage_in_every_chunk_takes_the_last_non_empty_one` | `tests/http.rs` |
| The user agent names radian | `the_user_agent_names_radian` | `tests/http.rs` |
| A response without usage reports `None`; `total` is not invented when the provider omits it | `a_response_without_usage_reports_none`, `total_is_not_invented_when_the_provider_omits_it` | `tests/http.rs` |
| `include_usage` defaults to the OpenAI host only; the body carries `stream_options` only when enabled; a 400 gets the hint and a 400 without it does not | `include_usage_defaults_to_the_openai_host_only`, `the_request_body_carries_stream_options_only_when_enabled`, `a_400_for_stream_options_tells_you_to_turn_it_off`, `a_400_without_stream_options_gets_no_hint` | `tests/http.rs` |
| A key is never sent over plain http to a remote host; bad configuration is reported; the key is hidden from `Debug` | `a_key_is_never_sent_over_plain_http_to_a_remote_host`, `bad_configuration_is_reported`, `the_key_is_hidden_from_debug_output` | `tests/http.rs` |
| An HTTP error body, a stream error and a custom header value that echo the key are masked; `Debug` hides the key and header values; URLs never leak credentials through `Debug` or config errors | `an_http_error_body_that_echoes_the_key_is_masked`, `a_stream_error_that_echoes_the_key_is_masked`, `a_custom_header_value_echoed_by_the_server_is_masked`, `debug_output_hides_the_key_and_header_values`, `urls_never_leak_credentials_through_debug_or_config_errors` | `tests/http.rs` |
| SSE parsing: events split at blank lines; any split point gives the same events; a multi-byte character split across chunks survives; a bare `\r` ends a line; a final event without a blank line is still returned; other fields and comments are ignored; invalid UTF-8 is a protocol error; an endless line is cut off | `events_are_split_at_blank_lines`, `a_split_anywhere_gives_the_same_events`, `a_multibyte_character_split_across_chunks_survives`, `a_bare_carriage_return_ends_a_line`, `the_final_event_without_a_blank_line_is_still_returned`, `other_fields_and_comments_are_ignored`, `invalid_utf8_is_a_protocol_error`, `an_endless_line_is_cut_off` | `src/sse.rs` |
| Assembly: text deltas become one part and stream as events; fragments join by index; servers that omit the index still work; missing ids are made up and repeated ids renamed; empty arguments mean `{}` and broken ones stay text; finish reasons map; a refusal is a refusal; a stream with no finish reason is an error; `[DONE]` without one is inferred; an error chunk ends the response; garbage is a protocol error; usage-only chunks carry usage; the dogfooded `completion_tokens_details` shape is kept; `total` is never invented; an over-limit response is cut off | `text_deltas_become_one_part_and_stream_as_events`, `tool_call_fragments_are_joined_by_index`, `servers_that_omit_the_index_still_work`, `missing_ids_are_made_up_and_repeated_ids_are_renamed`, `empty_arguments_mean_none_and_broken_ones_are_kept_as_text`, `finish_reasons_map_to_stop_reasons`, `a_refusal_is_a_refusal_and_its_text_is_kept`, `a_stream_with_no_finish_reason_is_an_error_not_a_partial_answer`, `done_without_a_finish_reason_is_inferred`, `an_error_chunk_ends_the_response`, `garbage_is_a_protocol_error`, `usage_only_chunks_carry_the_usage`, `usage_in_every_chunk_takes_the_last_non_empty_one`, `usage_maps_openai_and_anthropic_field_names`, `the_dogfooded_usage_shape_keeps_completion_tokens_details`, `total_is_never_invented_from_input_plus_output`, `a_response_over_the_limit_is_cut_off` | `src/stream.rs` |
| Request building: host `meta` and `reasoning` are not sent; a tool conversation maps to the expected messages; interrupted assistants are plain; no tools means no `tools` key; `stream_options` only when enabled and always reserved; arguments that were not JSON go back as received; a tool error is marked for the model; images and audio are sent where the API takes them; what the API cannot take becomes a placeholder; settings override the config and reserved keys are refused | `host_message_meta_is_not_sent_to_the_provider`, `reasoning_is_not_sent_to_the_provider`, `a_tool_conversation_maps_to_the_expected_messages`, `interrupted_assistants_are_sent_as_plain_assistants`, `no_tools_means_no_tools_key`, `stream_options_carries_include_usage_only_when_enabled`, `stream_options_is_reserved_like_stream`, `arguments_that_were_not_json_go_back_as_received`, `a_tool_error_is_marked_for_the_model`, `images_and_audio_are_sent_where_the_api_takes_them`, `what_the_api_cannot_take_becomes_a_placeholder_in_text`, `settings_override_the_config_and_reserved_keys_are_refused` | `src/request.rs` |

Mutation records for the usage behaviour are in `agent-core/MUTATIONS-0007.md` (M-D1…M-D12, all
caught; the usage slice was driven from this adapter).

## The `testing` feature

`testing` exposes an in-process mock server for this crate's tests and for dependent crates:
`MockServer::start(Vec<Reply>)`, `Reply` (`Sse`, `SseThenHang`, `SseThenCut`, `Status`), and the
helpers `Reply::sse`, `Reply::status`, `Reply::sse_split`, `chunk(delta, finish_reason)`,
`RecordedRequest` (`header`, `json`), `MockServer::requests`. It binds `127.0.0.1:0` and is
compiled only under the feature (ADR 0013: no test server ships in the product).

## Known limits and what is not verified

- **Lossy by design**: resource links, embedded resources and opaque blocks become short text
  placeholders; an assistant turn's `reasoning` is not sent; tool arguments the model produced
  badly are sent back as the raw string rather than repaired.
- **`reasoning_content` is verified on one endpoint only** (see above). Another compatible
  server may use another field name, and then its reasoning is simply not seen.
- **`include_usage` is a heuristic.** The default is "on for `api.openai.com`, off elsewhere",
  compared by exact host; a compatible server that supports `stream_options` but is not OpenAI
  gets no usage unless the host sets `include_usage` explicitly. A 400 is given a hint, not a
  retry.
- **No total time limit.** A response may take as long as bytes keep arriving; `idle_timeout`
  only bounds silence. `max_response_bytes` bounds text and tool-call arguments, not the raw
  bytes off the wire.
- **`[DONE]` without a finish reason is inferred** as `stop` (or `tool_calls` when calls are
  present). That is an inference, not something the provider said.
- **Error classification is by status, with one narrow exception**: 5xx/408/429 →
  `Transport`, other non-2xx → `Protocol`. A provider that signals a request error with a 500
  is classified as transport. The exception is ADR 0018's **context overflow**: a 400/413 whose
  JSON carries `error.code == "context_length_exceeded"` or a context-shaped `error.message`
  (or, mid-stream, the same code/message) becomes `ContextOverflow`. 429/5xx are never an
  overflow, and generic "too many tokens" wording is deliberately excluded (it is as likely to
  be a rate limit).
- **The two adapters are not identical**, and this is intentional: the Anthropic one has no
  `stream_options`, no `include_usage`, and its own usage field names. The shared contract suite
  is what keeps them comparable.
- **Not verified:** any real endpoint (every test runs against the in-process mock), and any
  server behaviour the mock cannot produce (e.g. HTTP/2 push, chunked trailers).
- `agent-provider-contract`'s review notes record one design gap this adapter caused: the
  contract's `pause_turn` fixture was added here even though `pause_turn` is not an OpenAI
  `finish_reason`, because the contract then had no "this provider has no such concept" hatch.

## Related ADRs

0003 (interrupted streaming prefix — this adapter's deltas are what the core accumulates) · 0005
G4 (`meta` is provider-invisible) · 0007 (usage: `include_usage`, the three SSE shapes, and
"never compute `total`") · 0010 (deltas are live projections) · 0013 (T1/T4: no telemetry, and an
endpoint named in an error is redacted) · 0014 (reasoning content). The contract itself and its
escape hatches are documented in `docs/provider-contract.md`.
