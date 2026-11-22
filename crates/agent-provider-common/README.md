# agent-provider-common

The code the three provider adapters share (ADR 0026 D12): the SSE reader (`SseParser`), the
`Retry-After` parser (`retry_after`), the usage-limit body recognizers (`usage_limit::go` for
opencode-go, `usage_limit::chatgpt` for the ChatGPT subscription), and, behind the `testing`
feature, the mock HTTP server (`testing::MockServer`, `Reply`, `Barrier`) that each adapter's own
`testing` module wraps with its wire event builders.

Wire-specific code stays in the adapters. The crate changes no adapter behavior; the three
provider contract suites (`agent-provider-contract/README.md`) stay green and unchanged.
Mutation checks are in `MUTATIONS.md`.

## Behaviour and the tests that check it

| Behaviour | Test | File |
|---|---|---|
| Events split at blank lines; any chunking gives the same events; a multibyte character split across chunks survives; a bare `\r` ends a line | `events_are_split_at_blank_lines`, `a_split_anywhere_gives_the_same_events`, `a_multibyte_character_split_across_chunks_survives`, `a_bare_carriage_return_ends_a_line` | `src/sse.rs` |
| The final event without a blank line is still returned; other fields and comments are ignored | `the_final_event_without_a_blank_line_is_still_returned`, `other_fields_and_comments_are_ignored` | `src/sse.rs` |
| Invalid UTF-8 is a protocol error; an endless line is cut off | `invalid_utf8_is_a_protocol_error`, `an_endless_line_is_cut_off` | `src/sse.rs` |
| `Retry-After` takes seconds or an IMF-fixdate; an unparseable value is ignored | `retry_after_seconds_are_taken_as_is`, `retry_after_accepts_an_imf_fixdate`, `an_unparseable_retry_after_is_ignored`, `days_from_civil_matches_known_epochs` | `src/retry_after.rs` |
| The opencode-go usage-limit body names the message and window; a `Retry-After` header wins over the body; other statuses or bodies are not a usage limit | `the_go_usage_limit_body_names_the_message_and_the_window`, `a_retry_after_header_is_the_reset_wait_and_wins_over_the_body`, `another_status_type_or_body_is_not_the_go_usage_limit` | `src/usage_limit/go.rs` |
| The ChatGPT usage-limit body names the plan, window and reset; optional fields may be absent; a sub-minute countdown is not zero minutes; other types and statuses are not a usage limit | `the_usage_limit_body_names_the_plan_the_window_and_the_reset`, `a_body_without_the_optional_fields_still_names_the_limit`, `a_second_countdown_under_a_minute_is_not_zero_minutes`, `another_types_and_statuses_are_not_the_usage_limit` | `src/usage_limit/chatgpt.rs` |

The mock server (`testing::MockServer`) has no tests of its own: every adapter's HTTP tests and
the contract suites run on it.
