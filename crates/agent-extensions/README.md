# agent-extensions

MCP stdio servers exposed as `agent-core` tools. `McpToolProvider` implements
`agent_core::ToolProvider`, so an `AgentSession` gets one set of server processes of its own;
`McpSession` implements `ToolSession` and lives for the whole conversation, so server-side
state (a browser, a database handle, a counter) survives turns.

It does **not** own: the transport (that is `rmcp`, pinned to `=3.5.1`), the agent loop, the
session file, permissions or sandboxes. It also does not decide what a host mounts — that is
`radian-host`'s `HostTools`.

## Public API

| Item | What it is |
|---|---|
| `McpToolProvider::new(Vec<McpServerConfig>)` | The provider. `configured()` is how many servers are configured (known before a session opens); `status() -> Option<McpStatus>` reports what the **last opened** session actually connected (`servers`, `tools` counts only — no identities). |
| `McpSession` | One session's servers and their tool specs. `close()` closes each server's stdin, waits `shutdown_timeout`, kills, then reaps. |
| `McpServerConfig` | `id`, `command`, `args`, `cwd`, `env`, `inherit_env`, `shared_tools`, `trust_read_only_hint`, `call_timeout` (60 s), `cancel_grace` (2 s), `startup_timeout` (30 s), `shutdown_timeout` (5 s). Builder: `new(id, command)`, `arg`, `env`. |
| `tool_alias(server_id, tool_name)` / `MAX_ALIAS_LEN` | The model-facing name. `server__tool`, sanitized to `[A-Za-z0-9_-]`; only when sanitizing changed something or the name is too long (over 64) does it cut the readable part and append an 8-hex-digit SHA-256 of the original pair. |
| `McpStatus` | `servers`, `tools`. |

## What it guarantees (each with the test that holds it)

| Guarantee | Test | File |
|---|---|---|
| A tool call round-trips through the core on **both** protocol lifecycles (`server/discover` 2026-07-28 and `initialize` 2025-11-25) | `tool_call_round_trips_through_core_on_both_protocol_versions` | `tests/mcp.rs` |
| An alias is readable, valid for the providers, and depends only on `(server_id, tool_name)` — so adding or reordering other tools never renames it and old transcripts stay resolvable | `aliases_are_readable_valid_and_stable` | `tests/mcp.rs` |
| A host sees what actually connected, not what was configured | `the_provider_reports_what_the_opened_session_connected` | `tests/mcp.rs` |
| Two tools mapping to one alias fail the open, and the servers already started are reaped | `alias_collision_fails_open_and_reaps_servers` | `tests/mcp.rs` |
| Concurrency comes from config (`shared_tools`, `trust_read_only_hint`), never from a server's own `readOnlyHint` claim | `concurrency_comes_from_config_not_server_claims` | `tests/mcp.rs` |
| The server environment is allowlisted: the child is started with an empty environment, plus `PATH` and the `inherit_env` names | `environment_is_allowlisted` | `tests/mcp.rs` |
| A server's `roots`/sampling/elicitation request is refused at once with method-not-found, so a server can never block waiting on the host | `server_requests_for_roots_are_refused_without_hanging` | `tests/mcp.rs` |
| Server state survives turns but not sessions (a new `AgentSession` re-opens its servers) | `server_state_survives_turns_but_not_sessions` | `tests/mcp.rs` |
| `replace_transcript` keeps the MCP process and its state — the tool session is never reopened | `replacing_a_transcript_keeps_the_mcp_process_and_its_state` | `tests/mcp.rs` |
| A cancelled call notifies the server and then stops it; a server that still answers `ping` is stopped too | `cancelled_call_notifies_then_stops_the_server`, `timeout_stops_even_a_server_that_still_responds` | `tests/mcp.rs` |
| A timeout on a frozen server kills it and marks it unavailable for the rest of the session (no restart) | `timeout_on_a_frozen_server_kills_it_and_marks_it_unavailable` | `tests/mcp.rs` |
| Dropping an in-flight call kills the server | `dropping_an_in_flight_call_kills_the_server` | `tests/mcp.rs` |
| A tool error and rich content (image/resource blocks) map into `ToolFailure`/`ContentPart` correctly | `tool_errors_and_rich_content` | `tests/mcp.rs` |
| Progress notifications become `AgentEvent::ToolProgress`; a flood keeps the queue bounded and the newest value; non-monotone values are dropped; late or unknown tokens are ignored; progress does not reset the call deadline; a settled call leaves no route behind | `progress_maps_to_tool_progress`, `a_progress_flood_still_delivers_the_latest_value`, `settled_progress_arrives_in_order`, `non_monotone_progress_is_dropped_on_the_call_path`, `concurrent_progress_calls_stay_separate`, `late_and_unknown_progress_tokens_are_ignored`, `progress_does_not_reset_the_call_deadline`, `settled_calls_leave_no_progress_registration_behind` | `tests/mcp.rs` |
| `close()` reaps the server **and its children**; dropping the session kills the process tree; a failed startup reaps the servers already started; a server crash kills its children at once | `close_reaps_the_server_and_its_children`, `dropping_the_session_kills_the_process_tree`, `failed_startup_reaps_servers_already_started`, `server_crash_kills_its_children_at_once` | `tests/mcp.rs` |
| The reaper thread is created **before** the child, and a child whose hand-over fails is stopped and reaped here | `no_child_is_started_when_the_reaper_thread_cannot_be_created`, `a_child_is_stopped_and_reaped_when_the_reaper_never_runs` | `src/process.rs` (unit tests) |
| Aliases: plain names stay readable; invalid characters get the fixed suffix and stay distinct; long names are cut to `MAX_ALIAS_LEN`; the alias is deterministic | `plain_names_stay_readable`, `invalid_characters_get_a_fixed_suffix`, `long_names_are_cut_to_the_limit`, `alias_is_deterministic` | `src/alias.rs` (unit tests) |
| Progress routing: live routes get their notifications; notifications that arrive before registration replay in order; the orphan buffer is bounded and drops the oldest; `unregister` stops delivery; a flood keeps the newest; `recv` never misses a push | `live_routes_receive_their_notifications`, `orphaned_notifications_replay_in_order_at_registration`, `the_orphan_buffer_is_bounded_and_drops_the_oldest`, `unregister_stops_delivery`, `a_flood_keeps_the_queue_bounded_and_keeps_the_newest`, `recv_waits_for_a_push_and_a_push_is_never_missed` | `src/client.rs` (unit tests) |
| Out-of-order notifications are filtered to a monotone sequence, and routed progress reaches emission in registration order | `out_of_order_progress_is_dropped_to_a_monotone_sequence`, `routed_progress_reaches_emission_in_registration_order` | `src/session.rs` (unit tests) |

## Known limits and what is not verified

- **No restart, by design.** A server that times out, is cancelled, crashes or answers with a
  protocol error is stopped and marked `extension_unavailable` for the rest of the session; its
  tools stay in the tool list and keep returning that failure. Only a new `AgentSession`
  re-opens it.
- **Cancellation is not "abort the tool".** A cancelled or timed-out call gets
  `cancel_grace` to write the notification and be seen to have read it, then the whole server is
  stopped. A server still working in the background cannot be resumed — that is the point
  (stateful servers would otherwise interleave abandoned work with the next call).
- **`ServerResult` kinds that are not `CallToolResult` are refused**, not emulated:
  `InputRequiredResult` and `CreateTaskResult` become `extension_unsupported_interaction`, any
  other result kind becomes `extension_protocol_error`. Elicitation, sampling and roots are
  answered with method-not-found (see the `roots` guarantee above) — so a server that needs
  them is unusable here.
- **Progress order is restored per call, but tail loss is accepted**: `rmcp` delivers each
  notification on its own task, so a value can arrive after the response was taken; the queue is
  polled before the response to trim that window, not to close it.
- **Bounded buffers, oldest dropped**: 64 progress notifications per call
  (`PROGRESS_QUEUE_LIMIT`) and 256 unrouted notifications (`ORPHAN_LIMIT`).
- **Unix process groups.** `kill_tree` uses `killpg`; on Windows it falls back to
  `taskkill /T`, which follows parent links and therefore cannot reach the children of a leader
  that has already exited (a Job Object is planned but not implemented). The
  `waitid(WNOWAIT)` dance that keeps the pid reserved while the group is signalled is
  Linux/macOS only.
- `McpSession::progress_route_count()` is `#[doc(hidden)]` and exists only so integration tests
  can prove a settled call leaves no route behind.
- Not verified here: any MCP transport other than stdio, and any server behaviour the
  hand-written `mcp-fixture` (a `[[bin]]` in this crate, used only by the tests) does not
  produce.

## Related ADRs

0006 §5 (the events a session records; MCP lifecycle is deliberately **not** recorded in v1,
· 0010 K4 (`McpServerLifecycle` is the one runtime event v1 does not
write) · 0012 (MCP subprocesses are **not** sandboxed in this build — the startup notice says
so) · 0013 T1/T3 (an MCP server is a user-configured child process; Radian does not police its
network access).
