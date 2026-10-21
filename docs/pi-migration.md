# Migrating from Pi

`radian config import-pi` drafts a Radian configuration from a Pi agent directory. It is a
**translation aid, not a compatibility layer**:

> Radian does not promise compatibility with Pi's file format. Pi may change its settings,
> models or keybindings at any time; the draft is a best-effort snapshot that you review and
> edit. Radian's own schema is the authority for what a Radian configuration means.

## The command

```sh
radian config import-pi --from <pi-agent-dir>            # draft on stdout, report on stderr
radian config import-pi --from <pi-agent-dir> > draft.toml
radian config import-pi --from <pi-agent-dir> --write    # save it, refusing to overwrite
```

- The Pi agent directory defaults to `~/.pi/agent` **for Pi**; for this command it is
  `--from`, which is required. There is no default, so the command never reads a directory you
  did not name.
- It is its own mode: no `--model`, no API key, no session file, no network.
- **Nothing is written without `--write`.** `--write` targets the file `--config` /
  `RADIAN_CONFIG` names, else `~/.radian/config/config.toml`, writes atomically (temporary file
  + rename) with mode `0600` (directory `0700`), validates the draft with the real reader
  before and after the write, and **refuses an existing file** — this version has no faithful
  write-back, so it never rewrites a configuration it did not create.

## What is read

| Pi file | What is used |
|---|---|
| `<dir>/settings.json` | `defaultProvider`, `defaultModel`, `theme`, `compaction.enabled` |
| `<dir>/models.json` | the active provider's `baseUrl`, `api`, `apiKey`, `headers`, `models[].id` |
| `<dir>/keybindings.json` | the actions listed in the mapping table below |

A file that is not there is skipped and named in the report. A file that does not parse is an
error naming the file and the position (exit 1). `mcp.json` is named in the report but not
migrated (Radian's configuration has no MCP section in this version; the hosts take MCP servers
from `--mcp-config`).

## Credentials are never migrated

**`auth.json` is never read** — not opened, not even `stat`ed, so a credential file that cannot
be read (mode `0000`) neither fails the migration nor leaks a byte. Saved credentials stay in
Pi; put the key in an environment variable instead.

| Pi value | What the draft gets |
|---|---|
| `"$NAME"` / `"${NAME}"` | `api_key = { from = "env", name = "NAME" }` |
| a **literal** value | `api_key = { from = "env", name = "<PROVIDER>_API_KEY" }` — the literal is **not** copied, and the report says a literal was skipped **without showing it** |
| `"!command"` | nothing; the report lists it and the command is **never executed** |

A `baseUrl` that carries credentials (`https://user:pass@host/…`) is a secret too, so it is not
migrated and the report says so. Header values follow the same rules: a literal, non-secret
value is carried over; environment interpolation, `!command` and secret-looking values are
skipped with a note (Radian's `model.headers` is a plain string map).

## Mapping table

| Pi | Radian | Notes |
|---|---|---|
| `settings.defaultProvider` | `model.provider` | only for providers Radian has an adapter for (`anthropic` → `anthropic`; OpenAI-compatible names → `openai-compatible`); anything else is reported |
| `models.providers.<p>.api` | `model.provider` | `openai-completions`/`openai-responses`/`azure-openai-responses` → `openai-compatible`; `anthropic` → `anthropic` |
| `models.providers.<p>.baseUrl` | `model.base_url` | skipped when the URL carries credentials |
| `settings.defaultModel` | `model.model` | else the first `models[].id` of the active provider |
| `models.providers.<p>.apiKey` | `model.api_key` | as an **environment reference** (see above) |
| `models.providers.<p>.headers` | `model.headers` | literal, non-secret values only |
| `settings.theme` | `tui.theme` | `dark`/`light`/`mono` only; Pi's `system` has no Radian equivalent |
| `settings.compaction.enabled` | `compaction.auto` | the token settings (`reserveTokens`, `keepRecentTokens`) have no Radian key |
| `keybindings.json` actions | `tui.keybindings` | see below |

Pi actions → Radian actions (only same-meaning pairs): `tui.input.submit` → `send`,
`tui.input.newLine` → `newline`, `tui.input.tab` → `complete`, `tui.editor.cursor*` →
`cursor_*`/`word_*`/`line_*`, `tui.editor.history*` → `history_*`, `tui.editor.delete*` →
`delete_*`, `tui.altScreen.page*`/`line*`/`top`/`bottom` → `scroll_*`, `tui.altScreen.search*` →
`find`/`find_next`/`find_prev`/`find_close`.

Pi writes `ctrl+shift+x`; Radian writes `ctrl-shift-x`, so keys are translated. A migrated key
that is already **another** Radian action's default is dropped with a note (the TUI refuses to
start when one key is bound to two actions), and so is a duplicate inside the draft. Review the
`tui.keybindings` block: the TUI's own conflict check is the authority.

## What is not migrated

Anything without a Radian equivalent is listed in the report, by key name and never by value:
thinking levels, tool selection (`defaultTools`), session directories, retry/transport/network
settings, resources (packages, extensions, skills, prompts, themes), the shell settings,
project trust, MCP servers, model overrides, and any setting this version does not know.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | the draft (or the write) succeeded |
| 1 | a missing directory, a malformed JSON file, a refused `--write` target, or an invalid draft |
| 2 | usage error (for example, no `--from`) |
