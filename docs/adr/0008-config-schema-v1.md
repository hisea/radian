# ADR 0008: Configuration Schema v1 and Migration Policy

Status: accepted

## 1. Content and location

The configuration file is host-static input. It covers: provider/model routing (`provider`, `base_url`, `model`, adapter switches), secret references, the MCP server list, permission tiers and approval policy, TUI preferences (theme, colors, input box, footer, widgets, display), Skills (`extra_paths`, `builtin`, `disabled`), sandbox Unix socket allowlists and environment allowlists, `[integrations] herdr`, named `[providers.<name>]` providers, `[model] retries`, `[model.smol]` (cheap-model slot; only the `explore` subagent reads it today), `[compaction] auto|thinking`, `[memory.injection]`, `[control]`, `[hooks]` (user-level external command hooks, ADR 0045 §1; `[hooks] intent_stop` is the built-in validator switch, on by default). `[shell]` holds no effective key. An old `auto_background_secs` warns one line and is ignored (ADR 0037 decision 2).

- **User-level file**: `$RADIAN_HOME/config/config.toml` (default `~/.radian/config/config.toml`). `--config <path>` and `RADIAN_CONFIG` override it. ADR 0020 defines the root directory. No XDG or system data/cache directory branches exist.
- **Project-level file**: `<workspace>/.radian/config.toml`. The file itself counts as a trust trigger (ADR 0031), but its only effective key `[permissions] profile = "workspace"` is a **narrow-only** request, so the narrowing **always applies** with or without trust. `full` is a configuration error: warn and ignore. `[sandbox]` keys (`allow_env`, `allow_unix_sockets`) and all other keys are rejected or skipped by name with the name stated. Project configuration can never widen permissions, and no content loads only when trusted.
- The configuration file is **not** part of the session log (ADR 0006). The session log carries only references to profile content.
- The authority for the runtime change sequence is the session log `ConfigChanged.revision` (ADR 0004). The configuration file carries only `schema_version`, never `revision`.

## 2. First start, default configuration, and model selection

- The configuration file is created when it is missing at the **default location**: `radian` and `radian exec` create it at startup when the resolved path is the default path (`<RADIAN_HOME>/config/config.toml`) and the file does not exist. They create it with directory mode 0700 and file mode 0600, and the startup line states once `created a default config at <path>`. The template holds only `schema_version = 1` as an effective key. All other common keys (`[model]` provider/model, `[permissions]` profile, `[tui]` theme, `[compaction]` auto/thinking) stay comments, and parsing equals an empty configuration. An existing file is never touched by one byte. A failed creation warns once and continues normally. An explicit `--config`/`RADIAN_CONFIG` path that does not exist creates **nothing** (a typo must not leave a file). Missing-file behavior stays as before, plus one line in §5.
- Without a model, the TUI does not exit: the session opens normally with no model selected (the footer shows `no model`, the input placeholder and startup block point at `/model`, and at `/login` when needed). A submitted message calls no model and starts no turn. It shows one hint line and keeps the input draft. `/model`, `/login`, `/theme`, and `/help` still work normally. `radian exec` still reports no model and exits non-zero, but the message names the configuration file path (new or existing) and the `--provider <p> --model <m>` form.

## 3. Secrets: references only

### 3.1 Reference shape

Any secret slot value must be a table object:

```toml
api_key = { from = "env", name = "OPENAI_API_KEY" }
```

`from` takes `"env"` (the only currently supported source), `"keychain"`, or `"file"`. `name` is a non-empty string. The reader passes only the **reference** to the host. The host resolves the secret at the injection point. The only runtime source for keys is `auth.json` (ADR 0026 D2). `model.api_key` is a removed spelling: its presence is an error, and the error points at `/login`. The `keychain`/`file` sources are explicitly rejected at present.

### 3.2 Plaintext secrets must fail

The reader must **fail and refuse to open** in two cases:

1. A sensitive key (`api_key`/`apikey`/`token`/`secret`/`password`/`passwd`/`authorization`, case-insensitive) holds a bare string instead of a `SecretRef` table.
2. An obvious secret-shaped string appears anywhere (prefixes `sk-`, `sk_`, `xoxb-`, `ghp_`, `AKIA`, `Bearer `, or a long random string of 40 or more characters outside `env: NAME`).

This check is a heuristic, not a security boundary. The true boundary is "plaintext values are always forbidden" plus host log redaction. The error names only the key path, never the value.

## 4. `schema_version` and migration

- Top-level required field `schema_version = 1` (integer, not boolean). A missing or non-integer value is an error. The version belongs to the whole file. No "part of the configuration is new version" state exists.
- A reader that meets a **higher** version either interprets read-only plus one warning line (unknown keys ignored) or refuses. A writer always refuses higher versions.
- A reader that meets a **lower** version refuses (no v0→v1 migration path exists). It never silently rewrites. Migration starts only through an explicit entry point: first backup or write a new path, then verify the result through write verification. Without a migration path, the error names the lowest supported version.
- **Unknown fields**: the interpreter ignores them (a skipped log is allowed). Write-back must preserve them exactly or refuse. Use a format-preserving library such as `toml_edit` and change only known keys. When preservation is impossible, refuse the write-back and report "configuration holds unknown field X; safe write-back is impossible". "Ignore at interpret time plus drop at write time" is forbidden. Unknown **enum values**: security-related keys (`permissions`) fail closed (refuse or take the most conservative meaning). Display-related keys (`tui.*`) ignore plus warn.
- **Atomic write**: unique-name temporary file plus rename, with original file permissions kept. After write, reparse and confirm equivalence with the pre-write state except for the target keys. Otherwise abandon the write and report the cause.

## 5. Write-back entries: `/theme` and first model selection

- `/theme`: changes only `tui.theme` through `toml_edit` and keeps all other bytes unchanged. It writes the file actually read this time. When the file does not exist, it creates it (`schema_version = 1` plus `[tui] theme`). When `schema_version` is above support, it does **not** write. The change stays effective for this session only, with the cause stated.
- First model selection: in a session started **without a model**, when the user first selects a route through `/model`, the selection writes `[model] provider` and `[model] model` back to the configuration file (creates `[model]` when missing). All other bytes stay unchanged. Rules match `/theme` (including read-only refusal). A failed write shows only one line, and the session still uses the selected model this time. Each session writes at most once. `/model` switches in a session that already has a model never write the file.

`/login` does not write configuration. `/login` writes `auth.json` atomically and independently. Other `/model` selections only advance the journal `ConfigChanged` (ADR 0026 D5) and never reach the configuration file.

## 6. Parse rules

- `schema_version` must exist and must be a non-boolean integer.
- `permissions.profile` knows two tier names (`workspace`/`full`). Old `ask`/`allow`/`deny` names are recognized as historical profile names and handled as unknown values (skip and name them). Unknown tier names fail closed.
- Unknown `permissions.approval` values fail closed. `auto` under the `full` tier warns and is ignored. Command-line `--approval`/`--yes` always win.
- `[model] retries` is a non-negative integer. A negative or non-integer value is rejected. `[shell]` holds no effective key. An old `auto_background_secs` (any value) warns one line and is ignored. It is never an error. `[compaction] thinking` is one of seven levels or `"current"`. An unknown tier name is a configuration error.
- `[tui] bell` is `"off" | "attention" | "all"` (default `off`; terminal bell). An unknown value is a display-class key: ignore and warn, never refuse.
- `[providers.<name>]` names must not collide with built-in descriptors or `openai-compatible`. `kind` allows only `openai-compatible` and `anthropic`.
- `[model.smol]` `provider` and `model` carry the same meaning as the `[model]` route keys. It is the cheap-model slot. Only `explore` reads it today. Only one of the two set is a configuration warning and ignored (`explore` follows the parent). A project-level occurrence is ignored as an unknown key with the name stated.
- `[sandbox] allow_unix_sockets` and `[sandbox] allow_env` belong only in the user-level configuration. A project-level occurrence is a configuration error.
- `[hooks]` (ADR 0045 §1): three event names (`PreToolUse`/`PostToolUse`/`Stop`; each is a `{ matcher, hooks }` entry list; each hook `type` must be `"command"`, with optional `async` and `asyncRewake` booleans on `PostToolUse` and `Stop` only) plus one switch (`[hooks] intent_stop`, boolean, on by default: `false` turns off the built-in validator; a user `Stop` hook still runs). Outside the subset (unknown event names, `type` other than `command`, keys `if`/`http`/`prompt`/`agent`, `async`/`asyncRewake` on `PreToolUse` or `PermissionRequest`, incomplete entries, bad `timeout`, bad `intent_stop`), each entry is skipped one by one and logged as `skipped`; an async field on the two events that do not accept it drops that hook alone. It never refuses. `matcher` recognizes only Radian tool names (separated by `|`). A misspelling is not checked here (it simply never matches at runtime).

## 7. Fixtures and verification

`docs/schema/config/*.toml` gives one sample per scenario, `docs/schema/config/README.md` is the human-readable table, and `docs/schema/check-config-fixtures.sh` `EXPECT` is the script authority. Both stay in sync. `crates/radian-host/tests/config_fixtures.rs` uses the same table. Reader unit tests and `crates/radian-tui/tests/config.rs` cover themes and key bindings. `/theme` write-back tests cover preservation and refusal.

## 8. MCP server list: three sources, one representation

The MCP server list is host-static input from three sources: `--mcp-config <FILE>`, the `[mcp]` section in the user configuration file, and the project root `.mcp.json` (read only when trusted, ADR 0031). All three sources accept the same two forms, and loading normalizes them into the single internal `McpServerConfig` (one normalize function: `[mcp]` TOML values convert to JSON and then follow the same file path, so the rules cannot fork):

- Own form `{"servers": [...]}` (`[[mcp.servers]]` in TOML): `id`, `command`, `args`, `cwd`, `env`, `inherit_env`, `shared_tools`, `call_timeout_secs`, `shutdown_timeout_secs`. Unknown fields are rejected (`deny_unknown_fields`). `inherit_env` defaults to empty; `cwd` defaults to unset (Radian's working directory).
- Shared form `{"mcpServers": {"<name>": {...}}}` (`[mcp.mcpServers.<name>]` tables in TOML): the key is the server id. `command`/`args`/`cwd`/`env` mean the same. Entries with `type` `http`/`sse`/`ws`, or with a `url`, are skipped (one startup warning line; the session still starts). Only stdio runs (`type: stdio` or no `type`). The four Radian-own keys (`inherit_env`, `shared_tools`, `call_timeout_secs`, `shutdown_timeout_secs`) are accepted in both forms. Other foreign keys (`disabled`, `autoApprove`, `timeout`, and similar) are ignored with one warning line per entry. `mcpServers` entries default `inherit_env = ["HOME"]`. An explicitly written value wins. The default is verified with a real `npx`-started server (`@modelcontextprotocol/server-filesystem`; `list_allowed_directories` returns normally).
- Both keys present, or neither key present, is a load error (both tables present, or both absent, under `[mcp]` is a configuration error). A duplicate id fails before any process starts.
- `command`, `args`, `cwd`, and `env` values expand `${VAR}` and `${VAR:-default}` at load time (Claude Code semantics). An unset variable without a default expands to empty plus one warning line.
- `cwd` (optional string, default unset) is the server's working directory. A leading `~/` (or `~` alone) expands to the home directory. A relative path resolves against the directory of the file that declares the entry (`.mcp.json` → its directory, `config.toml` → its directory, `--mcp-config` → that file's directory); an absolute path stands. A path that is not an existing directory skips that one server with one warning line naming the server and the path; the other servers load and the session opens. The server process runs there outside the sandbox, like any MCP server; `cwd` changes no tier, approval or sandbox rule.
- Merge order across the three sources: `--mcp-config` > `[mcp]` > `.mcp.json`. A later source never wins over an earlier one (a project file cannot shadow the user's own configuration). Each collision logs one warning line naming the id and the winner. Each warning prints under `[mcp] …`. The project-level `<workspace>/.radian/config.toml` `[mcp]` is never read (ignored with the name stated; no process starts).
- Project `.mcp.json` (an ADR 0031 T1 trigger resource): read only when the directory is trusted (or `--trust`). When untrusted, it is not read and starts no process. The startup line states the fact (`[mcp] project .mcp.json not loaded (untrusted): …`). `--no-config-mcp` (the `explore` subagent) skips it. Read servers merge with the two user sources under the same rules. A broken file warns and skips the whole file. A project file never breaks startup (same principle as the project-level `config.toml`). A file changed mid-session does not hot-load. It takes effect at the next start.
- Credential variables: a project entry that references a name in the fixed constant table expands to empty (in `command`, `args`, `cwd`, and `env` values alike, and the name leaves `inherit_env`). Each reference logs one warning line. User-level entries expand normally. The table holds key variable names for built-in providers (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENCODE_API_KEY`, `OPENCODE_GO_API_KEY`) plus common third-party keys (`DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `XAI_API_KEY`, `MISTRAL_API_KEY`). Matching uses no patterns. A new built-in provider updates the table together.
