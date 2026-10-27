---
description: Every radian command, subcommand, flag, and exit code.
---

# CLI

This page lists every command `radian` accepts.
The running binary is the source of truth: `radian <cmd> --help` prints the same facts.
`rad` is an alias of `radian`.

## `radian`: the full-screen interface

Run `radian` with no subcommand to open the full-screen interface.
Run it only in a real terminal: it takes over the screen.
`--model` is required unless the configuration file sets `model.model`.

### TUI options

| Flag | Value | Default | Overrides | Conflicts | What it does |
|---|---|---|---|---|---|
| `--config` | `FILE` | `RADIAN_CONFIG`, else `~/.radian/config/config.toml` | `RADIAN_CONFIG` | — | Read this configuration file. Radian never writes it. |
| `-w`, `--workspace` | `DIR` | current directory | — | — | Root the built-in file tools here. Without it the current directory is the workspace. |
| `--system` | `SYSTEM` | — | — | — | System prompt text. |
| `-h`, `--help` | — | — | — | — | Print help and exit. |
| `-V`, `--version` | — | — | — | — | Print the version and exit. |

### Model options

| Flag | Value | Default | What it does |
|---|---|---|---|
| `-m`, `--model` | `MODEL` | `model.model` from the file | The model name sent to the API. A bare name, never `provider/model`. In the full-screen interface `/model` takes `<provider>/<model>`. |
| `--provider` | `PROVIDER` | `openai-compatible` | A built-in provider id (`opencode-go`, `chatgpt`, `anthropic`, `openai`, `openai-compatible`) or a `[providers.<name>]` name. An unknown provider is refused. |
| `--base-url` | `URL` | provider public endpoint | Everything before the provider path (`/chat/completions`, `/v1/messages`). Only for `openai-compatible`; refused for other providers. |
| `--max-tokens` | `N` | `16384` | `max_tokens` for anthropic; the API requires it. Refused for `openai-compatible`. |
| `--thinking` | `MODE` | `medium` | Thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. Clamped to what the route supports. Legacy `adaptive` and `enabled:<budget>` warn and still work. |
| `--header` | `NAME=VALUE` | — | One extra request header. Repeat the flag for more headers. `Authorization` is refused. Only for `openai-compatible`; refused for other providers. `{session_id}` in a value expands to the session id. A flag of the same name wins over the file. |
| `--max-model-calls` | `N` | `1000` | Per-turn model call fuse. A loop of small calls is not stopped by the context budget. |
| `--context-window` | `TOKENS` | catalog lookup | Explicit context window in tokens. Wins over the models.dev catalog. With neither, the budget guard is off. |
| `--offline-catalog` | — | fetch | Use only the local catalog cache, never the network. |
| `--catalog-provider` | `PROVIDER` | derived from `--base-url` host | The models.dev provider id to look the model up under. |
| `--no-auto-compact` | — | compact | Turn automatic compaction off. Over budget the request goes out unchanged. |

### Session options

| Flag | Value | Default | Overrides | Conflicts | What it does |
|---|---|---|---|---|---|
| `--session-dir` | `DIR` | `RADIAN_SESSION_DIR`, else `~/.radian/sessions` | `RADIAN_SESSION_DIR` | — | Where session files live. |
| `--session-id` | `VALUE` | generated per process | — | `--resume` | The session file name for this run. |
| `-r`, `--resume` | optional `SESSION_ID` | — | — | `--session-id`, `--list` | Resume a session by id. A bare `--resume` opens the picker. |
| `-l`, `--list` | — | — | — | `--resume` | Print this workspace sessions and exit. |
| `--pick` | — | — | — | `--resume`, `--session-id` | Pick a session in a full-screen list before opening one. Creates no file. |

### MCP options

| Flag | Value | What it does |
|---|---|---|
| `--mcp-config` | `FILE` | Start MCP stdio servers from this JSON file (`{"servers": [...]}`). Under `workspace` their tools ask first. The servers themselves are never sandboxed. |

### Permission options

| Flag | Value | Default | Conflicts | What it does |
|---|---|---|---|---|
| `--profile` | `TIER` | `full` | — | `workspace` (sandboxed) or `full` (unsandboxed). The aliases `workspace-write` and `full-access` are accepted but hidden from `--help`. A project file can only narrow it. |
| `--approval` | `POLICY` | `on-ask` | — | `on-ask`, `always`, `never`, or `auto`. Falls back to the file, then the session state, then `on-ask`. `auto` in the file has no effect under `full`. |
| `-y`, `--yes` | — | ask | — | Approve every tool question without asking. Deny rules still deny. The sandbox still applies. |
| `--allow-shell-command` | `PREFIX` | — | — | Allow one simple shell prefix without asking. Repeat the flag. A compound command needs every segment allowed. |
| `--trust` | — | — | `--no-trust` | Trust this workspace project resources for this process only. Never written to disk. |
| `--no-trust` | — | — | — | Distrust this workspace project resources for this process only. |

Example:

```sh
radian --provider openai-compatible --model gpt-5 --profile workspace -w "$PWD" --session-id demo
```

## `radian exec`: the headless command

`radian exec` runs one turn or a line session without a terminal.
With `-p` it prints only the model text to stdout and exits; status goes to stderr.
Without `-p` it runs a line prompt loop.
A named-provider probe's one notice line goes to stderr after the turn, under `--output jsonl` too, like a key-switch notice.
The print-and-exit modes (`--list`, `--export`, `--search`, `--fork`) need no model and read no key.

| Flag | Value | Default | Conflicts | What it does |
|---|---|---|---|---|
| `--model` | `MODEL` | file `model.model` | — | Bare model name. Required for a conversation unless the file sets it. In the full-screen interface `/model` takes `<provider>/<model>`. |
| `--provider` | `PROVIDER` | `openai-compatible` | — | A built-in provider id (`opencode-go`, `chatgpt`, `anthropic`, `openai`, `openai-compatible`) or a `[providers.<name>]` name. An unknown provider is refused. |
| `--base-url` | `URL` | provider endpoint | — | Everything before the provider path. Only for `openai-compatible`; refused for other providers. |
| `--config` | `FILE` | `RADIAN_CONFIG`, else default file | — | Read this configuration file. Radian never writes it. |
| `--max-tokens` | `N` | `16384` | — | `max_tokens` for anthropic. Refused for `openai-compatible`. |
| `--thinking` | `MODE` | `medium` | — | Thinking level: `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`, clamped per route. Legacy `adaptive` and `enabled:<budget>` warn and still work. |
| `--system` | `SYSTEM` | — | `--system-file` | System prompt text. |
| `--system-file` | `FILE` | — | — | Read the system prompt from this file. |
| `--mcp-config` | `FILE` | — | — | MCP servers JSON file. |
| `--workspace` | `DIR` | current directory | — | Root the built-in file tools here. |
| `--profile` | `workspace` \| `full` | `full` | — | Permission tier. The aliases `workspace-write` and `full-access` are accepted but hidden from `--help`. A project file can only narrow it. |
| `--allow-shell-command` | `PREFIX` | — | — | Allow one simple shell prefix without asking. Repeatable. |
| `--approval` | `POLICY` | `on-ask` | — | `on-ask`, `always`, `never`, or `auto`. The file is the fallback; `auto` in the file has no effect under `full`. |
| `--yes` | — | ask | — | Shorthand for `--approval auto`. |
| `-p`, `--prompt` | `PROMPT` | — | `--list` | One-shot mode: run this prompt, then exit. `-p -` reads stdin to EOF. |
| `--image` | `PATH` | — | — | Attach an image to the `-p` prompt's user message. Repeatable; the images attach in the order given. Needs `-p`. The path is read by Radian, not the sandbox. PNG, JPEG, GIF or WebP only, at most 5 MiB and 8000 px per side. A user-message image is sent by every provider route. A missing file, another format, or an over-limit image is a usage error: exit `2`, naming the path and the reason. |
| `--output` | `jsonl` | model text | — | Print every journal line, live deltas, and a closing `Result` record as JSON lines. |
| `--header` | `NAME=VALUE` | — | — | Extra request header. Repeatable. `Authorization` is refused. Only for `openai-compatible`; refused for other providers. `{session_id}` expands. A flag of the same name wins over the file. |
| `--session-id` | `VALUE` | generated | `--resume`, `--list`, `--search` | The session file name for this process. |
| `--resume` | `SESSION_ID` | — | `--list` | Resume a session by id. |
| `--list` | — | — | — | Print this workspace sessions and exit. |
| `--export` | `SESSION_ID` | — | `--prompt`, `--resume`, `--list`, `--search` | Export one session of this workspace and exit. |
| `--search` | `TEXT` | — | `--prompt`, `--resume`, `--list`, `--export` | Search this workspace sessions and exit. |
| `--fork` | `SESSION_ID` | — | `--prompt`, `--resume`, `--list`, `--export`, `--search` | Fork a session at a turn boundary. Needs no model and reads no key. |
| `--at` | `SEQ` | last boundary | — | Fork at this parent sequence number. Needs `--fork`. |
| `--format` | `markdown` \| `json` | `markdown` | — | Export shape. Needs `--export`. |
| `--out` | `FILE` | stdout | — | Write the export here. Refuses to overwrite unless `--force`. Needs `--export`. |
| `--force` | — | — | — | Overwrite `--out`. Needs `--out`. |
| `--include-reasoning` | — | off | — | Include assistant reasoning in the export. Needs `--export`. |
| `--include-provider-payload` | — | off | — | Include raw provider payloads in the export. Needs `--export`. |
| `--session-dir` | `DIR` | `RADIAN_SESSION_DIR`, else default | — | Where session files live. |
| `--include-usage` | optional `true` \| `false` | per route; see [providers](providers.md) | — | Ask the server to include usage in the stream. |
| `--max-model-calls` | `N` | `1000` | — | Per-turn model call fuse. |
| `--turn-timeout-secs` | `N` | — | — | Per-turn time limit in seconds. |
| `--context-window` | `TOKENS` | catalog lookup | — | Explicit context window. Wins over the catalog. |
| `--offline-catalog` | — | fetch | — | Use only the local catalog cache. |
| `--catalog-provider` | `PROVIDER` | derived from host | — | The models.dev provider id for the lookup. |
| `--no-auto-compact` | — | compact | — | Turn automatic compaction off. |
| `--trust` | — | — | `--no-trust` | Trust project resources for this process only. |
| `--no-trust` | — | — | — | Distrust project resources for this process only. |

Examples:

```sh
radian exec --provider openai-compatible --model gpt-5 -p "summarize Cargo.toml"
radian exec --resume demo -p "now read docs/"
radian exec --list
radian exec --export demo --format json --out demo.json
radian exec --search "bubblewrap"
radian exec --fork demo --at 12
```

### The `--output jsonl` `Result` record

`--output jsonl` prints a closing `Result` object as the last stdout line.
When the run ended because a turn failed on a model error, `Result` carries a `model_error` object with no message text (the message stays on stderr, and the exit code does not change):

| Member | Type | Meaning |
|---|---|---|
| `kind` | string | One value per model-error variant: `transport`, `protocol`, `unavailable`, `usage_limit`, `context_overflow`, `empty_response`. |
| `status` | number or null | The HTTP status when the error carries one (an `unavailable` error), else `null`. |
| `retry_after_secs` | number or null | The provider's `Retry-After` (`unavailable`) or the plan window's reset wait (`usage_limit`) in whole seconds, rounded up, else `null`. |

`usage_limit` covers a spent ChatGPT plan window and opencode-go's Go plan limit (an HTTP 429 whose body has `error.type` `GoUsageLimitError`). For opencode-go the message on stderr is the body's `error.message` plus the limit's window name when the body carries one, for example `Go usage limit exceeded (monthly)`; it never includes the workspace id. `status` stays `null` for both, and `retry_after_secs` is the reset wait only when the provider sent one (a `Retry-After` header, or the body's own countdown).

The field is absent on every other run end: completed, cancelled, budget, a hook error, or a tool failure.
It is never `null`.

```json
{"model_error": {"kind": "usage_limit", "status": null, "retry_after_secs": 6240}}
```

## `radian acp`

`radian acp` serves the same session over the Agent Client Protocol on stdio.
It accepts the conversation flags above with the same names and meanings, without `-p`, `--output`, `--yes`, and the print-and-exit modes.
The one-session rule, the protocol methods, the `_meta.radian.*` fields, the permission and `ask_user` mapping, the stdio-only `mcpServers`, and the non-interactive trust gate are on [`acp`](acp.md).

## `radian arc`

`radian arc` is the plan and task orchestrator.
A bare `radian arc` opens its TUI (plans, board, task and merge pages, with the owner's actions); a subcommand runs one CLI operation.
The subcommands are `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`, `start`, `cancel`, `reset-budget`, and `mcp`; run `radian arc --help`.
arc's exit codes are its own: `3` means the state does not allow the operation and `4`
means a lock is held.
See [`arc`](arc/index.md) for every subcommand, flag, and exit code, [`arc/loop`](arc/loop.md) for the scheduling loop and `loop.log`, [`arc/gate`](arc/gate.md) for the gate and its failures, [`arc/mcp`](arc/mcp.md) for the lead's MCP server, [`arc/files`](arc/files.md) for every file field and configuration key, and [`arc/tui`](arc/tui.md) for the TUI and its owner's actions.

```sh
radian arc init
radian arc add plan --slug first --title "First plan" < request.txt
radian arc list --json
radian arc confirm PL0001-first
radian arc auto PL0001-first run
radian arc start TK0001-first-work
radian arc cancel TK0001-first-work
radian arc reset-budget TK0001-first-work
```

## `radian login` and `radian logout`

`radian login [PROVIDER]` stores an API key in `~/.radian/credentials/auth.json`.
`PROVIDER` is a built-in id (`opencode-go`, `anthropic`, `openai`, `openai-compatible`), `chatgpt` (browser OAuth), `chatgpt-device` (device code), or `custom`, which names a new endpoint: name, protocol, base URL, and key. The custom protocol is `openai-compatible`, `anthropic` or `openai-responses` (pi's `openai-completions` and `anthropic-messages` are accepted). An unknown provider is refused.
A bare `radian login` shows the numbered provider list.
The key never echoes.
A verification request (`GET <base>/models`) runs before the save.
A failure shows and still offers to save.
When the provider already has a key, it asks `replace the current key, or add another? [r/a]`.
`a` then asks a name for the new key; empty is allowed and the key then shows masked.
Without a terminal the command refuses and points at `auth.json` instead.

```sh
radian login
radian login anthropic
```

`radian logout <provider>` removes one stored credential.
It asks first.
With several keys it lists them (`1 first (sk-…irst)  2 team (sk-…cond)`) and asks for a number, a name, or `all`.
`radian logout <provider> <name>` removes that key without asking; an unknown name is refused naming the known ones.
Removing the first key promotes the next one to `key`; removing the last key removes the entry.
Every other entry stays untouched.
Exit `0` on removal or on an explicit keep, `2` when the entry is missing or the run has no terminal.

```sh
radian logout anthropic
radian logout opencode-go team
```

## `radian memory`

`radian memory` runs the same grammar as the `/memory` slash command.
Needs no model, no key, no session file, no network.
`--workspace <DIR>` resolves the project against another directory.

| Command | Arguments and flags | What it does |
|---|---|---|
| `list` | `--global`, `--project`, `--all`, `--json` | List records, project first then global. `--all` includes archived records. |
| `search` | `--global`, `--project`, `--json`, `WORD...` (required) | Search by keyword. Every word must match. |
| `show` | `--global`, `--project`, `--json`, `ID` | Show one record by id. |
| `add` | `--global`, `--kind preference\|decision\|fact\|lesson` (default `fact`), `--key KEY`, `CONTENT` (`-` reads stdin) | Add a project record, or a global record with `--global`. A git workspace registers itself on first write. |
| `edit` | `--global`, `--revision REV` (required), `ID`, `CONTENT` | Change a record. Needs the current revision. |
| `archive` | `--global`, `--revision REV` (required), `ID` | Archive a record. It stays stored but leaves injection and default search. |
| `restore` | `--global`, `--revision REV` (required), `ID` | Restore an archived record. |
| `forget` | `--global`, `--yes`, `ID` | Forget a record, after confirmation. The bytes stay until `purge`. |
| `purge` | `--global`, `--project`, `--yes` | Rewrite the log and remove forgotten content, after confirmation. |
| `confirm` | `--global`, `--revision REV` (required), `ID` | Confirm a record. Keeps its original source. |
| `export` | `--global`, `--project` | Print one scope as editable Markdown. |
| `import` | `--global`, `--project`, `SOURCE` (`-` reads stdin) | Import an edited Markdown export. |
| `project` | `--json`, `project new` | Show project identity and state, or register this workspace as a project. |
| `relink` | `PROJECT_ID` | Attach this workspace identity key to an existing project. |
| `split` | `--yes`, `FROM`, `TO`, `ID...` | Move records between projects, after confirmation. |
| `merge` | `--yes`, `INTO`, `FROM` | Merge one project into another, after confirmation. Reports key conflicts. |

`--global` and `--project` conflict except where both are absent (the default scope).
`list`, `search`, `show`, and `project` accept `--json` for machine output.
`forget`, `purge`, `split`, and `merge` ask for confirmation unless `--yes` is passed.
Exit `0` on success, `1` for a conflict or a missing record, `2` for bad use.
See `memory` for scopes, revisions, and confirmation rules.

```sh
radian memory add "prefers tabs" --kind preference --key style
radian memory search tabs --json
radian memory list --all
```

## `radian trust`

`radian trust` shows or changes a directory's project trust decision.
Needs no model, no key, no session file, no network.
`--workspace <DIR>` picks the directory the decision is about; omitted, the current
directory. Only `list` ignores it: it prints every saved decision.

| Command | Arguments and flags | What it does |
|---|---|---|
| `status` | — | Show the current directory's trust decision and its source: `trust on (saved for <dir>)`, `trust off (inherited from <dir>)`, or `trust: no saved decision (untrusted)`. A pure read: nothing is written. |
| `on` | — | Trust the current directory, saving `true`. Disk state only: the notice says it takes effect at the next start. |
| `off` | — | Distrust the current directory, saving `false`. Disk state only: like `on`, it takes effect at the next start. |
| `forget` | — | Remove the current directory's saved decision. An ancestor's decision then applies, or none at all. With no saved decision, it prints `no saved trust decision for <dir>` and writes nothing. |
| `list` | — | Print every saved decision, one `<dir>: on|off` line per directory, sorted. With none saved, it prints `no saved trust decisions`. |

`status`, `on`, `off` and `forget` print exactly one line to stdout.
A closed output pipe (`radian trust status | head`) exits `0` with no panic and no error
line: the reader going away is a normal stop.
Exit `0` on success, `1` when the store cannot be read or written, `2` for bad use.
The TUI's `/trust` runs the same record and prints the same lines; see `commands.md`.

```sh
radian trust status
radian trust on
radian trust list
```

## `radian config`

`radian config import-pi --from <DIR>` drafts a configuration from a Pi agent directory.
The draft goes to stdout and the mapping report to stderr.
`--write` saves the draft to the configuration path (`--config`, else `RADIAN_CONFIG`, else the default file) and refuses an existing file.
Nothing is written without `--write`.
The command reads only the directory you name: never `auth.json`, never a secret value, never a `!command`.
Exit `0` for a draft, `1` for a missing directory, a malformed file, a refused write, or an invalid draft, `2` without `--from`.

```sh
radian config import-pi --from ~/.pi/agent > draft.toml
radian config import-pi --from ~/.pi/agent --write
```

`radian config import-claude --claude-home <DIR> --project <DIR>` drafts a configuration from Claude Code files.
At least one of `--claude-home` and `--project` is required.
Stdout carries the draft: the config, then each skill file and `AGENTS.md` body under a header line.
`--write` saves every file instead; nothing that exists is ever changed.
Exit `0` for a draft, `2` when neither source is given, `1` for other failures.
See `configuration` for the file format and `pi-migration` for the Pi mapping table.

```sh
radian config import-claude --claude-home ~/.claude > draft.toml
radian config import-claude --project "$PWD" --write
```

## `radian theme import`

`radian theme import <PI-THEME.json> [--name <NAME>] [--force]` converts a Pi theme JSON file to a Radian TOML theme under `~/.radian/themes/`.
The name defaults to the file `name` field, else the file stem.
`--force` overwrites an existing file.
The command prints the written path and one report line per mapped value.
Exit `0` on success, `1` for a missing or bad input, or an existing output without `--force`.
See `themes` for names, fallback, and overrides.

```sh
radian theme import night.json --name night
```

## `radian footer export`

`radian footer export <PRESET>` prints one built-in preset TOML: `deck`, `full`, `pi`, or `classic`.
Use it as the `[tui.footer]` section to start a customization.
An unknown preset exits `2` and names the four.
See `configuration` for the footer keys.

```sh
radian footer export pi
```

## `radian docs`

`radian docs` prints one line per manual topic with its description.
`radian docs <topic>` prints that page verbatim, the same text the agent reads.
The topic is a page stem (`commands`), a path under the manual (`arc/loop`), or a file name (`commands.md`, `arc/loop.md`).
A topic that names a directory (`arc`) opens its `index.md`.
An unknown topic exits `2` and lists the pages on stderr.

```sh
radian docs
radian docs cli
radian docs arc/loop
```

## `radian attach`

`radian attach [SESSION_ID] [--follow] [--session-dir <DIR>]` views a session read-only.
No input reaches the session and nothing is written.
A bare `radian attach` opens the workspace session picker.
`--follow` keeps reading new committed events until `q`, Esc, or Ctrl-C.
Exit `0` for the view, `2` for an unknown id or no terminal, `1` when rendering fails.
See `sessions` for ids, resume, and export.

```sh
radian attach demo --follow
```

## `radian serve` and `radian mcp`

Both names are placeholders in this build.
Each exits `2` without serving anything.

## Exit codes

Both hosts share these codes.
`radian exec --list | head` and every print-and-exit mode still exit `0` on a broken pipe.
A closed output pipe ends any print-and-exit command of either host with exit `0`: the
reader going away is a normal stop, so the command stops writing quietly, without a
panic and without an error line.

| Code | Meaning |
|---|---|
| `0` | Success: the turn completed, the view printed, the draft saved. |
| `1` | The run failed: a model or turn error, a failed sandbox probe target, a missing record, a refused write. |
| `2` | Bad use or refused setup: clap usage errors, a bad configuration, an unknown keybinding, a startup refusal, a reserved subcommand, an unknown `docs` topic or footer preset, a bad `memory` invocation. |
| `3` | The budget ran out: `BudgetExceeded` or a exceeded deadline. |
| `4` | The answer is unfinished: `Incomplete` (a response truncated at the output token limit after two truncated responses in a row, a content-filtered answer, an unknown stop reason) or `Refused`. |
| `101` | The app panicked. A crash log lands under `~/.radian/cache/diagnostics`. |
| `130` | Requested cancellation, or a forced exit after a second Ctrl-C. |
| `143` | SIGTERM (`128 + 15`). A signal-driven exit otherwise follows the shell rule `128 + signal`. |
