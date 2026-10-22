---
description: Add local model servers over standard input and output.
---

# MCP

MCP adds tools from local servers. Each server runs as a child process and
speaks over standard input and output. Radian never sandboxes the server
process itself: the startup notice says so. Tools from servers ask before
running under the `workspace` tier, like built-in tools that write. See
`permissions.md` for the tiers.

Each tool appears as `server__tool`: the server id, two underscores, the
tool name. Characters outside letters, digits, `_` and `-` become `_`. The
id is the alias prefix, so renaming a server renames its tools, and two
servers with one id fail before anything starts.

Server text results use the same 64 KiB cap as file reads. Long results are
cut with a marker that names the full size. The stored record holds the cut
text.

## The two config forms

Both forms describe the same servers, and both are accepted in every config
source. A source with both keys, or with neither, is an error.

Radian's own form names each server with an `id`:

```json
{
  "servers": [
    {
      "id": "notes",
      "command": "notes-mcp",
      "args": ["--stdio"],
      "env": {"NOTES_DIR": "/tmp/notes"},
      "inherit_env": ["HOME"],
      "shared_tools": ["search"],
      "call_timeout_secs": 60,
      "shutdown_timeout_secs": 5
    }
  ]
}
```

The shared form names each server with its object key, so other clients read
the same file:

```json
{
  "mcpServers": {
    "notes": {
      "command": "notes-mcp",
      "args": ["--stdio"],
      "env": {"NOTES_DIR": "/tmp/notes"}
    }
  }
}
```

Only stdio servers run. An entry with a `type` other than `stdio`, or with a
`url`, is skipped with one warning, and the session does not fail. Radian's
own keys (`inherit_env`, `shared_tools`, `call_timeout_secs`,
`shutdown_timeout_secs`) are accepted in both forms. Foreign keys
(`disabled`, `autoApprove`, `timeout`, and the like) are ignored with one
warning each: the shared form is a foreign format, not a typo. Radian's own
form rejects unknown fields, so a typo cannot silently drop a setting.

## Every server field

| Field | Forms | What it does |
|---|---|---|
| `id` | own | The server id. Required. It prefixes every tool alias (`notes__search`). |
| `command` | both | The program to run. Required for a stdio entry. |
| `args` | both | Arguments, default none. |
| `env` | both | Variables set for the server, default none. The host environment is not inherited: `env` plus `inherit_env` is all the server sees, except `PATH`, which is always copied through. |
| `inherit_env` | both | Host variable names copied through by name, for example `HOME`. Defaults to none in Radian's own form and to `["HOME"]` in the shared form. |
| `shared_tools` | both | MCP tool names (not aliases) that may run in parallel with other calls, default none. |
| `call_timeout_secs` | both | Limit for one `tools/call`, default 60. On expiry the call is cancelled. |
| `shutdown_timeout_secs` | both | How long closing waits for the server to exit after its stdin is closed, default 5. |

Timeouts that the file cannot set keep built-in defaults: 30 seconds to
spawn and list tools, 2 seconds of grace before a cancelled call kills the
server. Two more internals stay fixed: a server's `readOnlyHint: true`
annotation never makes a tool shareable on its own (only `shared_tools`
names do), and a server starts in the session's working directory (no field
sets another).

## Environment and secrets

`${VAR}` and `${VAR:-default}` in `command`, `args` and `env` values expand
at load time. An unset variable with no default expands to empty with one
warning. `${VAR:-default}` uses the default when the variable is unset or
empty. A set-but-empty `${VAR}` expands to empty with no warning.

The project `.mcp.json` additionally blanks eleven credential variables
wherever they are referenced, in `command`, `args`, `env` values and
`inherit_env`, with one warning per reference. The blanked names are the
customary provider keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`OPENCODE_API_KEY`, `OPENCODE_GO_API_KEY`, `DEEPSEEK_API_KEY`,
`GEMINI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`,
`XAI_API_KEY` and `MISTRAL_API_KEY`. A trusted project file still cannot
carry your shell keys into its servers. Radian's own credentials never live
in process environment, so they never pass through `inherit_env`.

## Where servers come from

Three sources load, in this priority order. A repeated run merges the flag
file with the config file: an id in both places uses the flag entry, and the
merge prints one warning.

1. `--mcp-config <file>`: one JSON file in either form.
2. The user config file: `[[mcp.servers]]` entries for Radian's own form, or
   `[mcp.mcpServers.<name>]` tables for the shared form. See
   `configuration.md` for the file path.
3. The project file `<workspace>/.mcp.json`, in either form. It loads only
   when the directory is trusted. Pass `--no-config-mcp` to skip it, or
   `--trust` to load it this run without trusting the directory.

The project file never breaks startup: a missing file loads nothing with no
lines, an unreadable file warns once, and an untrusted directory prints one
line saying nothing started.

## Approval under each tier

MCP tools follow the same permission rules as built-in tools. Under the
`workspace` tier a tool call asks first, and the approval box shows the full
`server__tool` name with its argument preview. Under the `full` tier nothing
asks. Deny rules still apply with auto-approve on. The server process itself
runs outside the sandbox under every tier: only its tool calls pass through
permissions, and the startup notice names the gap.

## Debugging a server that does not start

Type `/mcp` to open the MCP page. It lists the servers and their tools, or
says what failed. Work through this list:

1. Run the command by hand with the same `args` and `env`. A server that
   exits at once or never lists tools fails here too.
2. Check the form: both keys or neither key is an error, a duplicate id
   fails before anything starts, and an unknown field in Radian's own form
   is rejected. The message names the problem.
3. Check for a skip line: a `url` or a non-`stdio` type never starts, by
   design. Move it to an stdio command.
4. Check the timeouts: 30 seconds to spawn and list tools, 60 seconds per
   call. A slow starter needs a faster start, not a longer timeout.
5. Check trust: a project `.mcp.json` in an untrusted directory starts
   nothing. Trust the directory or pass `--trust`.
6. Check the environment: with no `inherit_env` the server sees almost
   nothing. Add the names it needs, and expand one `${VAR}` at a time to
   find the empty one (each warns once).
