# ADR 0013: No Telemetry by Default

Status: accepted

## Decisions

### T1: Radian sends no unsolicited telemetry

**Radian itself** (Core, runtime, TUI, CLI, `radian-session`) has **no** telemetry, no crash reporting, no update checks, no analytics endpoints, and no collection or upload path enabled by default. The default is not "implemented but off". These paths **do not exist**. Any new collection or upload path needs a new ADR. The default stays off.

This does not mean "no network by default": a `full` session has network (ADR 0016, ADR 0043). The following **explicit** network surfaces are product functions, not telemetry:

- User-configured **provider API requests** (model calls, login verification, provider model list refresh);
- **models.dev model catalog** (read-only `GET https://models.dev/api.json`, no key, no headers, no user data; 24-hour cache, 3-second timeout, `--offline-catalog` disables it);
- `/login` credential verification (read-only `GET <base>/models`, to the endpoint the user names in `/login`);
- Background refresh of provider model lists (read-only `GET <base>/models`, with that provider key; TTL 1 hour, retry 5 minutes after failure, never blocks startup; the cache stores only the key fingerprint);
- opencode-go usage interface (read-only `GET https://opencode.ai/zen/go/v1/usage`, key from `auth.json`; active only when the current provider is opencode-go);
- MCP child processes can use the network themselves (user-configured services; Radian does not manage their network behavior);
- Shell, `git`, and network runs from tool execution (they belong to the tool execution boundary, ADR 0012 and ADR 0016).

### T2: Usage and cost are local records

`ModelUsage` enters the journal and JSONL as local bookkeeping, not as a report. **A local record never equals authorized upload**. Any "export usage" function needs its own privacy decision. The default stays off.

### T3: Redaction for diagnostics and export

- Provider `Debug` and config errors use `redact_url` and `mask`: URL userinfo, query values, and fragments are cleared or masked. Every known secret in error text becomes `<redacted>`.
- Host errors, `--search` snippets, TUI panic logs, and `--export` pass the same full secret list: **every** parseable key in `auth.json` (including `$ENV` interpolation) plus every `--header` value.
- Export drops raw provider payloads by default. It includes them only when the user explicitly opts in for that export. Opt-in never disables redaction of API keys, Authorization, or custom sensitive headers.
- Export drops reasoning by default (ADR 0014). It includes reasoning only on opt-in.
- JSONL never stores API keys, Authorization or Cookie request headers, secret reference values, environment variable values, or `.netrc` and keychain content. `provider_payload` and `Opaque` keep provider-returned bytes verbatim and can carry model-echoed sensitive content. The host treats JSONL as potentially sensitive data.
- Honest boundary: model answer and reasoning text plus `--search` snippets are session content. The guarantee covers only **known key and header values**. It never claims to redact other secrets that the user pastes into the conversation.

### T4: Diagnostics land on disk

Panic logs live in `~/.radian/cache/diagnostics/` (`$RADIAN_HOME/cache/diagnostics`). Retention is 7 days. The total size cap is 50 MiB. Past the cap, the oldest files go first. Cleanup runs at startup and at write time. The directory mode is 0700. File mode is 0600. A diagnostic write failure **must never block** the agent. Session journals ignore cache cleanup rules (the journal is the authoritative fact source).

### T5: File permissions

| Path | On creation (Unix) |
|---|---|
| Session directory | `0700` |
| `.jsonl` | `0600` |
| `index.db` and WAL/SHM | `0600` |
| Panic log | `0600` |

New files use the mode at creation (`OpenOptionsExt::mode` and `DirBuilderExt::mode`), not "create then chmod". **Existing directories and files: warn only, never modify**. Permissions can be intentionally set by the user, and silent tightening would break them. When group or other permission bits exist at open time, the host prints one stderr warning with the path, the current mode, and a suggested `chmod`.

## Verification

- Network surfaces stay pinned by code review and tests: the models.dev catalog, `/login` verification, provider model list refresh, and opencode-go usage are the only added network paths. No telemetry, update-check, or crash-report module exists.
- Redaction tests: `a_key_is_never_sent_over_plain_http_to_a_remote_host`, `the_key_is_hidden_from_debug_output`, `urls_never_leak_credentials_through_debug_or_config_errors`, `a_custom_header_value_echoed_by_the_server_is_masked`, `a_stream_error_that_echoes_the_key_is_masked`, `pty_the_panic_log_redacts_every_auth_key_not_only_the_startup_one`, `search_redacts_the_key_from_a_snippet`, `a_runtime_error_redacts_every_auth_key_not_only_the_route_key`.
- Permission tests: `radian-session/tests/privacy.rs`, `radian-exec/tests/cli.rs` (new 0700 and 0600, wide-directory warn without change), `radian-tui/tests/pty.rs` (panic log 0600).
- Cleanup tests: `radian-host` `diagnostics` unit tests cover retention, the total cap, foreign files untouched, and write failure is not a panic.
