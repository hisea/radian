# `agent-provider-openai-responses` mutation record

Each mutation was applied to the working tree, the whole crate suite was run with
`--no-fail-fast`, and the source was restored by a reverse edit (no `git checkout --`). The
mutations the brief asked for are M1–M4; the test that caught each is named.

| # | Mutation | Result | Test(s) that caught it |
|---|---|---|---|
| M1 | `store: false` → `store: true` in the request body (`request.rs`) | caught (FAILED) | `request::tests::the_body_carries_the_fixed_responses_keys`; `the_request_body_and_headers_are_the_responses_shape` (http) |
| M2 | drop the reasoning replay (`replay_reasoning` never pushes the item) | caught (FAILED) | `request::tests::the_providers_reasoning_item_is_replayed_verbatim`; `an_encrypted_reasoning_item_round_trips_into_the_next_request` (http) |
| M3 | drop `response.function_call_arguments.delta` (arguments from deltas lost) | caught (FAILED) | `stream::tests::tool_call_arguments_split_across_deltas_rejoin`; `contract_4_split_arguments`; `tool_call_arguments_split_across_deltas_rejoin_over_the_wire` (http) |
| M4 | `429` removed from the transient classification (`408 \| 429` → `408`) | caught (FAILED) | `a_429_is_transient_and_carries_the_retry_after` (http) |

## Note on M3

The first attempt at M3 was caught only by the contract and http tests: the unit test
`tool_call_arguments_split_across_deltas_rejoin` also sent an authoritative
`response.function_call_arguments.done`, so the assembly path was not what it exercised. The
unit test was changed to send **only** deltas (the `done` event removed), so the named unit test
now catches M3 too. A test named for a behavior must fail when that behavior breaks.
