---
description: Which extension mechanism fits which need, where each lives, and a first example.
---

# Extensions

Radian has seven extension mechanisms. Each one fits a different need. Pick
from the need, not from the name.

| Need | Mechanism | Detail page |
|---|---|---|
| Always-on project directions the model must follow | Context files (`AGENTS.md`) | `context-files.md` |
| A reusable procedure the model can run on demand | Skill | `skills.md` |
| Facts the model should remember across sessions | Memory | `memory.md` |
| An extra tool (a local server over stdio) | MCP server | `mcp.md` |
| Run your own command when a tool runs or the model stops | Hook | `hooks.md` |
| Change the colors | Theme | `themes.md` |
| Change the status line | Footer | `footer.md` |

No mechanism blocks startup. A bad file warns once on the startup line and
the session opens without it.

## Where things live

Three layers hold configuration. The user layer is `~/.radian/`: the config
file, the `themes/` directory, the `skills/` directory, the memory store.
The project layer is the workspace: `.agents/skills/`, `.mcp.json`,
`.radian/hooks.json`, and the context files. The built-in layer is compiled
in: three skills and three themes. A user skill of the same name wins over a
built-in one. A project skill loads only when the project is trusted.

The startup line names what loaded and warns about what did not. Trust is
per directory: an untrusted project contributes no servers, no hooks, and no
project skills, and the startup line says so. See `sessions.md` for the trust
prompt.

## Context files in one example

Create `AGENTS.md` in the workspace root:

```md
# Build

Run `make check` before you finish a change.
```

Open radian in that workspace. The startup line names the file and its size.
Every model request in the session carries the file's text. No command runs
it; it is always present. Use context files for directions, never for secrets.

## Skill in one example

Create `.agents/skills/review/SKILL.md` in a trusted project:

```md
---
name: review
description: Review the current diff for style and tests.
---

Read the current diff. List style problems and missing tests.
```

Run `/skills` to see the row, then `/skill:review` to run it. The body plus
your arguments become one user message. Use skills for procedures the model
follows on demand, not for always-on directions.

## Memory in one example

Type `/memory add The deploy needs the vpn profile "office".` The record
persists across sessions. Later turns find it with `/memory search vpn`
or through the `memory_search` tool. Use memory for facts, use context files
for directions.

## MCP server in one example

Write `mcp.json` with one stdio server:

```json
{
  "servers": [
    {"id": "notes", "command": "notes-mcp", "args": ["--stdio"]}
  ]
}
```

Start with `radian --mcp-config mcp.json`. Type `/mcp` to see the server and
its tools. A tool call asks first under the `workspace` tier. Use MCP for
capabilities outside the built-in tools.

## Hook in one example

Add one entry to the user config file (`configuration.md` holds the path):

```toml
[[hooks.PreToolUse]]
matcher = "shell"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo tool call starting >> /tmp/radian-hooks.log"
```

Every `shell` call now runs the command first. The hook's stdin is JSON with
the event name and the tool input. Use hooks to observe or gate tool calls
with your own programs.

## Theme in one example

Type `/theme light` to switch now; radian writes the name back to the config
file. To tune one color without switching files, set it in the config:

```toml
[tui.colors]
user = "#0af"
```

Run `radian theme import palette.json --name mine` to convert a pi theme to
a Radian file under `~/.radian/themes/`, then `/theme mine`. Use themes for
colors only; they never change behavior.

## Footer in one example

Run `radian footer export deck` to print the default status line as TOML.
Paste it under `[tui.footer]` in the config file and edit the rows. Add an
item to a row's `left` or `right` list to show it. Use the footer to watch
model, cost, context and jobs, not to add behavior.

## Load order

At session start radian reads, in this order: the config file, the theme,
the keymap, the context files (global first, then outer directories to
inner), the skills (built-in last, so yours win), the MCP servers (project
only when trusted), the hooks (project first, then user), and the memory
store. Each step warns and continues on failure. The startup screen shows
the result: theme, context files with sizes, skill count, MCP servers and
tools, hook counts, and the trust state.
