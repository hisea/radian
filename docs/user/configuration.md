---
description: Every configuration key, where the file lives, and how it is checked.
---

# Configuration

## Locations and precedence

Radian reads one user configuration file.
The first hit wins:

1. `--config <FILE>`.
2. The `RADIAN_CONFIG` environment variable.
3. `~/.radian/config/config.toml`, or `$RADIAN_HOME/config/config.toml` when `RADIAN_HOME` is set.

A missing file is an empty configuration.
Nothing creates it except the first start: on first start radian writes a commented template at the default path only.
An explicit `--config` or `RADIAN_CONFIG` path is never created; a typo there stays an error, not a new file.

A project may hold `<workspace>/.radian/config.toml`.
That file holds only `[permissions] profile = "workspace"`.
A value of `full` warns and has no effect: a project can only narrow the tier, never widen it.
`[sandbox]` keys in a project file are refused by name; other project keys are skipped and named.
Project skills and project servers load only when you trust the project.
Untrusted projects still contribute context files as data.
Pass `--trust` to trust one run, `--no-trust` to distrust one run; neither writes to disk.
Confirm the trust prompt once to save it.

The command line always wins over the file.
For the provider the full order is: `--provider`, then the resumed session route, then `model.provider`, then `openai-compatible`.
Startup prints `[config] <path|none>` and names ignored keys, never values.

## When a change applies

File keys apply at startup: change the file and start a new session.
Three settings also change inside a session through slash commands, and take effect at the next turn boundary, never mid-turn: the model route (`/model`), the thinking level (`/thinking`), and the approval answers (`/auto`).
The theme (`/theme`) applies at once and writes back to the file.

## `schema_version`

| Key | Type | Required | What it does |
|---|---|---|---|
| `schema_version` | integer | yes | `1` is current. A higher version reads read-only with a warning and is never written back. A lower version is refused: there is no migration path. |

```toml
schema_version = 1
```

## `[model]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `model.provider` | string | `openai-compatible` | a built-in or `[providers.<name>]` name | The endpoint name. The key is looked up under this name. |
| `model.base_url` | string | provider public endpoint | any URL | Everything before the provider path. Only for the `openai-compatible` provider; refused for other providers. |
| `model.model` | string | none (flag required) | any model id | The default model name. A bare name, never `provider/model`. |
| `model.headers` | table of strings | none | any names except `Authorization` | Extra request headers. Only for the `openai-compatible` provider; refused for other providers. A non-string value is skipped. `{session_id}` in a value expands per session. The flag wins on a shared name. |
| `model.context_window` | non-negative integer | catalog lookup | `0` means unset | Explicit context window in tokens. `0` behaves as absent. A negative or fractional value is refused. The flag `--context-window 0` is a usage error instead. |
| `model.retries` | non-negative integer | `3` | `0` disables retrying | How many times one model call retries a transient error. |
| `model.smol` | table | none | both keys or neither | The cheap route for the `explore` helper. A half-set table warns and is ignored. |
| `model.smol.provider` | string | parent route | any provider name | The cheap route provider. Both keys are required; a half-set table warns and is ignored. |
| `model.smol.model` | string | parent route | any model id | The cheap route model. See above. |
| `model.include_usage` | — | — | — | Recognized but ignored: `--include-usage` controls this. Named on the startup line. |

```toml
[model]
provider = "openai-compatible"
model = "gpt-5"
context_window = 200000
retries = 3

[model.headers]
x-tenant = "team"
```

## `[providers.<name>]`

Named endpoints. `model.provider = "<name>"` selects one.
The key is stored under the same name in `auth.json`, so each endpoint has its own slot.

| Key | Type | Required | Allowed | What it does |
|---|---|---|---|---|
| `providers.<name>.kind` | string | yes | `openai-compatible`, `anthropic` | The wire protocol. Anything else is refused. |
| `providers.<name>.base_url` | non-empty string | yes | any URL | The endpoint base. An empty value is refused. |
| `providers.<name>.reasoning_replay` | string or `false` | none | a field name, or `false` | The assistant-message field the model needs its reasoning replayed under. `false` turns replay off. `true` is refused. |

The name uses letters, digits, `-`, and `_` only, and must not shadow a built-in provider.
Unknown keys under a name are skipped and named.
See `providers` for login and routing.

```toml
[providers.office]
kind = "openai-compatible"
base_url = "https://models.office.example/v1"
```

## `[permissions]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `permissions.profile` | string | `full` | `workspace`, `full` | The permission tier. `workspace-write` and `full-access` stay accepted as aliases. The file aliases `ask`, `allow`, `deny` are ignored with a warning: they name no tier. Anything else is refused. |
| `permissions.approval` | string | `on-ask` | `on-ask`, `always`, `never`, `auto` | How an `ask` resolves. Unknown values are refused. `auto` in the file has no effect under `full`. |

The command line (`--profile`, `--approval`, `--yes`) always wins over this section.
See `permissions` for tiers, prompts, and the sandbox.

```toml
[permissions]
profile = "workspace"
approval = "on-ask"
```

## `[sandbox]`

Only the user file may set this section.
Names here pass through to sandboxed children; values never do.

| Key | Type | Default | What it does |
|---|---|---|---|
| `sandbox.allow_unix_sockets` | array of strings | none (empty list) | absolute paths only | Unix sockets the sandbox may connect to. Only absolute paths; `~` expands from home. A path that does not resolve is skipped with a warning. Ignored on Linux with a warning: the filter there denies every Unix socket. |
| `sandbox.allow_env` | array of strings | none beyond the fixed list | variable names only, no `=`, no whitespace, no value | Variable names passed to sandboxed children. Names only: no `=`, no whitespace, no value. A bad entry is refused. |

Sandboxed children otherwise start from a fixed list: `PATH`, `HOME`, `LANG`, `TERM`, `TMPDIR`, `USER`, `LOGNAME`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, and the six `HERDR_*` pane coordinates.
No key or token reaches a child unless you name its variable here.
An unsandboxed shell gets no such filtering, but the same list still gates what the model-run command inherits.

```toml
[sandbox]
allow_unix_sockets = ["~/.run/agent.sock"]
allow_env = ["MY_TOKEN_NAME"]
```

## `[hooks]`

User-level command hooks: three events, each a list of `{ matcher, hooks }` entries.
A matcher is a regular expression over Radian tool names, anchored to the whole name (`shell|write` names two tools); `*`, an empty matcher, or a missing matcher matches every tool.
Each hook is `{ type = "command", command, timeout }`.
The command runs as `/bin/sh -c` inside the session sandbox when one is enforced; `timeout` is seconds (`60` when unset or `0`).
`PreToolUse` exit `2` denies the call; other failures fail closed.

| Key | Type | Default | What it does |
|---|---|---|---|
| `hooks.PreToolUse` | array of entries | none | Gate a tool call before it runs. |
| `hooks.PostToolUse` | array of entries | none | React after a tool call. |
| `hooks.Stop` | array of entries | none | React at stop. The matcher is ignored. |
| `hooks.intent_stop` | boolean | `true` | The built-in stop validator. `false` turns it off; user `Stop` hooks still run. |

Anything outside this shape is skipped with one line, never refused: unknown event names, a `type` other than `"command"`, non-command handler keys, an empty command, a bad timeout.

```toml
[[hooks.PreToolUse]]
matcher = "shell|write"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "make lint"
timeout = 30

[hooks]
intent_stop = true
```

## `[skills]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `skills.builtin` | boolean | `true` | `false` turns off all built-in skills. User directories stay active. |
| `skills.disabled` | array of strings | none | Built-in skill names to leave out. Empty or blank entries are refused. |
| `skills.extra_paths` | array of strings | none | More skill roots. `~` expands from home; relative paths resolve from the config file directory. A missing directory warns once. |

See `skills` for discovery order and trust.

```toml
[skills]
builtin = true
disabled = ["review"]
extra_paths = ["~/.claude/skills"]
```

## `[memory.injection]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `memory.injection.additions` | boolean | `true` | Append later turns' new memory hits as one system message. `false` leaves only the fixed block. |
| `memory.injection.addition_budget` | positive integer | `500` | The token cap on one turn's addition. Whole records only; a record that does not fit is skipped. |

The rest of the injection table (`memory.injection.enabled`, `memory.injection.max_tokens`, `memory.injection.global_budget`, `memory.injection.project_budget`, `memory.injection.overflow`) is recognized and interpreted as the documented default, and named on the startup line.
See `memory` for scopes and injection.

```toml
[memory.injection]
additions = true
addition_budget = 500
```

## `[control]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `control.enabled` | boolean | `true` | Every hosting process binds `$RADIAN_HOME/run/<pid>.sock`. `false` turns the port off for the process. A non-boolean is refused. |

## `[integrations]`

| Key | Type | Default | What it does |
|---|---|---|---|
| `integrations.herdr` | boolean | `true` | Report state to herdr. Takes effect only inside a herdr pane; `RADIAN_HERDR=0` turns it off for one run. |

## `[compaction]`

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `compaction.auto` | boolean | `true` | `true`, `false` | Turn automatic compaction off with `false`. `--no-auto-compact` turns the proactive path off; the overflow fallback still runs. |
| `compaction.thinking` | string | `low` | `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `current` | The summary-call reasoning level. `current` follows the conversation level. The host clamps it to the route and converts it to the wire value. Unknown levels are refused. |

```toml
[compaction]
auto = true
thinking = "low"
```

## `[tui]`

Display preferences never refuse the file: unknown keys and wrong types warn and fall back, except an over-wide `brand`. They apply at startup: change the file and start a new session. The theme also switches at once through `/theme`, which writes the choice back to the file.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `tui.theme` | string | `dark` | `dark`, `light`, `mono`, or a file under `~/.radian/themes/` | The startup theme. The reader accepts any string; the TUI resolves a built-in name else a theme file, and an unknown name warns and falls back to `dark`. `NO_COLOR` forces `mono` and wins over the file and `/theme`. |
| `tui.colors` | table | none | role names to a color literal, a `0`–`255` index, or a `{ fg, bg, bold, italic, dim, underline }` table | Per-role overrides applied after the theme. A bare string or a `0`–`255` integer sets the foreground; a table sets `fg`/`bg` (literal or `0`–`255`) plus the `bold`, `italic`, `dim`, `underline` forces. A malformed entry is skipped by name. A non-table value refuses the file. |
| `tui.keybindings` | table of arrays of strings | the built-in map | action names to key-string arrays | Custom keys applied over the built-in map. A non-table value refuses the file; a non-array entry is skipped by action and an unparsable key is ignored. A key bound to two actions refuses startup. |
| `tui.mouse` | boolean | `true` | `true`, `false` | Whether the terminal reports the mouse wheel. `false` sends no mouse sequence. A non-boolean warns and keeps `true`. |
| `tui.input_style` | string | `lines` | `lines`, `box` | The input frame. The removed `bar` warns and falls back to `lines`; any other unknown value warns and keeps `lines`. |
| `tui.clipboard` | string | `osc52` | `osc52`, `pbcopy`, `off` | How a copy reaches the system clipboard. `pbcopy` warns off macOS and uses `osc52`. `off` refuses a copy with a notice. At most 1 MiB per copy. An unknown value warns and keeps `osc52`. |
| `tui.bell` | string | `off` | `off`, `attention`, `all` | `attention` rings on questions and turn ends; `all` adds background job settles. At most one ring per event. An unknown value warns and keeps `off`. |
| `tui.tips` | boolean | `true` | `true`, `false` | One Tip line on the startup screen. A non-boolean warns and keeps `true`. |
| `tui.banner` | boolean | `true` | `true`, `false` | The startup wordmark. A non-boolean warns and keeps `true`. |
| `tui.brand` | string | per glyph set (`R` for `unicode`, the Nerd mark for `nerd`, `rad` for `ascii`, `⚡` for `emoji`) | any text at most 4 display columns wide | Brand text for the footer and the banner. Wider text refuses the file; a non-string warns and keeps the default. |
| `tui.spinner` | string | `pulse` | `pulse`, `arc`, `quadrant`, `braille` | The spinner shape. Unknown names warn and use `pulse`. |
| `tui.usage_period_secs` | integer | `60` | clamped to `15`–`600`; `0` disables | The usage refresh period. |
| `tui.display` | table | defaults below | the five keys below | The transcript display shapes. A bad entry warns and keeps the key default. |
| `tui.display.reasoning` | string | `collapsed` | `collapsed`, `expanded`, `hidden` | The reasoning block shape. |
| `tui.display.tool_output` | string | `errors` | `collapsed`, `expanded`, `errors` | The tool body shape. `errors` expands failures only. |
| `tui.display.tool_preview_lines` | positive integer | `5` | `1` and up | Body lines a collapsed tool card shows. A zero, negative, or non-integer value warns and keeps `5`. |
| `tui.display.mermaid` | boolean | `true` | `true`, `false` | Draw closed `mermaid` blocks as diagrams. A non-boolean warns and keeps `true`. |
| `tui.display.highlight` | boolean | `true` | `true`, `false` | Syntax-highlight fenced code blocks. A non-boolean warns and keeps `true`. |
| `tui.footer` | table | the `deck` preset rows | the keys below | The footer layout. A bad entry warns and keeps the default. |
| `tui.footer.preset` | string | `deck` | `deck`, `full`, `pi`, `classic` | The footer layout. An unknown value warns and uses `deck`. `classic` draws one status line and carries no rows. |
| `tui.footer.separator` | string | `" · "` (the `deck` preset value) | any text | Glyphs between items. A row may override it. |
| `tui.footer.separator_style` | string | the `footer_separator` theme role | a theme role or a color | The separator style. A bad value warns and keeps the role. |
| `tui.footer.glyphs` | string | `unicode` | `unicode`, `nerd`, `ascii`, `emoji` | The glyph set. An unknown value warns and uses `unicode`. |
| `tui.footer.context_line` | string | `off` | `off`, `percentage` | Fill the row gap with context usage. An unknown value warns and keeps `off`. |
| `tui.footer.transparent` | boolean | `false` | `true`, `false` | Inherit the terminal background in every footer cell. |
| `tui.footer.items` | table | none | per-id tables, each `{ icon` or `symbol`, `symbol_style`, `format`, `style`, `disabled`, `priority`, `thresholds`, `command`, `period`, item parameters `}` | Per-item defaults for a bare id. See `footer` for the item catalog, each item's parameters, and the preset contents. |
| `tui.footer.items.<id>` | table | — | `icon` or `symbol` (string), `symbol_style` (string), `format` (string), `style` (string), `disabled` (boolean, default `false`), `priority` (integer), `thresholds` (array of `{ at, style }`), `command` (string), `period` (integer seconds) | One item default: `format` holds the item's placeholders; `disabled = true` hides the item everywhere it is named; `thresholds` restyle a numeric item at each `at` value; `command` runs a contributor with `period` seconds between runs (`0` runs once at startup, omitted means every `60` seconds, other values clamp to `15`–`3600`; a non-number warns and uses `60`). A known option with the wrong type is skipped. A contributor id needs no built-in: until it publishes, the item hides. |
| `tui.footer.rows` | array of tables | the preset rows; a row's `position` defaults to `footer` | `left` (array), `right` (array), `separator` (string), `position` (`footer`, `above_input`, `below_input`) | Footer rows. An item is a bare id or a `{ item, ... }` inline table; a bare id takes the `[tui.footer.items.<id>]` defaults and an inline table is used as written. `position` defaults to `footer`; an unknown position warns and uses `footer`. When `rows` is empty the preset rows show with the per-id defaults on top. |
| `tui.widgets` | table | none | `above_input` and `below_input`, each an array of item ids | Deprecated contributor lines around the input box. A non-empty slot warns once and folds into a footer row at that position: move the ids into `[[tui.footer.rows]]` with `position`. A bad entry warns and is skipped. |
| `tui.widgets.above_input` | array of strings | none | item ids | Contributor items on a line above the input box. |
| `tui.widgets.below_input` | array of strings | none | item ids | Contributor items on a line below the input box. |

```toml
[tui]
theme = "dark"
input_style = "lines"

[tui.display]
reasoning = "collapsed"
tool_output = "errors"

[[tui.footer.rows]]
left = ["model", "context"]
right = ["usage"]
```

## `[mcp]`

Two spellings, same servers as `--mcp-config`: `mcp.servers` entries (`id`, `command`, `args`, `inherit_env`), or `mcp.mcpServers.<name>` tables (`command` plus `args` or a `url`).
Remote entries warn and never start.
`mcpServers` entries inherit `HOME` unless `inherit_env` says otherwise.
Both spellings together are an error; duplicate ids are refused.
The command line `--mcp-config` merges with this section; a shared id uses the command line entry with one warning.
See `mcp` for the server rules.

```toml
[[mcp.servers]]
id = "search"
command = "search-server"
args = ["--fast"]
inherit_env = ["HOME"]
```

## `[shell]`

This section has no live keys.
Every key, including the removed `shell.auto_background_secs`, warns once and is ignored.
Background runs start only with an explicit `background: true`.

## Secret references

The file never holds a secret value.
A sensitive key (`api_key`, `apikey`, `token`, `secret`, `password`, `passwd`, `authorization`, in any letter case) must be a reference table, and only `from = "env"` exists in this version.
`model.api_key` is refused outright: store the key with `radian login` instead.

A bare string on a sensitive key is refused.
A secret-looking string anywhere else is refused too: values starting with `sk-`, `sk_`, `xoxb-`, `ghp_`, `AKIA`, `Bearer `, or a 40-plus character run of letters, digits, `_`, `-`.
Every refusal names the key path, never the value.

Credentials live in `auth.json` instead.
A key value there may reference the environment: `$NAME` or `${NAME}` resolves from the entry `env` object first, then the process environment.
A missing variable is an error naming the provider and the name.
`!command` keys are refused and never run.
See `providers` for login and `environment` for the variable rules.

## Migration

`radian config import-pi --from <DIR>` drafts a file from a Pi agent directory: the draft on stdout, the mapping report on stderr.
`--write` saves to the configuration path and refuses an existing file.
Nothing is written without `--write`.
`radian config import-claude --claude-home <DIR> --project <DIR>` drafts from Claude Code files, including skill files and `AGENTS.md`.
Its `--write` saves every file and skips anything that exists, one report line each.
At least one of `--claude-home` and `--project` is required.
See `cli` for flags and exit codes, `pi-migration` for the Pi mapping table.

A fresh version mismatch behaves like this: an unknown top-level field or an unknown nested field is accepted and ignored — the write-back must preserve it or refuse, never drop it silently.
A newer `schema_version` reads read-only with a warning.
An older `schema_version` is refused; migration runs only through the explicit import commands above.

## Errors you will see

| Message | Meaning |
|---|---|
| `missing schema_version` | The file sets no `schema_version`. Add `schema_version = 1`. |
| `version too old` | The version is below `1`. Recreate the file or import it. |
| `unknown profile` / `unknown approval` | A `[permissions]` value this build does not know. Pick a listed value. |
| `api_key is removed` | `model.api_key` is set. Store the key with `radian login` instead. |
| `plaintext secret` / `secret lookalike` | A bare or secret-looking value sits in the file. Replace it with a reference. |
| `SecretRef ...` | A sensitive key is not `{ from = "env", name = "..." }`. Fix the shape. |
| `wrong type ... expected ...` | A key has the wrong TOML shape. The message names the expected shape. |
| `brand ... at most 4 display columns` | `tui.brand` is too wide. Shorten it. |
